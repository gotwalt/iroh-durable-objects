//! The JavaScript side of the node: the TypeScript object's callbacks, and the per-instance
//! state that the node's `Send + Sync` callbacks reach through a thread-local.
//!
//! `iroh_do_core::Host` (and iroh's custom transport sender below it) must be `Send + Sync`;
//! JavaScript values are neither. wasm32 has one thread, so, as in the workers-rs host, the
//! `Host` value carries only the local EndpointId and a [`Token`], and looks everything else
//! up here. The objects of one isolate share its wasm instance, hence the map.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io,
    num::NonZeroU16,
    time::Duration,
};

use iroh::EndpointId;
use iroh_do_core::{FrameSink, Host, Sql};
use iroh_relay_ingress::{DatagramBatch, IngressSession, encode_datagrams};
use js_sys::{Object, WeakRef};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(typescript_custom_section)]
const NODE_HOST_TS: &str = r#"
/**
 * What the TypeScript object provides to the node. Called synchronously from wasm.
 *
 * The node holds this object weakly, so that an object the runtime has dropped can be
 * garbage-collected together with its node: the caller keeps a reference to it for as long
 * as it uses the node. No callback may call back into the node.
 */
export interface NodeHost {
  /**
   * Writes one relay frame to the socket with this id. Throws if the socket is gone.
   *
   * `frame` is a view into wasm memory, valid only during the call: copy it, or pass it to
   * an API that copies synchronously (`WebSocket.send` does), before returning.
   */
  sendFrame(sid: string, frame: Uint8Array): void;
  /** Runs the app's handler for an accepted connection; the promise settles when it is done. */
  accept(alpn: string, conn: Connection): Promise<void>;
  /** Persists work under `key`, due in `delayMs`, and arms the alarm. Only Rust apps schedule. */
  schedule(key: string, delayMs: number, payload: string): void;
  /** Deletes work under `key`. */
  cancel(key: string): void;
  /** Called after the node has bound an endpoint. */
  endpointBuilt(): void;
  /** Called after the node has closed its endpoint, t_linger after the last connection. */
  endpointClosed(): void;
}
"#;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(typescript_type = "NodeHost")]
    pub type NodeHost;

    #[wasm_bindgen(method, catch, js_name = sendFrame)]
    fn send_frame(this: &NodeHost, sid: &str, frame: &[u8]) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch)]
    pub(crate) fn accept(
        this: &NodeHost,
        alpn: &str,
        conn: crate::streams::Connection,
    ) -> Result<js_sys::Promise, JsValue>;

    #[wasm_bindgen(method, catch)]
    fn schedule(this: &NodeHost, key: &str, delay_ms: f64, payload: &str) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch)]
    fn cancel(this: &NodeHost, key: &str) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch, js_name = endpointBuilt)]
    fn endpoint_built(this: &NodeHost) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch, js_name = endpointClosed)]
    fn endpoint_closed(this: &NodeHost) -> Result<(), JsValue>;
}

/// Identifies one `WasmNode` among the nodes of this instance.
///
/// Not the EndpointId: two nodes can share one, when a new construction of an object builds
/// its node while the previous construction's node still exists (until the garbage collector
/// frees it), and the old node's tasks must not reach the new one's sockets.
pub(crate) type Token = u64;

/// One node's state in this instance.
pub(crate) struct Entry {
    /// The object's callbacks, held weakly. A strong reference from wasm would keep the
    /// object's whole graph alive, because the garbage collector cannot see into wasm memory,
    /// and the `WasmNode`, whose `Drop` removes this entry, would never be freed.
    host: WeakRef<Object>,
    /// Ingress sessions by socket id.
    pub(crate) sessions: HashMap<String, IngressSession>,
    /// The socket of each authenticated peer.
    pub(crate) peers: HashMap<EndpointId, String>,
    /// Challenges of accepted upgrades, until the socket is open for writing (`opened`).
    pub(crate) challenges: HashMap<String, Vec<u8>>,
    /// The app's ALPNs and their idle limits, from `protocols()`.
    pub(crate) alpns: Vec<(Vec<u8>, Option<Duration>)>,
}

impl Entry {
    pub(crate) fn new(host: &NodeHost, alpns: Vec<(Vec<u8>, Option<Duration>)>) -> Self {
        Entry {
            host: WeakRef::new(host.unchecked_ref::<Object>()),
            sessions: HashMap::new(),
            peers: HashMap::new(),
            challenges: HashMap::new(),
            alpns,
        }
    }
}

