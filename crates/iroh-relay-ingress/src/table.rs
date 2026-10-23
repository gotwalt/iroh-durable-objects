//! The relay sockets of one endpoint: one [`IngressSession`] per socket, and the socket
//! each authenticated peer is reached through.
//!
//! [`Sockets`] is what an object hosting the ingress keeps between socket events. Like the
//! session it is sans-io: the host names each socket with a key of its choosing, passes
//! every socket event in with the current time, and carries out the [`Effect`]s each call
//! returns, in order.
//!
//! One peer has at most one socket. When a second socket of the same peer authenticates,
//! the older one is sent `SameEndpointIdConnected` and closed with [`CLOSE_SUPERSEDED`], as
//! an upstream relay server does. The table holds no socket handles and no attachments: a
//! host that restarts rebuilds it with [`Sockets::restore`] from the snapshots it stored
//! when told to ([`Effect::Persist`]).

use std::{collections::HashMap, fmt::Debug, hash::Hash, num::NonZeroU16, time::Duration};

use iroh_base::EndpointId;
use rand_core::CryptoRng;
use tracing::{info, warn};

use crate::{
    Close, Ecn, Frame, IngressConfig, IngressSession, Output, Reject, SessionSnapshot, Status,
    UpgradeRequest, UpgradeResponse, close_code,
};

/// Close code for a socket superseded by a newer one of the same peer (private-use range,
/// RFC 6455 section 7.4.2).
pub const CLOSE_SUPERSEDED: u16 = 4000;

/// What the host does after a call into [`Sockets`]. Effects are returned in the order the
/// host must carry them out.
#[derive(Debug)]
pub enum Effect<K> {
    /// Write this frame to the socket.
    Send(K, Frame),
    /// Store this snapshot as the socket's attachment, to [`Sockets::restore`] it after a
    /// restart.
    Persist(K, SessionSnapshot),
    /// Close the socket with this code and reason. The table has already forgotten it.
    Close(K, u16, &'static str),
    /// The peer is now reached through this socket: send its datagrams there. It replaces
    /// any earlier socket of the peer.
    PeerUp(EndpointId, K),
    /// The peer has no socket any more.
    PeerDown(EndpointId),
    /// Datagrams from a peer, addressed to this endpoint.
    Datagrams {
        /// The sending peer.
        from: EndpointId,
        /// The frame's ECN codepoint, as [`Ecn::to_bits`] encodes it.
        ecn: u8,
        /// The size of each datagram if `contents` holds several.
        segment_size: Option<NonZeroU16>,
        /// One datagram, or several of `segment_size` bytes.
        contents: Vec<u8>,
    },
    /// An unauthenticated socket must be closed at this time (since the UNIX epoch):
    /// call [`Sockets::sweep`] then. The table sets no timer.
    Deadline(Duration),
}

/// What [`Sockets::upgrade`] accepted.
#[derive(Debug)]
pub struct Upgraded<K> {
    /// The upgrade response: the subprotocol header to answer with, and the challenge, which
    /// the host writes as the socket's first message once it has accepted the socket.
    pub response: UpgradeResponse,
    /// Effects to carry out once the socket is accepted.
    pub effects: Vec<Effect<K>>,
}

/// The relay sockets of one endpoint. See the [module docs](self).
#[derive(Debug)]
pub struct Sockets<K> {
    cfg: IngressConfig,
    sessions: HashMap<K, IngressSession>,
    /// The socket each authenticated peer is reached through.
    peers: HashMap<EndpointId, K>,
}

impl<K: Clone + Eq + Hash + Debug> Sockets<K> {
    /// Creates an empty table for the endpoint of `cfg`.
    pub fn new(cfg: IngressConfig) -> Self {
        Self {
            cfg,
            sessions: HashMap::new(),
            peers: HashMap::new(),
        }
    }

    /// The configuration the sessions are created with.
    pub fn config(&self) -> &IngressConfig {
        &self.cfg
    }

    /// Answers a WebSocket upgrade for a new socket `key`, and adds its session.
    ///
    /// On `Ok` the host accepts the socket, carries out the effects, and writes the
    /// challenge. If accepting fails, it calls [`Sockets::closed`].
    pub fn upgrade<R: CryptoRng + ?Sized>(
        &mut self,
        key: K,
        req: &UpgradeRequest<'_>,
        rng: &mut R,
        now: Duration,
    ) -> Result<Upgraded<K>, Reject> {
        let (response, session) = IngressSession::on_upgrade(req, &self.cfg, rng, now)?;
        let mut effects = Vec::new();
        if let Some(snap) = session.snapshot() {
            effects.push(Effect::Persist(key.clone(), snap));
        }
        if let Some(d) = session.handshake_deadline() {
            effects.push(Effect::Deadline(d));
        }
        self.sessions.insert(key, session);
        Ok(Upgraded { response, effects })
    }

