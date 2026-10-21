//! An iroh custom transport whose paths are the peers connected to a relay ingress.
//!
//! An iroh [`Endpoint`](iroh::Endpoint) hosted in a Durable Object has no IP sockets and no
//! relay client. Peers reach it through its relay URL, where the object runs the server side
//! of the iroh relay protocol for its own EndpointId only. This crate turns those relay
//! connections into iroh paths: every peer holding an authenticated relay WebSocket to the
//! object is one path.
//!
//! The crate does not speak the relay protocol, own sockets or spawn tasks. The host does
//! those things and drives the transport in two directions:
//!
//! - For each relay datagram frame a peer sends, the host calls
//!   [`IngressTransport::push_datagrams`] with the peer's id.
//! - For each transmit iroh addresses to a peer, the transport calls the host's
//!   [`FrameSink`], which writes one relay datagram frame to that peer's socket.
//!
//! ```no_run
//! # use std::{io, sync::Arc};
//! # use iroh::{EndpointId, SecretKey, endpoint::{BindError, Builder}};
//! # use iroh_ingress_transport::{FrameSink, IngressTransport, OnlyIngress};
//! # #[derive(Debug)]
//! # struct Sockets;
//! # impl FrameSink for Sockets {
//! #     fn send_datagrams(&self, _: &EndpointId, _: &[u8], _: Option<usize>) -> io::Result<()> {
//! #         Ok(())
//! #     }
//! # }
//! # async fn run(secret_key: SecretKey, sockets: Arc<Sockets>) -> Result<(), BindError> {
//! let transport = IngressTransport::new(secret_key.public(), sockets);
//! let endpoint = Builder::empty()
//!     .secret_key(secret_key)
//!     .add_custom_transport(Arc::new(transport.clone()))
//!     .path_selector(Arc::new(OnlyIngress))
//!     .clear_ip_transports()
//!     .bind()
//!     .await?;
//! // On each relay datagram frame from `peer`:
//! // transport.push_datagrams(peer, ecn, contents, segment_size);
//! # Ok(())
//! # }
//! ```
//!
//! # Paths are peer ids
//!
//! A peer reaches the object over exactly one relay socket, so the peer's EndpointId is the
//! whole path address: the [`CustomAddr`] of a path is the peer's 32-byte key under
//! [`INGRESS_TRANSPORT_ID`]. The address is local to the object. It is never published and
//! peers never see it; from the peer's side the path is an ordinary relay path to the
//! object's relay URL.
//!
//! # No dialing out
//!
//! The transport only writes to sockets that peers opened. When a peer has no socket, the
//! sink returns [`io::ErrorKind::NotConnected`] and iroh counts the transmit as lost. The
//! transport does not queue for that peer and does not open a connection to it: the object
//! has no relay client and knows no address for a peer other than the peer's own socket.
//! If the peer reconnects, QUIC retransmits what was lost.
//!
//! # Transport id
//!
//! [`INGRESS_TRANSPORT_ID`] is provisional and not yet registered in iroh's `TRANSPORTS.md`.
//! Because the address never leaves the object, a collision with another transport's id
//! could only matter inside the object's own process.
//!
//! The crate has no async runtime dependency and builds for `wasm32-unknown-unknown`.
#![deny(missing_docs)]

use std::{
    collections::VecDeque,
    fmt, io,
    num::NonZeroUsize,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll, Waker},
};

use bytes::Bytes;
use iroh::{
    EndpointAddr, EndpointId, TransportAddr,
    endpoint::transports::{
        Addr, CustomEndpoint, CustomSender, CustomTransport, PathSelection, PathSelectionContext,
        PathSelector, RecvInfo, Transmit,
    },
};
pub use iroh_base::CustomAddr;
use noq_udp::{EcnCodepoint, RecvMeta};
use tracing::{debug, trace};

/// Custom transport id of ingress paths: ASCII `"INGR"`, provisional.
///
/// iroh's `TRANSPORTS.md` reserves `0x00..=0x1F`; this id is outside that range and not yet
/// registered. The address data is the peer's 32-byte Ed25519 public key.
pub const INGRESS_TRANSPORT_ID: u64 = 0x494E_4752;

/// Received datagrams buffered until iroh polls them; further datagrams are dropped.
///
/// QUIC recovers from loss, and an unbounded buffer would let a peer exhaust the object's
/// memory.
const INBOX_CAP: usize = 1024;

/// Returns the ingress path address of `peer`.
pub fn peer_addr(peer: &EndpointId) -> CustomAddr {
    CustomAddr::from_parts(INGRESS_TRANSPORT_ID, peer.as_bytes())
}

/// Returns an [`EndpointAddr`] for `peer` whose only address is its ingress path.
///
/// Connecting to this address sends over `peer`'s existing relay socket to the object, with
/// no address lookup. This is how the object dials back a peer.
pub fn peer_endpoint_addr(peer: EndpointId) -> EndpointAddr {
    EndpointAddr::new(peer).with_addrs([TransportAddr::Custom(peer_addr(&peer))])
}

