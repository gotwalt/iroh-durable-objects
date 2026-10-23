//! Sans-io server side of the iroh relay protocol, for an endpoint that serves
//! its own relay URL.
//!
//! A peer that has this endpoint's relay URL connects to it with the stock iroh
//! relay client, authenticates with the challenge handshake, and sends
//! datagrams addressed to this endpoint. This crate decodes those messages,
//! verifies the handshake, and encodes the server's replies. It speaks the wire
//! format of iroh-relay 1.4.0, protocol versions `iroh-relay-v1` and
//! `iroh-relay-v2`.
//!
//! It is not a relay server. A session only accepts datagrams addressed to the
//! one endpoint it serves ([`IngressConfig::local_id`]); there is no
//! forwarding path between peers. The crate does no I/O and keeps no timers:
//! the host owns the WebSocket and the clock. It depends on neither tokio nor
//! hyper and builds for `wasm32-unknown-unknown`.
//!
//! # Call sequence
//!
//! The host creates one [`IngressSession`] per WebSocket and drives it:
//!
//! 1. On the HTTP upgrade request, [`IngressSession::on_upgrade`] checks the
//!    method and path, negotiates the protocol version, and draws a challenge
//!    from the host's CSPRNG. On [`Reject`] the host answers with
//!    [`Reject::http_status`]. Otherwise it answers 101 with
//!    [`UpgradeResponse::subprotocol_header`], which the client requires, and
//!    writes [`UpgradeResponse::challenge`] as the first binary message.
//! 2. For each binary message, [`IngressSession::on_message`] returns the
//!    [`Output`]s to act on: frames to write back ([`Output::Send`]), then
//!    [`Output::Authenticated`] once the handshake completes, then
//!    [`Output::Datagrams`] for each datagram frame addressed to this endpoint.
//!    On [`Close`] the host writes [`Close::frame`], if any, and closes the
//!    socket with [`Close::code`]. A text message is handled with
//!    [`IngressSession::on_text_message`].
//! 3. To send datagrams from this endpoint to the peer,
//!    [`IngressSession::encode_datagrams`] produces the frame to write.
//! 4. Before the host is evicted from memory, [`IngressSession::snapshot`]
//!    yields a small serializable [`SessionSnapshot`];
//!    [`IngressSession::restore`] rebuilds the session from it. An
//!    authenticated session resumes without a new handshake.
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use iroh_relay_ingress::{
//!     DatagramBatch, EndpointId, IngressConfig, IngressSession, Output, UpgradeRequest,
//! };
//!
//! # trait Socket {
//! #     fn send_binary(&mut self, bytes: &[u8]);
//! #     fn close(&mut self, code: u16, reason: &str);
//! # }
//! fn serve(
//!     local_id: EndpointId,
//!     req: &UpgradeRequest<'_>,
//!     ws: &mut impl Socket,
//!     messages: &[Vec<u8>],
//!     now: Duration,
//! ) -> Result<(), u16> {
//!     let cfg = IngressConfig::new(local_id);
//!     let (resp, mut session) =
//!         IngressSession::on_upgrade(req, &cfg, &mut rand::rng(), now)
//!             .map_err(|reject| reject.http_status())?;
//!     // Respond 101 with `resp.subprotocol_header()`, then:
//!     ws.send_binary(resp.challenge.as_bytes());
//!
//!     for msg in messages {
//!         match session.on_message(msg, now) {
//!             Ok(outputs) => {
//!                 for output in outputs {
//!                     match output {
//!                         Output::Send(frame) => ws.send_binary(frame.as_bytes()),
//!                         // Index the socket by `peer`.
//!                         Output::Authenticated { peer: _ } => {}
//!                         // Hand `batch` to the local endpoint as received from `from`.
//!                         Output::Datagrams { from: _, batch: _ } => {}
//!                     }
//!                 }
//!             }
//!             Err(close) => {
//!                 if let Some(frame) = &close.frame {
//!                     ws.send_binary(frame.as_bytes());
//!                 }
//!                 ws.close(close.code, close.reason_text());
//!                 return Ok(());
//!             }
//!         }
//!     }
//!
//!     let reply = DatagramBatch { ecn: None, segment_size: None, contents: b"datagram" };
//!     if let Ok(frame) = session.encode_datagrams(&reply) {
//!         ws.send_binary(frame.as_bytes());
//!     }
//!
//!     let snapshot = session.snapshot().expect("session is open");
//!     let _session = IngressSession::restore(snapshot, &cfg);
//!     Ok(())
//! }
//! ```
//!
//! # Time
//!
//! `now` is a [`Duration`](std::time::Duration) since an epoch the host
//! chooses, which must stay the same across snapshot and restore; wall-clock
//! time since the UNIX epoch works. It is only used for the handshake
//! deadline, and only checked when a message arrives. The host closes sockets
//! that are still unauthenticated at [`IngressSession::handshake_deadline`].
//! The session never sends pings; it answers the client's.
//!
//! # Guarantees
//!
//! - [`Output::Datagrams`] is only emitted after a valid challenge signature,
//!   and only for frames whose destination is [`IngressConfig::local_id`].
//!   Other datagrams are dropped and counted; the session closes once the
//!   count exceeds [`IngressConfig::dst_violation_limit`]. Datagram frames
//!   with empty contents are dropped.
//! - Before authentication only `ClientAuth` is accepted. After it, only
//!   datagram, `Ping` and `Pong` frames are. Any other frame, an unknown frame
//!   type, a malformed frame, or a payload over [`MAX_PACKET_SIZE`] closes the
//!   session.
//! - Nothing but the handshake is sent before authentication.
//!   [`IngressSession::encode_datagrams`] and [`IngressSession::status_frame`]
//!   refuse to encode for an unauthenticated session, and `status_frame`
//!   encodes `Status` for `iroh-relay-v2` and `Health` for `iroh-relay-v1`.
//!
//! # Host responsibilities
//!
//! The WebSocket handshake itself (`Upgrade`, `Sec-WebSocket-Key`,
//! `Sec-WebSocket-Version`) is left to the host. So is keeping one socket per
//! peer: like upstream, the session does not close a superseded socket. The
//! host sends it [`IngressSession::status_frame`] with
//! [`Status::SameEndpointIdConnected`] and routes to the newer one.
//!
//! [`Output::Datagrams`] borrows from the message passed to
//! [`IngressSession::on_message`].
//!
//! # Limitations
//!
//! Authentication always uses the challenge. The TLS-exporter header
//! ([`CLIENT_AUTH_HEADER`]) is ignored because TLS terminates before the host,
//! and admission tokens (`Authorization`) are ignored because iroh-relay 1.4.0
//! defines no token format.
//!
//! iroh-relay keeps its codec `pub(crate)` and depends on tokio, so the wire
//! format is reimplemented here. `tests/conformance.rs` checks it against the
//! stock iroh-relay 1.4.0 client and a stock iroh 1.4.0 `Endpoint`.
#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod proto;
mod session;
mod table;

pub use iroh_base::EndpointId;

pub use crate::{
    proto::{
        CHALLENGE_LEN, CLIENT_AUTH_HEADER, CLIENT_AUTH_LEN, DOMAIN_SEP_CHALLENGE, DatagramBatch,
        Ecn, EncodeError, Frame, FrameType, KEY_LEN, MAX_BATCH_CONTENTS, MAX_DATAGRAM_CONTENTS,
        MAX_PACKET_SIZE, PING_LEN, ProtocolVersion, RELAY_PATH, SEC_WEBSOCKET_PROTOCOL, Status,
        encode_datagrams, encode_status,
    },
    session::{
        Close, CloseReason, DEFAULT_DST_VIOLATION_LIMIT, HANDSHAKE_TIMEOUT, IngressConfig,
        IngressSession, Output, Outputs, Reject, SessionSnapshot, UpgradeRequest, UpgradeResponse,
        close_code,
    },
    table::{CLOSE_SUPERSEDED, Effect, Sockets, Upgraded},
};
