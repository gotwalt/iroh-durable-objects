//! `Connection`, `BiStream`, `SendStream` and `RecvStream` with the names and signatures of
//! `@number0/iroh` 1.1.0, bytes as `Uint8Array`.
//!
//! The wrappers report to the node's idle close themselves, so apps need no activity API:
//! every read and write records activity, and each stream half counts as open (an
//! [`StreamGuard`]) until the send half is finished or reset and the receive half has reached
//! its end or was stopped. While a stream is open the node closes the connection only after
//! `t_silent` without data; otherwise after `t_idle` (`iroh_do_core::conn`).
//!
//! A half that JavaScript drops without finishing stays open until the wrapper is
//! garbage-collected: wasm-bindgen frees exported objects through a `FinalizationRegistry`.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use iroh::endpoint::{self, VarInt};
use iroh_do_core::{Activity, StreamGuard};
use js_sys::Promise;
use tokio::sync::Mutex;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

use crate::{ids::EndpointId, js_err};

type Act = Option<Arc<Activity>>;

fn touch(a: &Act) {
    if let Some(a) = a {
        a.touch();
    }
}

fn var_int(code: u64) -> Result<VarInt, JsValue> {
    VarInt::from_u64(code).map_err(js_err)
}

/// One open stream half, counted until [`Half::done`] or drop.
#[derive(Clone)]
struct Half(Rc<RefCell<Option<StreamGuard>>>);

impl Half {
    fn new(a: &Act) -> Self {
        Half(Rc::new(RefCell::new(a.as_ref().map(|a| a.stream()))))
    }

    fn done(&self) {
        self.0.borrow_mut().take();
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
        SendStream {
            inner: Rc::new(Mutex::new(s)),
            open: Half::new(&self.activity),
            activity: self.activity.clone(),
        }
    }

    fn recv(&self, r: endpoint::RecvStream) -> RecvStream {
        RecvStream {
            inner: Rc::new(Mutex::new(r)),
            open: Half::new(&self.activity),
            activity: self.activity.clone(),
        }
    }
}

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
            let (s, r) = conn.inner.accept_bi().await.map_err(js_err)?;
            touch(&conn.activity);
            Ok(BiStream {
                send: conn.send(s),
                recv: conn.recv(r),
            }
            .into())
        })
    }

    /// Opens a bidirectional stream.
    #[wasm_bindgen(js_name = openBi, unchecked_return_type = "Promise<BiStream>")]
    pub fn open_bi(&self) -> Promise {
        let conn = self.clone_handle();
        future_to_promise(async move {
            let (s, r) = conn.inner.open_bi().await.map_err(js_err)?;
            touch(&conn.activity);
            Ok(BiStream {
                send: conn.send(s),
                recv: conn.recv(r),
            }
            .into())
        })
    }

    /// Waits for the peer to open a unidirectional stream.
    #[wasm_bindgen(js_name = acceptUni, unchecked_return_type = "Promise<RecvStream>")]
    pub fn accept_uni(&self) -> Promise {
        let conn = self.clone_handle();
        future_to_promise(async move {
            let r = conn.inner.accept_uni().await.map_err(js_err)?;
            touch(&conn.activity);
            Ok(conn.recv(r).into())
        })
    }

    /// Opens a unidirectional stream.
    #[wasm_bindgen(js_name = openUni, unchecked_return_type = "Promise<SendStream>")]
    pub fn open_uni(&self) -> Promise {
        let conn = self.clone_handle();
        future_to_promise(async move {
            let s = conn.inner.open_uni().await.map_err(js_err)?;
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
    #[wasm_bindgen(js_name = closeReason)]
    pub fn close_reason(&self) -> Option<String> {
        self.inner.close_reason().map(|e| e.to_string())
    }

    fn clone_handle(&self) -> Connection {
        Connection {
            inner: self.inner.clone(),
            activity: self.activity.clone(),
        }
    }
}

/// A bidirectional stream.
#[wasm_bindgen]
pub struct BiStream {
    send: SendStream,
    recv: RecvStream,
}

#[wasm_bindgen]
impl BiStream {
    #[wasm_bindgen(getter)]
    pub fn send(&self) -> SendStream {
        self.send.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn recv(&self) -> RecvStream {
        self.recv.clone()
    }
}

/// The sending half of a stream.
#[wasm_bindgen]
#[derive(Clone)]
pub struct SendStream {
    inner: Rc<Mutex<endpoint::SendStream>>,
    open: Half,
    activity: Act,
}

#[wasm_bindgen]
impl SendStream {
    /// Writes some of `buf`; resolves with the number of bytes written.
    #[wasm_bindgen(unchecked_return_type = "Promise<number>")]
    pub fn write(&self, buf: Vec<u8>) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            touch(&this.activity);
            let n = this.inner.lock().await.write(&buf).await.map_err(js_err)?;
            touch(&this.activity);
            Ok(JsValue::from_f64(n as f64))
        })
    }

    /// Writes all of `buf`.
    #[wasm_bindgen(js_name = writeAll, unchecked_return_type = "Promise<void>")]
    pub fn write_all(&self, buf: Vec<u8>) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            touch(&this.activity);
            this.inner
                .lock()
                .await
                .write_all(&buf)
                .await
                .map_err(js_err)?;
            touch(&this.activity);
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Finishes the stream: the peer reads to its end.
    #[wasm_bindgen(unchecked_return_type = "Promise<void>")]
    pub fn finish(&self) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            let res = this.inner.lock().await.finish();
            this.open.done();
            res.map_err(js_err)?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Abandons the stream with an application error code.
    #[wasm_bindgen(unchecked_return_type = "Promise<void>")]
    pub fn reset(&self, error_code: u64) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            let code = var_int(error_code)?;
            let res = this.inner.lock().await.reset(code);
            this.open.done();
            res.map_err(js_err)?;
            Ok(JsValue::UNDEFINED)
        })
    }
}