/// Returns the peer id of an ingress path address.
///
/// Returns `None` for addresses of other transports and for data that is not a valid key.
pub fn parse_peer(addr: &CustomAddr) -> Option<EndpointId> {
    if addr.id() != INGRESS_TRANSPORT_ID {
        return None;
    }
    let bytes: &[u8; 32] = addr.data().try_into().ok()?;
    EndpointId::from_bytes(bytes).ok()
}

/// Writes outgoing datagrams to peers' relay sockets; implemented by the host.
///
/// An implementation encodes the datagrams as one relay datagram frame whose source is the
/// object's own EndpointId and writes it to `peer`'s socket. It must not block, must not
/// queue for a peer without a socket and must not open a connection.
///
/// The `Send + Sync` bounds come from iroh: [`CustomTransport`] and [`CustomSender`] require
/// them. A Durable Object runs on one thread and its runtime handles are not `Send`; a host
/// there can keep those handles in a thread-local and store only a key in the sink.
pub trait FrameSink: fmt::Debug + Send + Sync + 'static {
    /// Writes `contents` to `peer`'s socket.
    ///
    /// `contents` holds one datagram, or, when `segment_size` is `Some`, several datagrams of
    /// `segment_size` bytes each, the last possibly shorter.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::NotConnected`] when `peer` has no socket. iroh logs any error
    /// and treats the transmit as lost.
    fn send_datagrams(
        &self,
        peer: &EndpointId,
        contents: &[u8],
        segment_size: Option<usize>,
    ) -> io::Result<()>;
}

#[derive(Debug, Default)]
struct Inbox {
    queue: VecDeque<(EndpointId, Option<EcnCodepoint>, Bytes)>,
    waker: Option<Waker>,
    dropped: u64,
}

/// The ingress transport.
///
/// Create one per [`Endpoint`](iroh::Endpoint) and register it with
/// [`Builder::add_custom_transport`](iroh::endpoint::Builder::add_custom_transport) together
/// with the [`OnlyIngress`] path selector. Keep a clone to push received datagrams; clones
/// share one receive queue.
#[derive(Debug, Clone)]
pub struct IngressTransport {
    inbox: Arc<Mutex<Inbox>>,
    sink: Arc<dyn FrameSink>,
    local: n0_watcher::Watchable<Vec<CustomAddr>>,
}

impl IngressTransport {
    /// Creates the transport for the local endpoint `local`, sending through `sink`.
    pub fn new(local: EndpointId, sink: Arc<dyn FrameSink>) -> Self {
        Self {
            inbox: Default::default(),
            sink,
            local: n0_watcher::Watchable::new(vec![peer_addr(&local)]),
        }
    }

    /// Queues the datagrams of one relay datagram frame received from `from`.
    ///
    /// `ecn` is the frame's ECN byte; only the low two bits are used. When `segment_size` is
    /// `Some`, `contents` is split into datagrams of that size. Returns the number of
    /// datagrams queued. Datagrams that do not fit the receive queue are dropped and counted
    /// in [`dropped`](Self::dropped).
    pub fn push_datagrams(
        &self,
        from: EndpointId,
        ecn: u8,
        contents: Bytes,
        segment_size: Option<usize>,
    ) -> usize {
        let ecn = EcnCodepoint::from_bits(ecn & 0b11);
        let mut inbox = lock(&self.inbox);
        let mut queued = 0;
        for d in segments(contents, segment_size) {
            if inbox.queue.len() >= INBOX_CAP {
                inbox.dropped += 1;
                trace!(
                    dropped = inbox.dropped,
                    "ingress inbox full, dropping datagram"
                );
                continue;
            }
            inbox.queue.push_back((from, ecn, d));
            queued += 1;
        }
        if queued > 0
            && let Some(w) = inbox.waker.take()
        {
            w.wake();
        }
        queued
    }

    /// Returns the number of received datagrams dropped because the queue was full or the
    /// datagram was larger than iroh's receive buffer.
    pub fn dropped(&self) -> u64 {
        lock(&self.inbox).dropped
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Splits `contents` into datagrams of `segment_size` bytes (the last may be shorter).
fn segments(contents: Bytes, segment_size: Option<usize>) -> impl Iterator<Item = Bytes> {
    let len = contents.len();
    let step = match segment_size {
        Some(s) if s > 0 => s,
        _ => len.max(1),
    };
    (0..len)
        .step_by(step)
        .map(move |start| contents.slice(start..(start + step).min(len)))
}

impl CustomTransport for IngressTransport {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
        Ok(Box::new(IngressEndpoint(self.clone())))
    }
}

#[derive(Debug)]
struct IngressEndpoint(IngressTransport);

impl CustomEndpoint for IngressEndpoint {
    fn watch_local_addrs(&self) -> n0_watcher::Direct<Vec<CustomAddr>> {
        self.0.local.watch()
    }

