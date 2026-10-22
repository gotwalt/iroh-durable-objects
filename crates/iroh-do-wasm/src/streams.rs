//! `Connection`, `SendStream` and `RecvStream` with the names and signatures of
//! `@number0/iroh` 1.1.0, bytes as `Uint8Array`. A `BiStream` is a plain object with `send`
//! and `recv`.
//!
//! The wrappers report to the node's idle close themselves, so apps need no activity API:
//! every read and write records activity, and each stream half counts as open (a
//! [`StreamGuard`]) until it reaches a terminal outcome. For a send half that is `finish`,
//! `reset`, a failed write, or `stopped()` resolving; for a receive half, the end of the
//! stream, a failed read (the peer reset it, the connection closed, ...), or `stop`. While a
//! stream is open the node closes the connection only after `t_silent` without data;
//! otherwise after `t_idle` (`iroh_do_core::conn`).
//!
//! A half that JavaScript abandons without finishing stops counting when its connection
//! closes (`iroh_do_core::conn`), not when the garbage collector frees the wrapper, which
//! happens at an unspecified time through wasm-bindgen's `FinalizationRegistry`. An accepted
//! connection closes when its handler's promise settles (`app.rs`).
//!
//! # Concurrent calls
//!
//! Each half keeps its iroh stream in a `RefCell` that is borrowed only inside a single
//! `poll`, never across an `.await`, and an async lock that orders the calls that wait for
//! data or flow control (`read*`, `write*`, `finish`, `receivedReset`), as `@number0/iroh`
//! orders them. `stop` and `reset` do not take that lock: they act at once, and a read or
//! write pending on the same half fails with `ClosedStream`. The operations polled here
//! (`read_chunk`, `write`, `received_reset`) are cancel-safe in noq, so polling a fresh
//! future each time loses nothing.

use std::{
    cell::{Cell, RefCell},
    future::{Future, poll_fn},
    pin::pin,
    rc::Rc,
    sync::Arc,
    task::{Poll, Waker},
};

use iroh::endpoint::{self, Side, VarInt};
use iroh_do_core::{Activity, StreamGuard};
use js_sys::{Object, Promise, Reflect};
use tokio::sync::Mutex;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

use crate::{ids::EndpointId, named_err};

type Act = Option<Arc<Activity>>;

fn touch(a: &Act) {
    if let Some(a) = a {
        a.touch();
    }
}

fn var_int(code: u64) -> Result<VarInt, JsValue> {
    VarInt::from_u64(code).map_err(|e| named_err("RangeError", e))
}

fn closed_stream() -> JsValue {
    named_err("ClosedStream", "closed stream")
}

/// One open stream half, counted until [`Half::done`] or drop.
struct Half(RefCell<Option<StreamGuard>>);

impl Half {
    fn new(a: &Act) -> Self {
        Half(RefCell::new(a.as_ref().map(|a| a.stream())))
    }

    fn done(&self) {
        self.0.borrow_mut().take();
    }
}

/// The state of one stream half, shared by the clones of its JavaScript wrapper.
struct Shared<S> {
    /// Borrowed only inside a poll or a synchronous call, never across an `.await`.
    stream: RefCell<S>,
    /// Orders the calls that wait for I/O.
    io: Mutex<()>,
    /// Waker of the I/O call in progress, for `stop` and `reset`.
    waker: RefCell<Option<Waker>>,
    /// Set by `stop` or `reset`: a pending or later read or write fails.
    abandoned: Cell<bool>,
    open: Half,
    activity: Act,
}

impl<S> Shared<S> {
    fn new(stream: S, activity: &Act) -> Rc<Self> {
        Rc::new(Shared {
            stream: RefCell::new(stream),
            io: Mutex::new(()),
            waker: RefCell::new(None),
            abandoned: Cell::new(false),
            open: Half::new(activity),
            activity: activity.clone(),
        })
    }

