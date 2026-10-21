//! Wire constants and the server side of the relay frame codec.
//!
//! Each constant cites its upstream definition.

use std::num::NonZeroU16;

use iroh_base::EndpointId;
use n0_error::{e, stack_error};
use serde::{Deserialize, Serialize};

/// HTTP path the relay client connects to.
///
/// The client replaces the relay URL's path with it, so a relay URL with any
/// path is dialed here.
///
/// iroh-relay 1.4.0, `src/http.rs`.
pub const RELAY_PATH: &str = "/relay";

/// Header carrying the offered protocol versions in the upgrade request and the
/// selected one in the 101 response.
pub const SEC_WEBSOCKET_PROTOCOL: &str = "Sec-WebSocket-Protocol";

/// Header carrying the client's TLS-exporter authentication.
///
/// Ignored: TLS terminates before the host, so the keying material the client
/// signed is not available. Clients fall back to the challenge.
///
/// iroh-relay 1.4.0, `src/http.rs`.
pub const CLIENT_AUTH_HEADER: &str = "x-iroh-relay-client-auth-v1";

/// Maximum length of a frame after its type, in either direction.
///
/// iroh-relay 1.4.0, `src/protos/relay.rs`.
pub const MAX_PACKET_SIZE: usize = 64 * 1024;

/// Length of the server challenge, in bytes.
///
/// iroh-relay 1.4.0, `src/protos/handshake.rs`.
pub const CHALLENGE_LEN: usize = 16;

/// Key derivation context for the challenge signature.
///
/// The client signs `blake3::derive_key(DOMAIN_SEP_CHALLENGE, challenge)`.
///
/// iroh-relay 1.4.0, `src/protos/handshake.rs`.
pub const DOMAIN_SEP_CHALLENGE: &str = "iroh-relay handshake v1 challenge signature";

/// Length of a `Ping` or `Pong` payload.
///
/// iroh-relay 1.4.0, `src/protos/relay.rs`.
pub const PING_LEN: usize = 8;

/// Length of an ed25519 public key on the wire.
///
/// iroh-base 1.4.0, `src/key.rs`.
pub const KEY_LEN: usize = 32;

/// Length of an ed25519 signature on the wire.
pub(crate) const SIG_LEN: usize = 64;

/// Exact length of a `ClientAuth` message: `01 ‖ key[32] ‖ 40 ‖ sig[64]`.
///
/// The body is the postcard encoding of `ClientAuth`. The public key
/// serializes as 32 raw bytes; the signature is `serde_bytes`, so it carries a
/// one-byte varint length (`0x40`).
///
/// iroh-relay 1.4.0, `src/protos/handshake.rs`.
pub const CLIENT_AUTH_LEN: usize = 1 + KEY_LEN + 1 + SIG_LEN;

/// Largest `contents` length of a single-datagram frame.
///
/// After the type byte the frame holds the source key, the ECN byte and the
/// contents, which together must fit in [`MAX_PACKET_SIZE`].
pub const MAX_DATAGRAM_CONTENTS: usize = MAX_PACKET_SIZE - KEY_LEN - 1;

/// Largest `contents` length of a batch frame, which also carries a two-byte
/// segment size.
pub const MAX_BATCH_CONTENTS: usize = MAX_PACKET_SIZE - KEY_LEN - 1 - 2;

/// Relay frame type, the QUIC varint that starts every binary message.
///
/// iroh-relay 1.4.0, `src/protos/common.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum FrameType {
    /// Server to client, handshake.
    ServerChallenge = 0,
    /// Client to server, handshake.
    ClientAuth = 1,
    /// Server to client, handshake.
    ServerConfirmsAuth = 2,
    /// Server to client, handshake.
    ServerDeniesAuth = 3,
    /// Client to server, one datagram.
    ClientToRelayDatagram = 4,
    /// Client to server, datagrams of a common segment size.
    ClientToRelayDatagramBatch = 5,
    /// Server to client, one datagram.
    RelayToClientDatagram = 6,
    /// Server to client, datagrams of a common segment size.
    RelayToClientDatagramBatch = 7,
    /// Server to client: a peer that sent to this client has disconnected.
    EndpointGone = 8,
    /// Either direction.
    Ping = 9,
    /// Either direction.
    Pong = 10,
    /// Server to client, `iroh-relay-v1` only.
    Health = 11,
    /// Server to client: the relay is restarting.
    Restarting = 12,
    /// Server to client, `iroh-relay-v2` only.
    Status = 13,
}

