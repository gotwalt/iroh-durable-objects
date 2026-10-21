//! Per-WebSocket session state machine.

use std::time::Duration;

use iroh_base::{EndpointId, PublicKey, Signature};
use n0_error::{e, stack_error};
use rand_core::CryptoRng;
use serde::{Deserialize, Serialize};
use tracing::{Span, debug, debug_span, trace, warn};

use crate::proto::{
    self, CHALLENGE_LEN, ClientFrame, DatagramBatch, DecodeError, EncodeError, Frame, FrameType,
    KEY_LEN, ProtocolVersion, RELAY_PATH, Status,
};

/// Time from the upgrade until a valid `ClientAuth` must have arrived.
///
/// Matches upstream's `ESTABLISH_TIMEOUT`, iroh-relay 1.4.0,
/// `src/server/http_server.rs`.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default number of datagrams addressed to another endpoint that a session
/// drops before it closes.
///
/// A stock client only sends to the endpoint whose relay URL it dialed, so
/// this only bounds misbehaving peers.
pub const DEFAULT_DST_VIOLATION_LIMIT: u32 = 32;

/// Denial reason upstream sends for a bad challenge signature.
///
/// iroh-relay 1.4.0, `src/protos/handshake.rs`.
const DENY_SIGNATURE_INVALID: &str = "signature invalid";

/// WebSocket close codes used by [`Close`] (RFC 6455, section 7.4.1).
pub mod close_code {
    /// Protocol error: a malformed or unexpected frame.
    pub const PROTOCOL_ERROR: u16 = 1002;
    /// Unsupported data: a text message.
    pub const UNSUPPORTED_DATA: u16 = 1003;
    /// Policy violation: failed authentication, handshake timeout, or too many
    /// datagrams for another endpoint.
    pub const POLICY_VIOLATION: u16 = 1008;
}

/// Configuration shared by all sessions of one endpoint.
#[derive(Debug, Clone)]
pub struct IngressConfig {
    /// The endpoint the sessions serve. Datagrams addressed to any other
    /// endpoint are dropped.
    pub local_id: EndpointId,
    /// Number of datagrams addressed to another endpoint a session drops
    /// before it closes.
    pub dst_violation_limit: u32,
}

impl IngressConfig {
    /// Creates a configuration for `local_id` with default limits.
    pub fn new(local_id: EndpointId) -> Self {
        Self {
            local_id,
            dst_violation_limit: DEFAULT_DST_VIOLATION_LIMIT,
        }
    }
}

/// The parts of the HTTP upgrade request a session needs.
///
/// The WebSocket handshake headers (`Upgrade`, `Sec-WebSocket-Key`,
/// `Sec-WebSocket-Version`) are checked by the host.
#[derive(Debug, Clone, Copy)]
pub struct UpgradeRequest<'a> {
    /// HTTP method; must be `GET`.
    pub method: &'a str,
    /// Request path. A query string is ignored; the stock client appends
    /// `?token=` when built for wasm.
    pub path: &'a str,
    /// Value of the `Sec-WebSocket-Protocol` request header, if present.
    pub sec_websocket_protocol: Option<&'a str>,
}

/// Data the host needs to complete an accepted upgrade.
#[derive(Debug)]
pub struct UpgradeResponse {
    /// Negotiated version.
    ///
    /// The 101 response must carry exactly this version in
    /// `Sec-WebSocket-Protocol`, or the client fails with `BadVersionHeader`.
    pub version: ProtocolVersion,
    /// The `ServerChallenge` frame, to be written as the first message on the
    /// socket.
    pub challenge: Frame,
}

impl UpgradeResponse {
    /// Returns the `Sec-WebSocket-Protocol` header name and value for the 101
    /// response.
    pub fn subprotocol_header(&self) -> (&'static str, &'static str) {
        (proto::SEC_WEBSOCKET_PROTOCOL, self.version.as_str())
    }
}

