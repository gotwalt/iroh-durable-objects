//! Discovery record for the DO's EndpointId (spec §5.9, note 004).
//!
//! Built with iroh's own construction (`EndpointInfo::to_pkarr_signed_packet`,
//! the call `PkarrPublisher` makes in `iroh-1.4.0/src/address_lookup/pkarr.rs`),
//! so the format matches stock resolvers exactly. Publishing is a single
//! `PUT <relay>/<z32>`; because a stale PUT also answers 204, success is
//! confirmed by a `GET` that returns our record (note 004 §2).

use iroh_base::{RelayUrl, SecretKey};
use iroh_dns::{endpoint_info::EndpointInfo, pkarr::SignedPacket};

/// Record TTL, same as iroh's `DEFAULT_PKARR_TTL` (`iroh-1.4.0/src/address_lookup/pkarr.rs:143`).
pub const PKARR_TTL_SECS: u32 = 30;

/// Republish interval (note 004 §3; n0's server evicts after 7 days by default).
pub const REPUBLISH_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;

/// Retry delay after a failed publish (429/5xx/network), before jitter.
pub const RETRY_BASE_MS: u64 = 60 * 1000;

/// A built publish request.
#[derive(Debug, Clone)]
pub struct PublishRequest {
    /// `<pkarr relay>/<z32>`.
    pub url: String,
    /// Body: the signed packet minus the leading public key.
    pub body: Vec<u8>,
    /// The packet's timestamp, to compare against what a GET returns.
    pub timestamp_us: u64,
}

/// Errors building a record.
#[derive(Debug)]
pub enum BuildError {
    /// The relay URL did not parse.
    RelayUrl,
    /// iroh-dns refused to encode the packet.
    Encode,
}

/// Build the relay-only record for `secret` pointing at `relay_url`.
pub fn build_publish(
    secret: &SecretKey,
    relay_url: &str,
    pkarr_relay: &str,
) -> Result<PublishRequest, BuildError> {
    let relay: RelayUrl = relay_url.parse().map_err(|_| BuildError::RelayUrl)?;
    let pkt = EndpointInfo::new(secret.public())
        .with_relay_url(relay)
        .to_pkarr_signed_packet(secret, PKARR_TTL_SECS)
        .map_err(|_| BuildError::Encode)?;
    Ok(PublishRequest {
        url: format!(
            "{}/{}",
            pkarr_relay.trim_end_matches('/'),
            secret.public().to_z32()
        ),
        body: pkt.to_relay_payload(),
        timestamp_us: pkt.timestamp().as_micros(),
    })
}

/// Check a GET response body: valid signature for `secret.public()`, at
/// least as recent as what we published, and advertising `relay_url`.
pub fn confirm(secret: &SecretKey, body: &[u8], min_timestamp_us: u64, relay_url: &str) -> bool {
    let Ok(pkt) = SignedPacket::from_relay_payload(&secret.public(), body) else {
        return false;
    };
    if pkt.timestamp().as_micros() < min_timestamp_us {
        return false;
    }
    let want = format!("relay={relay_url}");
    pkt.all_txt_records().iter().any(|(_, v)| *v == want)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_confirm_roundtrip() {
        let sk = SecretKey::from_bytes(&[3u8; 32]);
        let relay = format!("https://{}.iroh-do.example.com/", sk.public().to_z32());
        let req = build_publish(&sk, &relay, "https://dns.iroh.link/pkarr/").unwrap();
        assert_eq!(
            req.url,
            format!("https://dns.iroh.link/pkarr/{}", sk.public().to_z32())
        );
        assert!(req.body.len() <= 1104);
        assert!(confirm(&sk, &req.body, req.timestamp_us, &relay));
        assert!(!confirm(&sk, &req.body, req.timestamp_us + 1, &relay));
        assert!(!confirm(&sk, &req.body, req.timestamp_us, "https://other/"));
        let other = SecretKey::from_bytes(&[4u8; 32]);
        assert!(!confirm(&other, &req.body, 0, &relay));
    }
}