impl FrameType {
    fn from_u64(v: u64) -> Option<Self> {
        Some(match v {
            0 => Self::ServerChallenge,
            1 => Self::ClientAuth,
            2 => Self::ServerConfirmsAuth,
            3 => Self::ServerDeniesAuth,
            4 => Self::ClientToRelayDatagram,
            5 => Self::ClientToRelayDatagramBatch,
            6 => Self::RelayToClientDatagram,
            7 => Self::RelayToClientDatagramBatch,
            8 => Self::EndpointGone,
            9 => Self::Ping,
            10 => Self::Pong,
            11 => Self::Health,
            12 => Self::Restarting,
            13 => Self::Status,
            _ => return None,
        })
    }
}

/// Relay protocol version, negotiated via [`SEC_WEBSOCKET_PROTOCOL`].
///
/// Ordered so that `V2 > V1`; the server picks the highest version offered.
///
/// iroh-relay 1.4.0, `src/http.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ProtocolVersion {
    /// `iroh-relay-v1`, spoken by iroh before 0.98. Uses `Health`, never `Status`.
    #[serde(rename = "v1")]
    V1,
    /// `iroh-relay-v2`, spoken by iroh 0.98 and later. Uses `Status`, never `Health`.
    #[serde(rename = "v2")]
    V2,
}

impl ProtocolVersion {
    /// Returns the subprotocol token for this version.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::V1 => "iroh-relay-v1",
            Self::V2 => "iroh-relay-v2",
        }
    }

    /// Parses one subprotocol token, without trimming whitespace.
    pub fn from_token(s: &str) -> Option<Self> {
        match s {
            "iroh-relay-v1" => Some(Self::V1),
            "iroh-relay-v2" => Some(Self::V2),
            _ => None,
        }
    }

    /// Selects the highest supported version from a comma-separated
    /// `Sec-WebSocket-Protocol` value, trimming each token.
    pub fn negotiate(offer: &str) -> Option<Self> {
        offer
            .split(',')
            .map(str::trim)
            .filter_map(Self::from_token)
            .max()
    }
}

/// Explicit congestion notification codepoint carried by datagram frames.
///
/// The wire byte is the codepoint's two bits; decoding ignores the upper six.
/// `0` means not ECN-capable and is represented as `None`.
///
/// noq-proto 1.4.0, `src/shared.rs` (`EcnCodepoint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Ecn {
    /// ECT(1), wire `0b01`.
    Ect1 = 0b01,
    /// ECT(0), wire `0b10`.
    Ect0 = 0b10,
    /// Congestion experienced, wire `0b11`.
    Ce = 0b11,
}

impl Ecn {
    /// Decodes the low two bits of an ECN byte, `None` for not-ECT.
    pub const fn from_bits(b: u8) -> Option<Self> {
        match b & 0b11 {
            0b01 => Some(Self::Ect1),
            0b10 => Some(Self::Ect0),
            0b11 => Some(Self::Ce),
            _ => None,
        }
    }

    /// Encodes an optional codepoint as a wire byte.
    pub const fn to_bits(ecn: Option<Self>) -> u8 {
        match ecn {
            Some(e) => e as u8,
            None => 0,
        }
    }
}

/// One or more QUIC datagrams carried by a relay datagram frame.
///
/// Same semantics as `iroh_relay::protos::relay::Datagrams`: `contents` is a
/// concatenation of `segment_size`-byte segments, the last of which may be
/// shorter. Without a segment size, `contents` is a single datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatagramBatch<'a> {
    /// ECN codepoint for all datagrams in the batch.
    pub ecn: Option<Ecn>,
    /// Segment size, `None` for a single datagram.
    pub segment_size: Option<NonZeroU16>,
    /// Concatenated datagram contents.
    pub contents: &'a [u8],
}

impl<'a> DatagramBatch<'a> {
    /// Returns an iterator over the individual datagrams.
    pub fn segments(&self) -> impl Iterator<Item = &'a [u8]> + use<'a> {
        let size = self
            .segment_size
            .map_or(self.contents.len().max(1), |s| usize::from(s.get()));
        self.contents.chunks(size)
    }