    /// Polls `op` on the stream until it is ready, unless the half is abandoned first.
    async fn poll_op<T, E>(
        &self,
        mut op: impl FnMut(&mut S, &mut std::task::Context<'_>) -> Poll<Result<T, E>>,
    ) -> Result<T, Option<E>> {
        let out = poll_fn(|cx| {
            if self.abandoned.get() {
                return Poll::Ready(Err(None));
            }
            *self.waker.borrow_mut() = Some(cx.waker().clone());
            let mut stream = self.stream.borrow_mut();
            op(&mut *stream, cx).map_err(Some)
        })
        .await;
        self.waker.borrow_mut().take();
        out
    }

    /// Marks the half abandoned and wakes the I/O call in progress, which then fails.
    fn abandon(&self) {
        self.abandoned.set(true);
        self.open.done();
        if let Some(w) = self.waker.borrow_mut().take() {
            w.wake();
        }
    }

    fn borrow_mut(&self) -> Result<std::cell::RefMut<'_, S>, JsValue> {
        self.stream
            .try_borrow_mut()
            .map_err(|_| named_err("Error", "stream busy"))
    }
}

/// An active QUIC connection.
#[wasm_bindgen]
pub struct Connection {
    inner: endpoint::Connection,
    activity: Act,
}

impl Connection {
    pub(crate) fn new(inner: endpoint::Connection, activity: Act) -> Self {
        Self { inner, activity }
    }

    fn send(&self, s: endpoint::SendStream) -> SendStream {
        SendStream(Shared::new(s, &self.activity))
    }

    fn recv(&self, r: endpoint::RecvStream) -> RecvStream {
        RecvStream(Shared::new(r, &self.activity))
    }

    fn bi(&self, s: endpoint::SendStream, r: endpoint::RecvStream) -> JsValue {
        let out = Object::new();
        let _ = Reflect::set(&out, &JsValue::from_str("send"), &self.send(s).into());
        let _ = Reflect::set(&out, &JsValue::from_str("recv"), &self.recv(r).into());
        // Fixed properties: `bi.send === bi.send`, unlike a getter returning new wrappers.
        Object::freeze(&out);
        out.into()
    }

    fn clone_handle(&self) -> Connection {
        Connection {
            inner: self.inner.clone(),
            activity: self.activity.clone(),
        }
    }
}

#[wasm_bindgen(typescript_custom_section)]
const BI_TS: &str = r#"
/** A bidirectional stream: its two halves. */
export interface BiStream {
  readonly send: SendStream;
  readonly recv: RecvStream;
}
"#;

#[wasm_bindgen]
impl Connection {
    /// The negotiated ALPN.
    pub fn alpn(&self) -> Vec<u8> {
        self.inner.alpn().to_vec()
    }

    /// The peer's EndpointId.
    #[wasm_bindgen(js_name = remoteId)]
    pub fn remote_id(&self) -> EndpointId {
        EndpointId(self.inner.remote_id())
    }

    /// `Client` for a connection this endpoint dialed, `Server` for one it accepted.
    #[wasm_bindgen(unchecked_return_type = "\"Client\" | \"Server\"")]
    pub fn side(&self) -> String {
        match self.inner.side() {
            Side::Client => "Client",
            Side::Server => "Server",
        }
        .to_owned()
    }

    /// An id unique among this endpoint's live connections.
    #[wasm_bindgen(js_name = stableId)]
    pub fn stable_id(&self) -> usize {
        self.inner.stable_id()
    }

    /// Waits for the peer to open a bidirectional stream.
    #[wasm_bindgen(js_name = acceptBi, unchecked_return_type = "Promise<BiStream>")]
    pub fn accept_bi(&self) -> Promise {
        let conn = self.clone_handle();
        future_to_promise(async move {
            let (s, r) = conn
                .inner
                .accept_bi()
                .await
                .map_err(|e| named_err("ConnectionError", e))?;
            touch(&conn.activity);
            Ok(conn.bi(s, r))
        })
    }