/// Reason an upgrade request was rejected.
///
/// The host responds with [`Reject::http_status`] instead of upgrading.
#[stack_error(derive, add_meta)]
#[non_exhaustive]
pub enum Reject {
    /// The path is not [`RELAY_PATH`].
    #[error("not found: {path}")]
    NotFound {
        /// The requested path.
        path: String,
    },
    /// The method is not `GET`.
    #[error("method not allowed: {method}")]
    MethodNotAllowed {
        /// The request method.
        method: String,
    },
    /// No supported protocol version was offered.
    #[error("unsupported relay protocol version, offered: {offered:?}")]
    UnsupportedVersion {
        /// The offered `Sec-WebSocket-Protocol` value.
        offered: Option<String>,
    },
}

impl Reject {
    /// Returns the HTTP status upstream uses for this rejection.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::NotFound { .. } => 404,
            Self::MethodNotAllowed { .. } => 405,
            Self::UnsupportedVersion { .. } => 400,
        }
    }
}

/// Why a session is being closed.
#[stack_error(derive, add_meta)]
#[non_exhaustive]
pub enum CloseReason {
    /// No valid `ClientAuth` within [`HANDSHAKE_TIMEOUT`].
    #[error("handshake timed out")]
    HandshakeTimeout {},
    /// Challenge signature did not verify.
    #[error("client auth signature invalid")]
    SignatureInvalid {},
    /// `ClientAuth` carried bytes that are not a valid ed25519 public key.
    #[error("client auth public key invalid")]
    InvalidPublicKey {},
    /// A frame type the client may not send in the session's current state.
    #[error("unexpected frame {frame_type:?} (authenticated: {authenticated})")]
    UnexpectedFrame {
        /// The frame type received.
        frame_type: FrameType,
        /// Whether the handshake had completed.
        authenticated: bool,
    },
    /// A frame type that is not part of the protocol.
    #[error("unknown frame type {tag}")]
    UnknownFrameType {
        /// The varint tag.
        tag: u64,
    },
    /// A frame longer than [`proto::MAX_PACKET_SIZE`] after its type.
    #[error("frame too large: {len} bytes")]
    FrameTooLarge {
        /// Bytes after the frame type.
        len: usize,
    },
    /// A frame of known type with an invalid layout.
    #[error("invalid {frame_type:?} frame")]
    InvalidFrame {
        /// The frame type.
        frame_type: FrameType,
    },
    /// An empty binary message.
    #[error("empty message")]
    EmptyMessage {},
    /// A text message. The protocol uses binary messages only.
    #[error("text message")]
    TextMessage {},
    /// Too many datagrams addressed to another endpoint.
    #[error("too many destination violations: {count}")]
    DestinationViolations {
        /// Datagrams dropped so far.
        count: u32,
    },
    /// A message arrived after the session closed.
    #[error("session already closed")]
    AlreadyClosed {},
}

/// Instruction to close the WebSocket.
///
/// The host writes `frame`, if any, then closes the socket with `code`.
#[derive(Debug)]
pub struct Close {
    /// WebSocket close code, one of [`close_code`].
    pub code: u16,
    /// Frame to write before closing: `ServerDeniesAuth` after a bad signature.
    pub frame: Option<Frame>,
    /// Why the session is closing.
    pub reason: CloseReason,
}

impl Close {
    fn new(reason: CloseReason) -> Self {
        let code = match reason {
            CloseReason::HandshakeTimeout { .. }
            | CloseReason::SignatureInvalid { .. }
            | CloseReason::DestinationViolations { .. } => close_code::POLICY_VIOLATION,
            CloseReason::TextMessage { .. } => close_code::UNSUPPORTED_DATA,
            _ => close_code::PROTOCOL_ERROR,
        };
        Self {
            code,
            frame: None,
            reason,
        }
    }