    /// Length of `ecn ‖ [segment_size] ‖ contents`, the part after the key.
    fn encoded_len(&self) -> usize {
        1 + self.segment_size.map_or(0, |_| 2) + self.contents.len()
    }
}

/// A connection status sent by the server.
///
/// iroh-relay 1.4.0, `src/protos/relay.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Status {
    /// The connection is healthy again. Upstream sends it when a superseded
    /// socket becomes the active one again.
    Healthy,
    /// Another socket for the same EndpointId became active. Upstream sends it
    /// to the superseded socket and does not close that socket.
    SameEndpointIdConnected,
    /// The relay is rate-limiting this client.
    RateLimited,
}

impl Status {
    const fn code(&self) -> u8 {
        match self {
            Self::Healthy => 0,
            Self::SameEndpointIdConnected => 1,
            Self::RateLimited => 2,
        }
    }

    /// The text upstream sends in a `Health` frame under `iroh-relay-v1`,
    /// which is the status's `Display` output.
    const fn v1_problem(&self) -> &'static str {
        match self {
            Self::Healthy => "The connection is healthy and has recovered from previous problems",
            Self::SameEndpointIdConnected => {
                "Another endpoint connected with the same endpoint id. No more messages will be received."
            }
            Self::RateLimited => {
                "The relay is rate-limiting this endpoint; outbound relay traffic is being throttled. \
                 Send less data over the relay, or, if you operate this relay, raise Limits::client_rx. \
                 Read more about rate limiting at https://docs.iroh.computer/relays/rate-limiting"
            }
        }
    }
}

/// An encoded relay frame: the bytes of exactly one binary WebSocket message.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame(Vec<u8>);

impl Frame {
    /// Returns the frame bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consumes the frame, returning its bytes.
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }

    fn with_type(t: FrameType, cap: usize) -> Self {
        let mut v = Vec::with_capacity(1 + cap);
        // All frame types are below 64, so their QUIC varint is a single byte
        // equal to the value.
        v.push(t as u8);
        Self(v)
    }
}

impl AsRef<[u8]> for Frame {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<Frame> for Vec<u8> {
    fn from(f: Frame) -> Self {
        f.0
    }
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.0.len().min(48);
        write!(f, "Frame(len={}, ", self.0.len())?;
        for b in &self.0[..n] {
            write!(f, "{b:02x}")?;
        }
        if n < self.0.len() {
            write!(f, "…")?;
        }
        write!(f, ")")
    }
}

/// Error encoding a server-to-client datagram frame.
#[stack_error(derive, add_meta)]
#[non_exhaustive]
pub enum EncodeError {
    /// The batch has no contents. The stock client rejects empty datagrams.
    #[error("datagram contents are empty")]
    Empty {},
    /// The frame would exceed [`MAX_PACKET_SIZE`] after the type byte, which
    /// makes the client drop the connection.
    #[error("datagram frame too large: {len} bytes after type, max {MAX_PACKET_SIZE}")]
    TooLarge {
        /// Bytes after the frame type.
        len: usize,
    },
    /// The session is not authenticated. A client that receives anything but
    /// a handshake frame during the handshake fails it.
    #[error("session is not authenticated")]
    NotAuthenticated {},
}

/// Encodes a server-to-client datagram frame from `src`.
///
/// The frame is `RelayToClientDatagramBatch` if `batch.segment_size` is set and
/// `RelayToClientDatagram` otherwise. [`IngressSession::encode_datagrams`]
/// calls this with the session's local EndpointId.
///
/// # Errors
///
/// Returns [`EncodeError::Empty`] for empty contents and
/// [`EncodeError::TooLarge`] if the frame would exceed [`MAX_PACKET_SIZE`].
///
/// [`IngressSession::encode_datagrams`]: crate::IngressSession::encode_datagrams
pub fn encode_datagrams(src: &EndpointId, batch: &DatagramBatch<'_>) -> Result<Frame, EncodeError> {
    if batch.contents.is_empty() {
        return Err(e!(EncodeError::Empty));
    }
    let len = KEY_LEN + batch.encoded_len();
    if len > MAX_PACKET_SIZE {
        return Err(e!(EncodeError::TooLarge { len }));
    }
    let t = if batch.segment_size.is_some() {
        FrameType::RelayToClientDatagramBatch
    } else {
        FrameType::RelayToClientDatagram
    };
    let mut f = Frame::with_type(t, len);
    f.0.extend_from_slice(src.as_bytes());
    f.0.push(Ecn::to_bits(batch.ecn));
    if let Some(s) = batch.segment_size {
        f.0.extend_from_slice(&s.get().to_be_bytes());
    }
    f.0.extend_from_slice(batch.contents);
    Ok(f)
}

