//! Per-isolate registry of authenticated peer sockets, and the
//! [`FrameSink`](iroh_ingress_transport::FrameSink) that writes through it.
//!
//! iroh's transport traits require `Send + Sync`; JS `WebSocket` handles are
//! neither. wasm32 is single-threaded, so the handles live in a thread-local
//! keyed by the local EndpointId (several DO instances may share an isolate)
//! and the sink only carries that id.

use std::{cell::RefCell, collections::HashMap, io, num::NonZeroU16};

use iroh_base::EndpointId;
use iroh_relay_ingress::{DatagramBatch, encode_datagrams};
use worker::WebSocket;

thread_local! {
    static LINKS: RefCell<HashMap<EndpointId, HashMap<EndpointId, WebSocket>>> = RefCell::new(HashMap::new());
}

/// Bind `peer` to `ws` for endpoint `local`. Returns the previous socket, if any.
pub fn bind(local: EndpointId, peer: EndpointId, ws: WebSocket) -> Option<WebSocket> {
    LINKS.with(|l| l.borrow_mut().entry(local).or_default().insert(peer, ws))
}

/// Remove `peer`'s binding.
pub fn unbind(local: &EndpointId, peer: &EndpointId) {
    LINKS.with(|l| {
        if let Some(m) = l.borrow_mut().get_mut(local) {
            m.remove(peer);
        }
    });
}

/// Drop every binding of `local` (constructor re-run).
pub fn reset(local: &EndpointId) {
    LINKS.with(|l| {
        l.borrow_mut().remove(local);
    });
}

/// Peers with a live socket.
pub fn peers(local: &EndpointId) -> Vec<EndpointId> {
    LINKS.with(|l| {
        l.borrow()
            .get(local)
            .map(|m| m.keys().copied().collect())
            .unwrap_or_default()
    })
}

/// Encode and write datagrams from `local` to `peer`.
pub fn send(
    local: &EndpointId,
    peer: &EndpointId,
    contents: &[u8],
    segment_size: Option<usize>,
) -> io::Result<()> {
    let ws = LINKS
        .with(|l| l.borrow().get(local).and_then(|m| m.get(peer).cloned()))
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
    let frame = encode_datagrams(local, &batch).map_err(io::Error::other)?;
    ws.send_with_bytes(frame.as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))
}