    /// Returns reason text for the WebSocket close frame, within its 123-byte
    /// limit.
    pub fn reason_text(&self) -> &'static str {
        match self.reason {
            CloseReason::HandshakeTimeout { .. } => "handshake timeout",
            CloseReason::SignatureInvalid { .. } => "signature invalid",
            CloseReason::InvalidPublicKey { .. } => "invalid public key",
            CloseReason::UnexpectedFrame { .. } => "unexpected frame",
            CloseReason::UnknownFrameType { .. } => "unknown frame type",
            CloseReason::FrameTooLarge { .. } => "frame too large",
            CloseReason::InvalidFrame { .. } => "invalid frame",
            CloseReason::EmptyMessage { .. } => "empty message",
            CloseReason::TextMessage { .. } => "text message",
            CloseReason::DestinationViolations { .. } => "destination not served here",
            CloseReason::AlreadyClosed { .. } => "closed",
        }
    }
}

impl std::fmt::Display for Close {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "close {}: {}", self.code, self.reason)
    }
}

impl std::error::Error for Close {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.reason)
    }
}

/// An action the host takes after a client message.
#[derive(Debug)]
pub enum Output<'a> {
    /// Write this frame to the socket: `ServerConfirmsAuth` or `Pong`.
    Send(Frame),
    /// The handshake completed and the socket belongs to `peer`.
    ///
    /// Emitted once, after the `Send` carrying `ServerConfirmsAuth`.
    Authenticated {
        /// The authenticated client.
        peer: EndpointId,
    },
    /// Datagrams from the authenticated peer, addressed to the local endpoint.
    ///
    /// Borrows from the message passed to [`IngressSession::on_message`].
    Datagrams {
        /// The sender, which is the authenticated peer.
        from: EndpointId,
        /// The datagrams, with ECN and segment size as received.
        batch: DatagramBatch<'a>,
    },
}

/// Iterator over the outputs of one [`IngressSession::on_message`] call, in
/// order. There are at most two.
#[derive(Debug, Default)]
pub struct Outputs<'a> {
    items: [Option<Output<'a>>; 2],
    next: usize,
}

impl<'a> Outputs<'a> {
    fn none() -> Self {
        Self::default()
    }

    fn one(o: Output<'a>) -> Self {
        Self {
            items: [Some(o), None],
            next: 0,
        }
    }

    fn two(a: Output<'a>, b: Output<'a>) -> Self {
        Self {
            items: [Some(a), Some(b)],
            next: 0,
        }
    }

    /// Returns whether no outputs remain.
    pub fn is_empty(&self) -> bool {
        self.items[self.next.min(2)..].iter().all(Option::is_none)
    }
}