    /// Opens a bidirectional stream.
    #[wasm_bindgen(js_name = openBi, unchecked_return_type = "Promise<BiStream>")]
    pub fn open_bi(&self) -> Promise {
        let conn = self.clone_handle();
        future_to_promise(async move {
            let (s, r) = conn
                .inner
                .open_bi()
                .await
                .map_err(|e| named_err("ConnectionError", e))?;
            touch(&conn.activity);
            Ok(conn.bi(s, r))
        })
    }

    /// Waits for the peer to open a unidirectional stream.
    #[wasm_bindgen(js_name = acceptUni, unchecked_return_type = "Promise<RecvStream>")]
    pub fn accept_uni(&self) -> Promise {
        let conn = self.clone_handle();
        future_to_promise(async move {
            let r = conn
                .inner
                .accept_uni()
                .await
                .map_err(|e| named_err("ConnectionError", e))?;
            touch(&conn.activity);
            Ok(conn.recv(r).into())
        })
    }

    /// Opens a unidirectional stream.
    #[wasm_bindgen(js_name = openUni, unchecked_return_type = "Promise<SendStream>")]
    pub fn open_uni(&self) -> Promise {
        let conn = self.clone_handle();
        future_to_promise(async move {
            let s = conn
                .inner
                .open_uni()
                .await
                .map_err(|e| named_err("ConnectionError", e))?;
            touch(&conn.activity);
            Ok(conn.send(s).into())
        })
    }

    /// Closes the connection with an application error code and reason.
    pub fn close(&self, error_code: u64, reason: &[u8]) -> Result<(), JsValue> {
        self.inner.close(var_int(error_code)?, reason);
        Ok(())
    }

    /// Resolves with the reason once the connection is closed, by either side.
    #[wasm_bindgen(unchecked_return_type = "Promise<string>")]
    pub fn closed(&self) -> Promise {
        let conn = self.inner.clone();
        future_to_promise(async move { Ok(JsValue::from_str(&conn.closed().await.to_string())) })
    }

    /// The close reason, or null while open.
    #[wasm_bindgen(js_name = closeReason, unchecked_return_type = "string | null")]
    pub fn close_reason(&self) -> JsValue {
        self.inner
            .close_reason()
            .map_or(JsValue::NULL, |e| JsValue::from_str(&e.to_string()))
    }
}

/// The sending half of a stream.
#[wasm_bindgen]
#[derive(Clone)]
pub struct SendStream(Rc<Shared<endpoint::SendStream>>);

impl SendStream {
    /// Writes some of `buf` under the I/O lock the caller holds.
    async fn write_some(&self, buf: &[u8]) -> Result<usize, JsValue> {
        let s = &self.0;
        let res = s
            .poll_op(|stream, cx| pin!(stream.write(buf)).poll(cx))
            .await;
        touch(&s.activity);
        res.map_err(|e| {
            s.open.done();
            e.map_or_else(closed_stream, |e| named_err("WriteError", e))
        })
    }
}