/// The receiving half of a stream.
#[wasm_bindgen]
#[derive(Clone)]
pub struct RecvStream {
    inner: Rc<Mutex<endpoint::RecvStream>>,
    open: Half,
    activity: Act,
}

#[wasm_bindgen]
impl RecvStream {
    /// Reads up to `size_limit` bytes. Resolves with an empty array at the end of the stream.
    #[wasm_bindgen(unchecked_return_type = "Promise<Uint8Array>")]
    pub fn read(&self, size_limit: usize) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            touch(&this.activity);
            let chunk = this
                .inner
                .lock()
                .await
                .read_chunk(size_limit)
                .await
                .map_err(js_err)?;
            touch(&this.activity);
            let bytes = match chunk {
                Some(b) => b.to_vec(),
                None => {
                    this.open.done();
                    Vec::new()
                }
            };
            Ok(js_sys::Uint8Array::from(bytes.as_slice()).into())
        })
    }

    /// Reads exactly `size` bytes.
    #[wasm_bindgen(js_name = readExact, unchecked_return_type = "Promise<Uint8Array>")]
    pub fn read_exact(&self, size: usize) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            touch(&this.activity);
            let mut buf = vec![0u8; size];
            this.inner
                .lock()
                .await
                .read_exact(&mut buf)
                .await
                .map_err(js_err)?;
            touch(&this.activity);
            Ok(js_sys::Uint8Array::from(buf.as_slice()).into())
        })
    }

    /// Reads to the end of the stream, failing beyond `size_limit` bytes.
    #[wasm_bindgen(js_name = readToEnd, unchecked_return_type = "Promise<Uint8Array>")]
    pub fn read_to_end(&self, size_limit: usize) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            touch(&this.activity);
            let res = this.inner.lock().await.read_to_end(size_limit).await;
            this.open.done();
            let bytes = res.map_err(js_err)?;
            Ok(js_sys::Uint8Array::from(bytes.as_slice()).into())
        })
    }

    /// Stops the stream: the peer's writes fail with `error_code`.
    #[wasm_bindgen(unchecked_return_type = "Promise<void>")]
    pub fn stop(&self, error_code: u64) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            let code = var_int(error_code)?;
            let res = this.inner.lock().await.stop(code);
            this.open.done();
            res.map_err(js_err)?;
            Ok(JsValue::UNDEFINED)
        })
    }
}