    fn create_sender(&self) -> Arc<dyn CustomSender> {
        Arc::new(IngressSender(self.0.sink.clone()))
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context,
        bufs: &mut [io::IoSliceMut<'_>],
        metas: &mut [RecvMeta],
        recv_infos: &mut [RecvInfo],
    ) -> Poll<io::Result<usize>> {
        let mut inbox = lock(&self.0.inbox);
        let mut n = 0;
        let slots = bufs.len().min(metas.len()).min(recv_infos.len());
        while n < slots {
            let Some((from, ecn, d)) = inbox.queue.pop_front() else {
                break;
            };
            if d.len() > bufs[n].len() {
                // Larger than noq's receive buffer (`max_udp_payload_size`, 1472 bytes).
                inbox.dropped += 1;
                debug!(len = d.len(), "dropping oversize ingress datagram");
                continue;
            }
            bufs[n][..d.len()].copy_from_slice(&d);
            metas[n].len = d.len();
            metas[n].stride = d.len();
            metas[n].ecn = ecn;
            recv_infos[n] = RecvInfo::new(peer_addr(&from), None);
            n += 1;
        }
        if n > 0 {
            Poll::Ready(Ok(n))
        } else {
            // Never return an error when idle: iroh shuts the endpoint down when every
            // transport errors repeatedly (iroh 1.4.0, src/socket/transports.rs:341-357).
            inbox.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    fn max_transmit_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }
}

#[derive(Debug)]
struct IngressSender(Arc<dyn FrameSink>);

impl CustomSender for IngressSender {
    fn is_valid_send_addr(&self, addr: &CustomAddr) -> bool {
        addr.id() == INGRESS_TRANSPORT_ID
    }

    fn poll_send(
        &self,
        _cx: &mut Context,
        dst: &CustomAddr,
        _src: Option<&CustomAddr>,
        transmit: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        // Never return Pending: iroh treats Pending like an error, as a dropped transmit
        // (iroh 1.4.0, src/socket/transports.rs:1277-1299). Outgoing frames carry no ECN
        // because `Transmit::ecn` is `pub(crate)`.
        let Some(peer) = parse_peer(dst) else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not an ingress address",
            )));
        };
        let res = self
            .0
            .send_datagrams(&peer, transmit.contents, transmit.segment_size);
        if let Err(e) = &res {
            trace!(peer = %peer.fmt_short(), "ingress send failed: {e}");
        }
        Poll::Ready(res)
    }
}

/// Path selector that picks the ingress path, for an endpoint whose only transport is ingress.
#[derive(Debug, Default, Clone, Copy)]
pub struct OnlyIngress;

impl PathSelector for OnlyIngress {
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        let mut sel = PathSelection::none();
        if let Some(p) = ctx.paths().find(|p| {
            matches!(p.network_path().remote(), Addr::Custom(c) if c.id() == INGRESS_TRANSPORT_ID)
        }) {
            sel.set(&p);
        }
        sel
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path addresses round-trip, and foreign ids or malformed keys are rejected.
    #[test]
    fn addr_round_trip() {
        let id = iroh::SecretKey::from([7u8; 32]).public();
        let a = peer_addr(&id);
        assert_eq!(a.id(), INGRESS_TRANSPORT_ID);
        assert_eq!(parse_peer(&a), Some(id));
        assert_eq!(
            parse_peer(&CustomAddr::from_parts(0x20, id.as_bytes())),
            None
        );
        assert_eq!(
            parse_peer(&CustomAddr::from_parts(INGRESS_TRANSPORT_ID, &[1, 2])),
            None
        );
    }

    /// Segmented contents split at `segment_size`; `None` and `Some(0)` mean one datagram.
    #[test]
    fn segmentation() {
        let b = Bytes::from_static(b"abcdefg");
        let v: Vec<_> = segments(b.clone(), Some(3)).collect();
        assert_eq!(v, vec![&b"abc"[..], &b"def"[..], &b"g"[..]]);
        let v: Vec<_> = segments(b.clone(), None).collect();
        assert_eq!(v, vec![&b"abcdefg"[..]]);
        let v: Vec<_> = segments(b, Some(0)).collect();
        assert_eq!(v.len(), 1);
        assert_eq!(segments(Bytes::new(), Some(3)).count(), 0);
    }

    #[derive(Debug)]
    struct Nowhere;
    impl FrameSink for Nowhere {
        fn send_datagrams(&self, _: &EndpointId, _: &[u8], _: Option<usize>) -> io::Result<()> {
            Err(io::ErrorKind::NotConnected.into())
        }
    }

    /// Datagrams beyond the receive queue bound are dropped and counted.
    #[test]
    fn inbox_is_bounded() {
        let id = iroh::SecretKey::from([7u8; 32]).public();
        let t = IngressTransport::new(id, Arc::new(Nowhere));
        let big = Bytes::from(vec![0u8; INBOX_CAP + 10]);
        assert_eq!(t.push_datagrams(id, 0, big, Some(1)), INBOX_CAP);
        assert_eq!(t.dropped(), 10);
    }
}