impl<'a> Iterator for Outputs<'a> {
    type Item = Output<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next < self.items.len() {
            let i = self.next;
            self.next += 1;
            if let Some(o) = self.items[i].take() {
                return Some(o);
            }
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    AwaitingAuth {
        challenge: [u8; CHALLENGE_LEN],
        /// Absolute deadline in host time.
        deadline: Duration,
    },
    Authenticated {
        peer: EndpointId,
    },
    Closed,
}

/// Server side of one relay WebSocket.
///
/// See the [crate documentation](crate) for the call sequence and for the
/// meaning of `now`.
#[derive(Debug)]
pub struct IngressSession {
    local_id: EndpointId,
    version: ProtocolVersion,
    state: State,
    dst_violations: u32,
    dst_violation_limit: u32,
    span: Span,
}

impl IngressSession {
    /// Accepts an upgrade request and creates a session awaiting `ClientAuth`.
    ///
    /// Checks the method and path, negotiates the protocol version, and draws
    /// the challenge from `rng`. The handshake deadline is `now` plus
    /// [`HANDSHAKE_TIMEOUT`]. The `Authorization` and
    /// [`proto::CLIENT_AUTH_HEADER`] headers are not consulted.
    ///
    /// # Errors
    ///
    /// Returns [`Reject`] for a method other than `GET`, a path other than
    /// [`RELAY_PATH`], or an offer with no supported protocol version.
    pub fn on_upgrade<R: CryptoRng + ?Sized>(
        req: &UpgradeRequest<'_>,
        cfg: &IngressConfig,
        rng: &mut R,
        now: Duration,
    ) -> Result<(UpgradeResponse, Self), Reject> {
        if !req.method.eq_ignore_ascii_case("GET") {
            return Err(e!(Reject::MethodNotAllowed {
                method: req.method.to_owned()
            }));
        }
        let path = req.path.split('?').next().unwrap_or_default();
        if path != RELAY_PATH {
            return Err(e!(Reject::NotFound {
                path: path.to_owned()
            }));
        }
        let version = req
            .sec_websocket_protocol
            .and_then(ProtocolVersion::negotiate)
            .ok_or_else(|| {
                e!(Reject::UnsupportedVersion {
                    offered: req.sec_websocket_protocol.map(str::to_owned)
                })
            })?;

        let mut challenge = [0u8; CHALLENGE_LEN];
        rng.fill_bytes(&mut challenge);

        let span = debug_span!(
            "relay-ingress",
            version = version.as_str(),
            peer = tracing::field::Empty
        );
        let session = Self {
            local_id: cfg.local_id,
            version,
            state: State::AwaitingAuth {
                challenge,
                deadline: now.saturating_add(HANDSHAKE_TIMEOUT),
            },
            dst_violations: 0,
            dst_violation_limit: cfg.dst_violation_limit,
            span,
        };
        session
            .span
            .in_scope(|| debug!("upgrade accepted, sending challenge"));
        let resp = UpgradeResponse {
            version,
            challenge: proto::encode_server_challenge(&challenge),
        };
        Ok((resp, session))
    }