/// Encodes `ServerChallenge`: `00 ‖ challenge[16]`. postcard writes a byte
/// array without a length prefix.
pub(crate) fn encode_server_challenge(challenge: &[u8; CHALLENGE_LEN]) -> Frame {
    let mut f = Frame::with_type(FrameType::ServerChallenge, CHALLENGE_LEN);
    f.0.extend_from_slice(challenge);
    f
}

/// Encodes `ServerConfirmsAuth`, a unit struct: `02`.
pub(crate) fn encode_server_confirms_auth() -> Frame {
    Frame::with_type(FrameType::ServerConfirmsAuth, 0)
}

/// Encodes `ServerDeniesAuth`: `03 ‖ varint(len) ‖ reason`, a postcard
/// `String`.
pub(crate) fn encode_server_denies_auth(reason: &str) -> Frame {
    let mut f = Frame::with_type(FrameType::ServerDeniesAuth, reason.len() + 5);
    let mut n = reason.len();
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            f.0.push(byte);
            break;
        }
        f.0.push(byte | 0x80);
    }
    f.0.extend_from_slice(reason.as_bytes());
    f
}

/// Encodes `Pong`: `0a ‖ data[8]`.
pub(crate) fn encode_pong(data: &[u8; PING_LEN]) -> Frame {
    let mut f = Frame::with_type(FrameType::Pong, PING_LEN);
    f.0.extend_from_slice(data);
    f
}

/// Encodes a connection status in the form `version` allows.
///
/// `iroh-relay-v2` gets `Status` (`0d ‖ code`). `iroh-relay-v1` clients do not
/// decode `Status`, so they get `Health` (`0b ‖ text`) with upstream's text.
pub fn encode_status(version: ProtocolVersion, status: Status) -> Frame {
    match version {
        ProtocolVersion::V2 => {
            let mut f = Frame::with_type(FrameType::Status, 1);
            f.0.push(status.code());
            f
        }
        ProtocolVersion::V1 => {
            let p = status.v1_problem();
            let mut f = Frame::with_type(FrameType::Health, p.len());
            f.0.extend_from_slice(p.as_bytes());
            f
        }
    }
}

/// Returns the message a client signs for `challenge`.
pub(crate) fn challenge_message(challenge: &[u8; CHALLENGE_LEN]) -> [u8; 32] {
    blake3::derive_key(DOMAIN_SEP_CHALLENGE, challenge)
}

/// Decodes a QUIC variable-length integer (RFC 9000, section 16), returning
/// the value and the number of bytes consumed.
///
/// Non-minimal encodings are accepted, as upstream's frame type decoder does.
pub(crate) fn decode_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let first = *buf.first()?;
    let len = 1usize << (first >> 6);
    let bytes = buf.get(..len)?;
    let mut v = u64::from(first & 0x3f);
    for b in &bytes[1..] {
        v = (v << 8) | u64::from(*b);
    }
    Some((v, len))
}

/// A client-to-server frame, decoded.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClientFrame<'a> {
    ClientAuth {
        public_key: [u8; KEY_LEN],
        signature: [u8; SIG_LEN],
    },
    Datagrams {
        dst: &'a [u8; KEY_LEN],
        batch: DatagramBatch<'a>,
    },
    Ping([u8; PING_LEN]),
    Pong([u8; PING_LEN]),
    /// A known frame type a client must never send.
    Unexpected(FrameType),
}

/// Why a client frame could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeError {
    Empty,
    UnknownFrameType(u64),
    TooLarge(usize),
    Invalid(FrameType),
}

