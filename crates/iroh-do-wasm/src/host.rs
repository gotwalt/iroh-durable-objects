//! The JavaScript side of the node: the TypeScript object's callbacks, and the per-isolate
//! state that the node's `Send + Sync` callbacks reach through a thread-local.
//!
//! `iroh_do_core::Host` (and iroh's custom transport sender below it) must be `Send + Sync`;
//! JavaScript values are neither. wasm32 has one thread, so, as in the workers-rs host, the
//! `Host` value carries only the local EndpointId and looks everything else up here. The
//! objects of one isolate share its wasm instance, hence the map.

use std::{cell::RefCell, collections::HashMap, io, num::NonZeroU16, time::Duration};

use iroh::EndpointId;
use iroh_do_core::{FrameSink, Host, Sql};
use iroh_relay_ingress::{DatagramBatch, IngressSession, encode_datagrams};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(typescript_custom_section)]
const NODE_HOST_TS: &str = r#"
/** What the TypeScript object provides to the node. Called synchronously from wasm. */
export interface NodeHost {
  /** Writes one relay frame to the socket with this id. Throws if the socket is gone. */
  sendFrame(sid: string, frame: Uint8Array): void;
  /** Runs the app's handler for an accepted connection; the promise settles when it is done. */
  accept(alpn: string, conn: Connection): Promise<void>;
  /** Persists work under `key`, due in `delayMs`, and arms the alarm. Only Rust apps schedule. */
  schedule(key: string, delayMs: number, payload: string): void;
  /** Deletes work under `key`. */
  cancel(key: string): void;
  /** Called each time the node binds an endpoint. */
  endpointBuilt(): void;
  /** Called each time the node has closed its endpoint after t_linger. */
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

/// One object's state in this isolate.
pub(crate) struct Entry {
    pub(crate) host: NodeHost,
    /// Ingress sessions by socket id.
    pub(crate) sessions: HashMap<String, IngressSession>,
    /// The socket of each authenticated peer.
    pub(crate) peers: HashMap<EndpointId, String>,
    /// The app's ALPNs and their idle limits, from `protocols()`.
    pub(crate) alpns: Vec<(Vec<u8>, Option<Duration>)>,
}

thread_local! {
    static ENTRIES: RefCell<HashMap<EndpointId, Entry>> = RefCell::new(HashMap::new());
}

/// Replaces the state of `id` (a new object instance for the same endpoint).
pub(crate) fn install(id: EndpointId, entry: Entry) {
    ENTRIES.with(|e| e.borrow_mut().insert(id, entry));
}

/// Forgets the state of `id`.
pub(crate) fn remove(id: &EndpointId) {
    ENTRIES.with(|e| e.borrow_mut().remove(id));
}

/// Runs `f` on the state of `id`. `f` must not call into JS that can re-enter wasm.
pub(crate) fn with<T>(id: &EndpointId, f: impl FnOnce(&mut Entry) -> T) -> Option<T> {
    ENTRIES.with(|e| e.borrow_mut().get_mut(id).map(f))
}

/// Returns a clone of the host object of `id`, to call outside the borrow.
pub(crate) fn host(id: &EndpointId) -> Option<NodeHost> {
    with(id, |e| e.host.clone().unchecked_into())
}

/// Sends `frame` to socket `sid` of object `id`.
pub(crate) fn send(id: &EndpointId, sid: &str, frame: &[u8]) -> Result<(), JsValue> {
    match host(id) {
        Some(h) => h.send_frame(sid, frame),
        None => Err(crate::js_err("object gone")),
    }
}

/// The node's [`Host`]: the local id only (see the module docs).
#[derive(Debug, Clone, Copy)]
pub(crate) struct JsHost {
    pub(crate) local: EndpointId,
}

impl FrameSink for JsHost {
    fn send_datagrams(
        &self,
        peer: &EndpointId,
        contents: &[u8],
        segment_size: Option<usize>,
    ) -> io::Result<()> {
        let sid = with(&self.local, |e| e.peers.get(peer).cloned())
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
        send(&self.local, &sid, frame.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "socket send failed"))
    }
}

impl Host for JsHost {
    fn schedule(&self, key: &str, delay: Duration, payload: &str) {
        if let Some(h) = host(&self.local)
            && let Err(e) = h.schedule(key, delay.as_millis() as f64, payload)
        {
            tracing::error!(event = "schedule_failed", error = ?e);
        }
    }

    fn cancel(&self, key: &str) {
        if let Some(h) = host(&self.local)
            && let Err(e) = h.cancel(key)
        {
            tracing::error!(event = "schedule_failed", error = ?e);
        }
    }

    /// TypeScript apps use `ctx.storage.sql` directly. No Rust app runs on this host.
    fn with_sql(&self, _f: &mut dyn FnMut(&dyn Sql)) {}

    fn endpoint_built(&self) {
        if let Some(h) = host(&self.local)
            && let Err(e) = h.endpoint_built()
        {
            tracing::error!(event = "host_callback_failed", callback = "endpointBuilt", error = ?e);
        }
    }

    fn endpoint_closed(&self) {
        if let Some(h) = host(&self.local)
            && let Err(e) = h.endpoint_closed()
        {
            tracing::error!(event = "host_callback_failed", callback = "endpointClosed", error = ?e);
        }
    }
}