#[wasm_bindgen]
impl SendStream {
    /// Writes some of `buf`; resolves with the number of bytes written.
    #[wasm_bindgen(unchecked_return_type = "Promise<number>")]
    pub fn write(&self, buf: Vec<u8>) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            touch(&this.0.activity);
            let _io = this.0.io.lock().await;
            let n = this.write_some(&buf).await?;
            Ok(JsValue::from_f64(n as f64))
        })
    }

    /// Writes all of `buf`.
    #[wasm_bindgen(js_name = writeAll, unchecked_return_type = "Promise<void>")]
    pub fn write_all(&self, buf: Vec<u8>) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            touch(&this.0.activity);
            let _io = this.0.io.lock().await;
            let mut rest = buf.as_slice();
            while !rest.is_empty() {
                let n = this.write_some(rest).await?;
                rest = &rest[n..];
            }
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Finishes the stream after the writes already called: the peer reads to its end.
    #[wasm_bindgen(unchecked_return_type = "Promise<void>")]
    pub fn finish(&self) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            let _io = this.0.io.lock().await;
            let res = this.0.borrow_mut()?.finish();
            this.0.open.done();
            res.map_err(|e| named_err("ClosedStream", e))?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Waits until the peer has received all data (resolves with null) or stopped the stream
    /// (resolves with its error code). Does not wait for pending writes.
    #[wasm_bindgen(unchecked_return_type = "Promise<number | null>")]
    pub fn stopped(&self) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            let stopped = this.0.borrow_mut()?.stopped();
            let res = stopped.await;
            // Either way nothing more can be sent that the peer will read.
            this.0.open.done();
            let code = res.map_err(|e| named_err("StoppedError", e))?;
            Ok(code.map_or(JsValue::NULL, |c| JsValue::from_f64(c.into_inner() as f64)))
        })
    }

    /// Abandons the stream with an application error code. A pending write fails at once.
    #[wasm_bindgen(unchecked_return_type = "Promise<void>")]
    pub fn reset(&self, error_code: u64) -> Promise {
        let res = var_int(error_code).and_then(|code| {
            let r = self.0.borrow_mut()?.reset(code);
            self.0.abandon();
            r.map_err(|e| named_err("ClosedStream", e))
        });
        future_to_promise(async move { res.map(|()| JsValue::UNDEFINED) })
    }

    /// The stream's id, as text.
    #[wasm_bindgen(unchecked_return_type = "Promise<string>")]
    pub fn id(&self) -> Promise {
        let id = self.0.stream.borrow().id().to_string();
        Promise::resolve(&JsValue::from_str(&id))
    }

    /// Sets the stream's priority: data of higher priority streams is sent first.
    #[wasm_bindgen(js_name = setPriority, unchecked_return_type = "Promise<void>")]
    pub fn set_priority(&self, p: i32) -> Promise {
        let res = self
            .0
            .stream
            .borrow()
            .set_priority(p)
            .map(|()| JsValue::UNDEFINED)
            .map_err(|e| named_err("ClosedStream", e));
        future_to_promise(async move { res })
    }

    /// The stream's priority.
    #[wasm_bindgen(unchecked_return_type = "Promise<number>")]
    pub fn priority(&self) -> Promise {
        let res = self
            .0
            .stream
            .borrow()
            .priority()
            .map(|p| JsValue::from_f64(f64::from(p)))
            .map_err(|e| named_err("ClosedStream", e));
        future_to_promise(async move { res })
    }
}

/// The receiving half of a stream.
#[wasm_bindgen]
#[derive(Clone)]
pub struct RecvStream(Rc<Shared<endpoint::RecvStream>>);

impl RecvStream {
    /// Reads the next chunk of at most `max` bytes under the I/O lock the caller holds.
    /// `Ok(None)` at the end of the stream. Any outcome but data ends the half.
    async fn chunk(&self, max: usize) -> Result<Option<bytes::Bytes>, JsValue> {
        let s = &self.0;
        let res = s
            .poll_op(|stream, cx| pin!(stream.read_chunk(max)).poll(cx))
            .await;
        touch(&s.activity);
        match res {
            Ok(Some(b)) => Ok(Some(b)),
            Ok(None) => {
                s.open.done();
                Ok(None)
            }
            Err(e) => {
                s.open.done();
                Err(e.map_or_else(closed_stream, |e| named_err("ReadError", e)))
            }
        }
    }
}

