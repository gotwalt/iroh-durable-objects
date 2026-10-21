//! Test helpers: a deterministic RNG, client-side encoders, and a decoder for
//! server frames written independently of the crate's codec.
#![allow(dead_code)]

use std::{convert::Infallible, num::NonZeroU16, time::Duration};

use iroh_base::{PublicKey, SecretKey};
use iroh_relay_ingress::{
    DOMAIN_SEP_CHALLENGE, Ecn, IngressConfig, IngressSession, Output, UpgradeRequest,
};

/// Secret key used by upstream's snapshot tests.
pub fn key42() -> SecretKey {
    SecretKey::from_bytes(&[42u8; 32])
}

pub fn hexd(s: &str) -> Vec<u8> {
    let s: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '…')
        .collect();
    hex::decode(s).expect("valid hex")
}

/// RNG returning a fixed byte forever. `TryCryptoRng` only so tests can pin the challenge.
pub struct FixedRng(pub u8);

impl rand_core::TryRng for FixedRng {
    type Error = Infallible;
    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        Ok(u32::from_ne_bytes([self.0; 4]))
    }
    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        Ok(u64::from_ne_bytes([self.0; 8]))
    }
    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
        dst.fill(self.0);
        Ok(())
    }
}
impl rand_core::TryCryptoRng for FixedRng {}

pub const T0: Duration = Duration::from_secs(1_700_000_000);

pub fn req(proto: Option<&str>) -> UpgradeRequest<'_> {
    UpgradeRequest {
        method: "GET",
        path: "/relay",
        sec_websocket_protocol: proto,
    }
}

/// `01 ‖ pk ‖ 40 ‖ sig` over `blake3::derive_key(DOMAIN_SEP, challenge)`.
pub fn client_auth(secret: &SecretKey, challenge: &[u8; 16]) -> Vec<u8> {
    let msg = blake3::derive_key(DOMAIN_SEP_CHALLENGE, challenge);
    let sig = secret.sign(&msg).to_bytes();
    let mut v = vec![0x01];
    v.extend_from_slice(secret.public().as_bytes());
    v.push(0x40);
    v.extend_from_slice(&sig);
    v
}

/// Client-to-server datagram frame (`04` / `05`).
pub fn client_datagram(
    dst: &PublicKey,
    ecn: Option<Ecn>,
    segment_size: Option<NonZeroU16>,
    contents: &[u8],
) -> Vec<u8> {
    let mut v = vec![if segment_size.is_some() { 0x05 } else { 0x04 }];
    v.extend_from_slice(dst.as_bytes());
    v.push(Ecn::to_bits(ecn));
    if let Some(s) = segment_size {
        v.extend_from_slice(&s.get().to_be_bytes());
    }
    v.extend_from_slice(contents);
    v
}

/// A decoded server-to-client frame, following upstream's
/// `RelayToClientMsg::from_bytes`.
#[derive(Debug, PartialEq, Eq)]
pub enum ServerMsg {
    Datagrams {
        src: [u8; 32],
        ecn: Option<Ecn>,
        segment_size: Option<NonZeroU16>,
        contents: Vec<u8>,
    },
    Pong([u8; 8]),
    Status(u8),
    Health(String),
}

pub fn decode_server(b: &[u8]) -> Option<ServerMsg> {
    let (t, p) = b.split_first()?;
    if p.len() > 65536 {
        return None;
    }
    match t {
        0x06 | 0x07 => {
            let batch = *t == 0x07;
            if p.len() < 32 + if batch { 3 } else { 1 } {
                return None;
            }
            let src: [u8; 32] = p[..32].try_into().ok()?;
            PublicKey::from_bytes(&src).ok()?;
            let ecn = Ecn::from_bits(p[32]);
            let (segment_size, rest) = if batch {
                (
                    NonZeroU16::new(u16::from_be_bytes([p[33], p[34]])),
                    &p[35..],
                )
            } else {
                (None, &p[33..])
            };
            Some(ServerMsg::Datagrams {
                src,
                ecn,
                segment_size,
                contents: rest.to_vec(),
            })
        }
        0x0a => Some(ServerMsg::Pong(p.try_into().ok()?)),
        0x0d => Some(ServerMsg::Status(*p.first()?)),
        0x0b => Some(ServerMsg::Health(String::from_utf8(p.to_vec()).ok()?)),
        _ => None,
    }
}

/// An authenticated session for local id `local`, peer `peer`.
pub fn authed(local: PublicKey, peer: &SecretKey, cfg: Option<IngressConfig>) -> IngressSession {
    let cfg = cfg.unwrap_or_else(|| IngressConfig::new(local));
    let (_resp, mut s) = IngressSession::on_upgrade(
        &req(Some("iroh-relay-v2, iroh-relay-v1")),
        &cfg,
        &mut FixedRng(7),
        T0,
    )
    .expect("upgrade");
    let m = client_auth(peer, &[7u8; 16]);
    let outs: Vec<_> = s.on_message(&m, T0).expect("auth").collect();
    assert!(matches!(outs.last(), Some(Output::Authenticated { .. })));
    s
}