    /// Restores the session of socket `key` from the snapshot stored with it, after the host
    /// restarted; `None` if the socket had none. Closes a socket without a snapshot or past
    /// its handshake deadline, and, of two sockets restored for the same peer, the one
    /// restored first.
    ///
    /// Emits no [`Effect::Deadline`]: the host set one when the socket was upgraded.
    pub fn restore(
        &mut self,
        key: K,
        snapshot: Option<SessionSnapshot>,
        now: Duration,
    ) -> Vec<Effect<K>> {
        let Some(snapshot) = snapshot else {
            return vec![Effect::Close(
                key,
                close_code::PROTOCOL_ERROR,
                "lost session",
            )];
        };
        let session = IngressSession::restore(snapshot, &self.cfg);
        if session.handshake_deadline().is_some_and(|d| d <= now) {
            self.log_handshake_timeout();
            return vec![Effect::Close(
                key,
                close_code::POLICY_VIOLATION,
                "handshake timeout",
            )];
        }
        let mut effects = Vec::new();
        let peer = session.peer();
        self.sessions.insert(key.clone(), session);
        if let Some(peer) = peer {
            // Two sockets of one peer: the host stopped between the steps of a supersede.
            if let Some(old) = self.peers.insert(peer, key.clone())
                && old != key
            {
                self.sessions.remove(&old);
                effects.push(Effect::Close(old, CLOSE_SUPERSEDED, "superseded"));
            }
            effects.push(Effect::PeerUp(peer, key));
        }
        effects
    }

    /// Handles a binary message from socket `key` at `now`.
    pub fn message(&mut self, key: &K, msg: &[u8], now: Duration) -> Vec<Effect<K>> {
        let mut effects = Vec::new();
        let Some(session) = self.sessions.get_mut(key) else {
            effects.push(Effect::Close(
                key.clone(),
                close_code::PROTOCOL_ERROR,
                "lost session",
            ));
            return effects;
        };
        let violations = session.dst_violations();
        let mut authed = None;
        let mut datagrams = Vec::new();
        match session.on_message(msg, now) {
            Ok(outputs) => {
                for out in outputs {
                    match out {
                        Output::Send(frame) => effects.push(Effect::Send(key.clone(), frame)),
                        Output::Authenticated { peer } => authed = Some(peer),
                        Output::Datagrams { from, batch } => datagrams.push(Effect::Datagrams {
                            from,
                            ecn: Ecn::to_bits(batch.ecn),
                            segment_size: batch.segment_size,
                            contents: batch.contents.to_vec(),
                        }),
                    }
                }
            }
            Err(close) => {
                self.close(key, &close, &mut effects);
                return effects;
            }
        }
        // The count lives in the snapshot, so a restart cannot reset it. Persisted only when
        // it changes, never per datagram.
        let count = session.dst_violations();
        if count != violations {
            warn!(
                event = "dst_violation",
                endpoint = %self.cfg.local_id.to_z32(),
                peer = %session.peer().map(|p| p.to_z32()).unwrap_or_default(),
                count,
            );
            if let Some(snap) = session.snapshot() {
                effects.push(Effect::Persist(key.clone(), snap));
            }
        }
        if let Some(peer) = authed {
            self.authenticated(key, peer, &mut effects);
        }
        effects.extend(datagrams);
        effects
    }

    /// Handles a text message from socket `key`, which the relay protocol does not allow.
    pub fn text(&mut self, key: &K) -> Vec<Effect<K>> {
        let mut effects = Vec::new();
        if let Some(session) = self.sessions.get_mut(key) {
            let close = session.on_text_message();
            self.close(key, &close, &mut effects);
        }
        effects
    }

    /// Forgets socket `key`, which the peer or the runtime closed.
    pub fn closed(&mut self, key: &K) -> Vec<Effect<K>> {
        let mut effects = Vec::new();
        self.forget(key, &mut effects);
        effects
    }