    /// Processes one binary WebSocket message.
    ///
    /// # Errors
    ///
    /// Returns [`Close`] if the socket must be closed. The session is closed
    /// afterwards and rejects further messages.
    pub fn on_message<'a>(&mut self, msg: &'a [u8], now: Duration) -> Result<Outputs<'a>, Close> {
        let span = self.span.clone();
        let _guard = span.enter();
        let res = self.on_message_inner(msg, now);
        if let Err(close) = &res {
            debug!(code = close.code, reason = %close.reason, "closing session");
            self.state = State::Closed;
        }
        res
    }

    fn on_message_inner<'a>(&mut self, msg: &'a [u8], now: Duration) -> Result<Outputs<'a>, Close> {
        let authenticated = match &self.state {
            State::Closed => return Err(Close::new(e!(CloseReason::AlreadyClosed))),
            State::AwaitingAuth { deadline, .. } => {
                if now > *deadline {
                    return Err(Close::new(e!(CloseReason::HandshakeTimeout)));
                }
                false
            }
            State::Authenticated { .. } => true,
        };
        let frame = proto::decode_client_frame(msg).map_err(|err| {
            Close::new(match err {
                DecodeError::Empty => e!(CloseReason::EmptyMessage),
                DecodeError::UnknownFrameType(tag) => e!(CloseReason::UnknownFrameType { tag }),
                DecodeError::TooLarge(len) => e!(CloseReason::FrameTooLarge { len }),
                DecodeError::Invalid(frame_type) => e!(CloseReason::InvalidFrame { frame_type }),
            })
        })?;
        trace!(?frame, "frame");
        match (&self.state, frame) {
            (
                State::AwaitingAuth { challenge, .. },
                ClientFrame::ClientAuth {
                    public_key,
                    signature,
                },
            ) => {
                let peer = PublicKey::from_bytes(&public_key)
                    .map_err(|_| Close::new(e!(CloseReason::InvalidPublicKey)))?;
                let message = proto::challenge_message(challenge);
                // `verify` is strict ed25519 verification, as upstream uses.
                if peer
                    .verify(&message, &Signature::from_bytes(&signature))
                    .is_err()
                {
                    warn!(peer = %peer.fmt_short(), "client auth signature invalid");
                    let mut close = Close::new(e!(CloseReason::SignatureInvalid));
                    close.frame = Some(proto::encode_server_denies_auth(DENY_SIGNATURE_INVALID));
                    return Err(close);
                }
                self.state = State::Authenticated { peer };
                self.span
                    .record("peer", tracing::field::display(peer.fmt_short()));
                debug!("authenticated");
                Ok(Outputs::two(
                    Output::Send(proto::encode_server_confirms_auth()),
                    Output::Authenticated { peer },
                ))
            }
            (State::Authenticated { peer }, ClientFrame::Datagrams { dst, batch }) => {
                if dst != self.local_id.as_bytes() {
                    // There is nowhere to forward it. Upstream also drops
                    // datagrams for unknown destinations without telling the
                    // sender.
                    self.dst_violations = self.dst_violations.saturating_add(1);
                    debug!(
                        count = self.dst_violations,
                        "dropping datagram for foreign destination"
                    );
                    if self.dst_violations > self.dst_violation_limit {
                        return Err(Close::new(e!(CloseReason::DestinationViolations {
                            count: self.dst_violations
                        })));
                    }
                    return Ok(Outputs::none());
                }
                if batch.contents.is_empty() {
                    return Ok(Outputs::none());
                }
                Ok(Outputs::one(Output::Datagrams { from: *peer, batch }))
            }
            (State::Authenticated { .. }, ClientFrame::Ping(data)) => {
                Ok(Outputs::one(Output::Send(proto::encode_pong(&data))))
            }
            // The session never pings, so any Pong is unsolicited. Upstream
            // ignores Pongs that match no ping.
            (State::Authenticated { .. }, ClientFrame::Pong(_)) => Ok(Outputs::none()),
            (_, frame) => {
                let frame_type = match frame {
                    ClientFrame::ClientAuth { .. } => FrameType::ClientAuth,
                    ClientFrame::Datagrams { batch, .. } => {
                        if batch.segment_size.is_some() {
                            FrameType::ClientToRelayDatagramBatch
                        } else {
                            FrameType::ClientToRelayDatagram
                        }
                    }
                    ClientFrame::Ping(_) => FrameType::Ping,
                    ClientFrame::Pong(_) => FrameType::Pong,
                    ClientFrame::Unexpected(t) => t,
                };
                Err(Close::new(e!(CloseReason::UnexpectedFrame {
                    frame_type,
                    authenticated
                })))
            }
        }
    }

    /// Closes the session after a text WebSocket message.
    pub fn on_text_message(&mut self) -> Close {
        self.state = State::Closed;
        Close::new(e!(CloseReason::TextMessage))
    }

    /// Encodes datagrams from the local endpoint to the peer.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::NotAuthenticated`] before the handshake
    /// completes, and otherwise the errors of [`proto::encode_datagrams`].
    pub fn encode_datagrams(&self, batch: &DatagramBatch<'_>) -> Result<Frame, EncodeError> {
        if !matches!(self.state, State::Authenticated { .. }) {
            return Err(e!(EncodeError::NotAuthenticated));
        }
        proto::encode_datagrams(&self.local_id, batch)
    }

    /// Encodes a status frame for the negotiated version.
    ///
    /// The host sends [`Status::SameEndpointIdConnected`] to a socket
    /// superseded by a newer one from the same peer. Returns `None` before
    /// authentication, when the client would reject the frame.
    pub fn status_frame(&self, status: Status) -> Option<Frame> {
        matches!(self.state, State::Authenticated { .. })
            .then(|| proto::encode_status(self.version, status))
    }

    /// Returns the authenticated peer, if any.
    pub fn peer(&self) -> Option<EndpointId> {
        match self.state {
            State::Authenticated { peer } => Some(peer),
            _ => None,
        }
    }

    /// Returns whether the handshake has completed and the session is open.
    pub fn is_authenticated(&self) -> bool {
        matches!(self.state, State::Authenticated { .. })
    }

    /// Returns whether the session is closed.
    pub fn is_closed(&self) -> bool {
        matches!(self.state, State::Closed)
    }

    /// Returns the negotiated protocol version.
    pub fn version(&self) -> ProtocolVersion {
        self.version
    }

    /// Returns the EndpointId of the endpoint this session serves.
    pub fn local_id(&self) -> EndpointId {
        self.local_id
    }

    /// Returns the number of datagrams dropped for another destination.
    pub fn dst_violations(&self) -> u32 {
        self.dst_violations
    }

    /// Returns the time until which a `ClientAuth` is accepted.
    ///
    /// `None` once authenticated or closed. The session only checks the
    /// deadline when a message arrives, so the host closes a socket that is
    /// still unauthenticated at this time, with
    /// [`close_code::POLICY_VIOLATION`].
    pub fn handshake_deadline(&self) -> Option<Duration> {
        match self.state {
            State::AwaitingAuth { deadline, .. } => Some(deadline),
            _ => None,
        }
    }

    /// Returns the session state to persist across eviction from memory.
    ///
    /// `None` for a closed session.
    pub fn snapshot(&self) -> Option<SessionSnapshot> {
        let state = match &self.state {
            State::AwaitingAuth {
                challenge,
                deadline,
            } => SnapshotState::AwaitingAuth {
                challenge: *challenge,
                deadline_ms: u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
            },
            State::Authenticated { peer } => SnapshotState::Authenticated { peer: *peer },
            State::Closed => return None,
        };
        Some(SessionSnapshot {
            version: self.version,
            state,
            dst_violations: self.dst_violations,
        })
    }

    /// Rebuilds a session from a snapshot.
    ///
    /// A session restored from an authenticated snapshot accepts datagrams and
    /// pings immediately. `cfg` supplies the local EndpointId and limits,
    /// which the snapshot does not store.
    pub fn restore(snapshot: SessionSnapshot, cfg: &IngressConfig) -> Self {
        let span = debug_span!(
            "relay-ingress",
            version = snapshot.version.as_str(),
            peer = tracing::field::Empty
        );
        let state = match snapshot.state {
            SnapshotState::AwaitingAuth {
                challenge,
                deadline_ms,
            } => State::AwaitingAuth {
                challenge,
                deadline: Duration::from_millis(deadline_ms),
            },
            SnapshotState::Authenticated { peer } => {
                span.record("peer", tracing::field::display(peer.fmt_short()));
                State::Authenticated { peer }
            }
        };
        Self {
            local_id: cfg.local_id,
            version: snapshot.version,
            state,
            dst_violations: snapshot.dst_violations,
            dst_violation_limit: cfg.dst_violation_limit,
            span,
        }
    }
}