thread_local! {
    static ENTRIES: RefCell<HashMap<Token, Entry>> = RefCell::new(HashMap::new());
    static NEXT_TOKEN: Cell<Token> = const { Cell::new(1) };
    /// The token of the node being constructed, for `JsApp::new`, which `Node::new` calls.
    static BUILDING: Cell<Option<Token>> = const { Cell::new(None) };
}

/// Stores `entry` under a new token.
pub(crate) fn install(entry: Entry) -> Token {
    let token = NEXT_TOKEN.with(|t| {
        let v = t.get();
        t.set(v + 1);
        v
    });
    ENTRIES.with(|e| e.borrow_mut().insert(token, entry));
    token
}

/// Forgets the state of `token`.
pub(crate) fn remove(token: Token) {
    ENTRIES.with(|e| e.borrow_mut().remove(&token));
}

/// The number of nodes with state in this instance.
pub(crate) fn count() -> usize {
    ENTRIES.with(|e| e.borrow().len())
}

/// Runs `f` with `token` marked as the node under construction.
pub(crate) fn building<T>(token: Token, f: impl FnOnce() -> T) -> T {
    BUILDING.with(|b| b.set(Some(token)));
    let out = f();
    BUILDING.with(|b| b.set(None));
    out
}

/// The token of the node under construction ([`building`]).
pub(crate) fn current_build() -> Option<Token> {
    BUILDING.with(Cell::get)
}

/// Runs `f` on the state of `token`. `f` must not call into JS that can re-enter wasm.
pub(crate) fn with<T>(token: Token, f: impl FnOnce(&mut Entry) -> T) -> Option<T> {
    ENTRIES.with(|e| e.borrow_mut().get_mut(&token).map(f))
}

/// Returns the host object of `token`, to call outside the borrow; None if the node is gone or
/// its object was collected.
pub(crate) fn host(token: Token) -> Option<NodeHost> {
    with(token, |e| e.host.deref())
        .flatten()
        .map(JsCast::unchecked_into)
}

/// Sends `frame` to socket `sid` of node `token`.
pub(crate) fn send(token: Token, sid: &str, frame: &[u8]) -> Result<(), JsValue> {
    match host(token) {
        Some(h) => h.send_frame(sid, frame),
        None => Err(crate::js_err("object gone")),
    }
}

/// The node's [`Host`]: the local id and the node's token (see the module docs).
#[derive(Debug, Clone, Copy)]
pub(crate) struct JsHost {
    pub(crate) local: EndpointId,
    pub(crate) token: Token,
}

impl FrameSink for JsHost {
    fn send_datagrams(
        &self,
        peer: &EndpointId,
        contents: &[u8],
        segment_size: Option<usize>,
    ) -> io::Result<()> {
        let sid = with(self.token, |e| e.peers.get(peer).cloned())
            .flatten()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no socket for peer"))?;
        let segment_size = match segment_size {
            Some(s) if s < contents.len() => Some(
                NonZeroU16::new(u16::try_from(s).map_err(|_| io::Error::other("segment size"))?)
                    .ok_or_else(|| io::Error::other("segment size 0"))?,
            ),
            _ => None,
        };
        let batch = DatagramBatch {
            ecn: None,
            segment_size,
            contents,
        };
        let frame = encode_datagrams(&self.local, &batch).map_err(io::Error::other)?;
        send(self.token, &sid, frame.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "socket send failed"))
    }
}

impl Host for JsHost {
    fn schedule(&self, key: &str, delay: Duration, payload: &str) {
        if let Some(h) = host(self.token)
            && let Err(e) = h.schedule(key, delay.as_millis() as f64, payload)
        {
            tracing::error!(event = "schedule_failed", error = ?e);
        }
    }

    fn cancel(&self, key: &str) {
        if let Some(h) = host(self.token)
            && let Err(e) = h.cancel(key)
        {
            tracing::error!(event = "schedule_failed", error = ?e);
        }
    }

    /// TypeScript apps use `ctx.storage.sql` directly. No Rust app runs on this host.
    fn with_sql(&self, _f: &mut dyn FnMut(&dyn Sql)) {}

    fn endpoint_built(&self) {
        if let Some(h) = host(self.token)
            && let Err(e) = h.endpoint_built()
        {
            tracing::error!(event = "host_callback_failed", callback = "endpointBuilt", error = ?e);
        }
    }

    fn endpoint_closed(&self) {
        if let Some(h) = host(self.token)
            && let Err(e) = h.endpoint_closed()
        {
            tracing::error!(event = "host_callback_failed", callback = "endpointClosed", error = ?e);
        }
    }
}