/// Decodes one client-to-server message.
///
/// Follows upstream's `ClientToRelayMsg::from_bytes`, with two differences:
/// the destination key is returned as raw bytes, since it is only compared
/// with the local EndpointId, and `ClientAuth` must be exactly
/// [`CLIENT_AUTH_LEN`] bytes.
pub(crate) fn decode_client_frame(msg: &[u8]) -> Result<ClientFrame<'_>, DecodeError> {
    let (tag, n) = decode_varint(msg).ok_or(DecodeError::Empty)?;
    let t = FrameType::from_u64(tag).ok_or(DecodeError::UnknownFrameType(tag))?;
    let p = &msg[n..];
    if p.len() > MAX_PACKET_SIZE {
        return Err(DecodeError::TooLarge(p.len()));
    }
    let inv = DecodeError::Invalid(t);
    match t {
        FrameType::ClientAuth => {
            if p.len() != CLIENT_AUTH_LEN - 1 || p[KEY_LEN] != SIG_LEN as u8 {
                return Err(inv);
            }
            let public_key: [u8; KEY_LEN] = p[..KEY_LEN].try_into().map_err(|_| inv)?;
            let signature: [u8; SIG_LEN] = p[KEY_LEN + 1..].try_into().map_err(|_| inv)?;
            Ok(ClientFrame::ClientAuth {
                public_key,
                signature,
            })
        }
        FrameType::ClientToRelayDatagram | FrameType::ClientToRelayDatagramBatch => {
            let is_batch = t == FrameType::ClientToRelayDatagramBatch;
            // Key and ECN byte, plus the segment size for a batch.
            let min = KEY_LEN + if is_batch { 3 } else { 1 };
            if p.len() < min {
                return Err(inv);
            }
            let (dst, rest) = p.split_at(KEY_LEN);
            let dst: &[u8; KEY_LEN] = dst.try_into().map_err(|_| inv)?;
            let ecn = Ecn::from_bits(rest[0]);
            let (segment_size, contents) = if is_batch {
                let s = u16::from_be_bytes([rest[1], rest[2]]);
                // Upstream decodes a zero segment size as none.
                (NonZeroU16::new(s), &rest[3..])
            } else {
                (None, &rest[1..])
            };
            Ok(ClientFrame::Datagrams {
                dst,
                batch: DatagramBatch {
                    ecn,
                    segment_size,
                    contents,
                },
            })
        }
        FrameType::Ping | FrameType::Pong => {
            let data: [u8; PING_LEN] = p.try_into().map_err(|_| inv)?;
            Ok(if t == FrameType::Ping {
                ClientFrame::Ping(data)
            } else {
                ClientFrame::Pong(data)
            })
        }
        other => Ok(ClientFrame::Unexpected(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_auth_vectors() {
        // Upstream's default denial reason.
        assert_eq!(
            encode_server_denies_auth("not authorized").as_bytes(),
            b"\x03\x0enot authorized"
        );
        // A length of 128 or more takes two varint bytes.
        let long = "x".repeat(300);
        let f = encode_server_denies_auth(&long);
        assert_eq!(&f.as_bytes()[..3], &[0x03, 0xac, 0x02]);
        assert_eq!(f.as_bytes().len(), 3 + 300);
    }

    #[test]
    fn varint() {
        assert_eq!(decode_varint(&[0x09]), Some((9, 1)));
        assert_eq!(decode_varint(&[0x40, 0x09]), Some((9, 2)));
        assert_eq!(decode_varint(&[0x80, 0, 0, 0x0d]), Some((13, 4)));
        assert_eq!(decode_varint(&[0xc0, 0, 0, 0, 0, 0, 0, 0x01]), Some((1, 8)));
        assert_eq!(decode_varint(&[0x80, 0]), None);
        assert_eq!(decode_varint(&[]), None);
    }

    #[test]
    fn challenge_and_confirm() {
        assert_eq!(
            encode_server_challenge(&[0x11; 16]).as_bytes(),
            [[0u8].as_slice(), &[0x11; 16]].concat()
        );
        assert_eq!(encode_server_confirms_auth().as_bytes(), [0x02]);
        assert_eq!(
            encode_pong(&[42; 8]).as_bytes(),
            [[0x0au8].as_slice(), &[42; 8]].concat()
        );
    }
}