/// Serializable session state, produced by [`IngressSession::snapshot`].
///
/// An authenticated snapshot is about 100 bytes as JSON and 35 as postcard
/// (`tests/snapshot.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    #[serde(rename = "v")]
    version: ProtocolVersion,
    #[serde(rename = "s")]
    state: SnapshotState,
    #[serde(rename = "dv", default)]
    dst_violations: u32,
}

impl SessionSnapshot {
    /// Returns the authenticated peer, if any.
    ///
    /// Lets a host index sockets by peer without restoring their sessions.
    pub fn peer(&self) -> Option<EndpointId> {
        match self.state {
            SnapshotState::Authenticated { peer } => Some(peer),
            SnapshotState::AwaitingAuth { .. } => None,
        }
    }

    /// Returns the negotiated protocol version.
    pub fn version(&self) -> ProtocolVersion {
        self.version
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum SnapshotState {
    #[serde(rename = "await")]
    AwaitingAuth {
        #[serde(rename = "c")]
        challenge: [u8; CHALLENGE_LEN],
        #[serde(rename = "d")]
        deadline_ms: u64,
    },
    #[serde(rename = "auth")]
    Authenticated {
        /// Human-readable formats store the key's hex `Display` form.
        #[serde(rename = "p")]
        peer: EndpointId,
    },
}

const _: () = assert!(KEY_LEN == EndpointId::LENGTH);