#[wasm_bindgen]
impl RecvStream {
    /// Reads up to `sizeLimit` bytes. Resolves with an empty array at the end of the stream.
    #[wasm_bindgen(unchecked_return_type = "Promise<Uint8Array>")]
    pub fn read(&self, #[wasm_bindgen(js_name = sizeLimit)] size_limit: usize) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            touch(&this.0.activity);
            let _io = this.0.io.lock().await;
            let chunk = this.chunk(size_limit).await?;
            Ok(js_sys::Uint8Array::from(chunk.as_deref().unwrap_or_default()).into())
        })
    }

    /// Reads exactly `size` bytes; fails if the stream ends first.
    #[wasm_bindgen(js_name = readExact, unchecked_return_type = "Promise<Uint8Array>")]
    pub fn read_exact(&self, size: usize) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            touch(&this.0.activity);
            let _io = this.0.io.lock().await;
            let out = js_sys::Uint8Array::new_with_length(size as u32);
            let mut filled = 0;
            while filled < size {
                match this.chunk(size - filled).await? {
                    Some(b) => {
                        out.subarray(filled as u32, (filled + b.len()) as u32)
                            .copy_from(&b);
                        filled += b.len();
                    }
                    None => {
                        return Err(named_err(
                            "ReadExactError",
                            format!("stream finished early ({filled} of {size} bytes)"),
                        ));
                    }
                }
            }
            Ok(out.into())
        })
    }

    /// Reads to the end of the stream, failing beyond `sizeLimit` bytes. Ends the half
    /// whatever the outcome.
    #[wasm_bindgen(js_name = readToEnd, unchecked_return_type = "Promise<Uint8Array>")]
    pub fn read_to_end(&self, #[wasm_bindgen(js_name = sizeLimit)] size_limit: usize) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            touch(&this.0.activity);
            let _io = this.0.io.lock().await;
            let mut buf = Vec::new();
            loop {
                // One byte past the limit tells a stream of exactly `size_limit` bytes from a
                // longer one.
                let room = (size_limit + 1).saturating_sub(buf.len());
                match this.chunk(room).await? {
                    Some(b) => {
                        buf.extend_from_slice(&b);
                        if buf.len() > size_limit {
                            this.0.open.done();
                            return Err(named_err(
                                "ReadToEndError",
                                format!("stream longer than {size_limit} bytes"),
                            ));
                        }
                    }
                    None => break,
                }
            }
            Ok(js_sys::Uint8Array::from(buf.as_slice()).into())
        })
    }

    /// Stops the stream: the peer's writes fail with `errorCode`. A pending read fails at once.
    #[wasm_bindgen(unchecked_return_type = "Promise<void>")]
    pub fn stop(&self, #[wasm_bindgen(js_name = errorCode)] error_code: u64) -> Promise {
        let res = var_int(error_code).and_then(|code| {
            let r = self.0.borrow_mut()?.stop(code);
            self.0.abandon();
            r.map_err(|e| named_err("ClosedStream", e))
        });
        future_to_promise(async move { res.map(|()| JsValue::UNDEFINED) })
    }

    /// Resolves with the peer's reset code once it resets the stream, or null if the stream
    /// ended otherwise (finished and read, or stopped).
    #[wasm_bindgen(js_name = receivedReset, unchecked_return_type = "Promise<number | null>")]
    pub fn received_reset(&self) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            let _io = this.0.io.lock().await;
            let res = this
                .0
                .poll_op(|stream, cx| pin!(stream.received_reset()).poll(cx))
                .await;
            match res {
                Ok(Some(code)) => {
                    this.0.open.done();
                    Ok(JsValue::from_f64(code.into_inner() as f64))
                }
                Ok(None) | Err(None) => Ok(JsValue::NULL),
                Err(Some(e)) => Err(named_err("ResetError", e)),
            }
        })
    }

    /// The stream's id, as text.
    #[wasm_bindgen(unchecked_return_type = "Promise<string>")]
    pub fn id(&self) -> Promise {
        let id = self.0.stream.borrow().id().to_string();
        Promise::resolve(&JsValue::from_str(&id))
    }

    /// Bytes read so far, by offset.
    #[wasm_bindgen(js_name = bytesRead, unchecked_return_type = "Promise<number>")]
    pub fn bytes_read(&self) -> Promise {
        let res = self
            .0
            .stream
            .borrow()
            .bytes_read()
            .map(|n| JsValue::from_f64(n as f64))
            .map_err(|e| named_err("ClosedStream", e));
        future_to_promise(async move { res })
    }
}