    /// Closes the sockets still unauthenticated at `now` past their handshake deadline, and
    /// returns the deadline of the earliest one left, if any.
    pub fn sweep(&mut self, now: Duration) -> Vec<Effect<K>> {
        let expired: Vec<K> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.handshake_deadline().is_some_and(|d| d <= now))
            .map(|(k, _)| k.clone())
            .collect();
        let mut effects = Vec::new();
        for key in expired {
            self.sessions.remove(&key);
            self.log_handshake_timeout();
            effects.push(Effect::Close(
                key,
                close_code::POLICY_VIOLATION,
                "handshake timeout",
            ));
        }
        if let Some(d) = self.next_deadline() {
            effects.push(Effect::Deadline(d));
        }
        effects
    }

    /// Forgets every socket, without effects; for an endpoint that is destroyed.
    pub fn clear(&mut self) {
        self.sessions.clear();
        self.peers.clear();
    }

    /// The peer of socket `key`, if it has authenticated.
    pub fn peer_of(&self, key: &K) -> Option<EndpointId> {
        self.sessions.get(key).and_then(|s| s.peer())
    }

    /// The socket `peer` is reached through.
    pub fn socket_of(&self, peer: &EndpointId) -> Option<&K> {
        self.peers.get(peer)
    }

    /// The peers with an authenticated socket.
    pub fn peers(&self) -> impl Iterator<Item = &EndpointId> {
        self.peers.keys()
    }

    /// The session of socket `key`.
    pub fn session(&self, key: &K) -> Option<&IngressSession> {
        self.sessions.get(key)
    }

    /// The number of sockets.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether there are no sockets.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// The earliest handshake deadline of an unauthenticated socket.
    pub fn next_deadline(&self) -> Option<Duration> {
        self.sessions
            .values()
            .filter_map(|s| s.handshake_deadline())
            .min()
    }

    /// Socket `key` authenticated as `peer`: supersedes the peer's older sockets.
    fn authenticated(&mut self, key: &K, peer: EndpointId, effects: &mut Vec<Effect<K>>) {
        let older: Vec<K> = self
            .sessions
            .iter()
            .filter(|(k, s)| *k != key && s.peer() == Some(peer))
            .map(|(k, _)| k.clone())
            .collect();
        let superseded = older.len();
        for old in older {
            if let Some(session) = self.sessions.remove(&old) {
                if let Some(f) = session.status_frame(Status::SameEndpointIdConnected) {
                    effects.push(Effect::Send(old.clone(), f));
                }
                effects.push(Effect::Close(old, CLOSE_SUPERSEDED, "superseded"));
            }
        }
        if let Some(snap) = self.sessions.get(key).and_then(|s| s.snapshot()) {
            effects.push(Effect::Persist(key.clone(), snap));
        }
        info!(
            event = "ingress_auth",
            endpoint = %self.cfg.local_id.to_z32(),
            ok = true,
            peer = %peer.to_z32(),
            superseded,
        );
        self.peers.insert(peer, key.clone());
        effects.push(Effect::PeerUp(peer, key.clone()));
    }

    /// Closes socket `key` as the session decided; before authentication that is a failed
    /// handshake, logged as `ingress_auth`.
    fn close(&mut self, key: &K, close: &Close, effects: &mut Vec<Effect<K>>) {
        let endpoint = self.cfg.local_id.to_z32();
        match self.routed_peer(key) {
            None => warn!(
                event = "ingress_auth",
                endpoint,
                ok = false,
                code = close.code,
                reason = close.reason_text(),
            ),
            Some(peer) => info!(
                event = "socket_close",
                endpoint,
                peer = %peer.to_z32(),
                by = "object",
                code = close.code,
                reason = close.reason_text(),
            ),
        }
        if let Some(f) = &close.frame {
            effects.push(Effect::Send(key.clone(), f.clone()));
        }
        effects.push(Effect::Close(key.clone(), close.code, close.reason_text()));
        self.forget(key, effects);
    }

    fn forget(&mut self, key: &K, effects: &mut Vec<Effect<K>>) {
        if self.sessions.remove(key).is_none() {
            return;
        }
        if let Some(peer) = self.routed_peer(key) {
            self.peers.remove(&peer);
            effects.push(Effect::PeerDown(peer));
        }
    }

    /// The peer routed to socket `key`. Looked up in the routes rather than the session,
    /// which no longer reports its peer once it has closed.
    fn routed_peer(&self, key: &K) -> Option<EndpointId> {
        self.peers
            .iter()
            .find_map(|(peer, k)| (k == key).then_some(*peer))
    }

    fn log_handshake_timeout(&self) {
        warn!(
            event = "ingress_auth",
            endpoint = %self.cfg.local_id.to_z32(),
            ok = false,
            reason = "handshake timeout",
        );
    }
}
