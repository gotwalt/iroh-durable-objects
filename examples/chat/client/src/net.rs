//! Endpoint construction, target addressing and connection helpers.
//!
//! Everything here is stock iroh 1.x with default features: the peer side of the
//! demo must not need anything a normal iroh application would not have (spec P3).

use std::{
    fmt,
    time::{Duration, Instant},
};

use iroh::{
    Endpoint, EndpointAddr, EndpointId, PublicKey, RelayMode, RelayUrl, SecretKey, TransportAddr,
    address_lookup::{DnsAddressLookup, PkarrResolver},
    endpoint::{Connection, ZeroRttStatus, presets},
};
use n0_error::{Result, StdResultExt, anyerr};
use n0_future::StreamExt;
use tokio::sync::mpsc;

/// `iroh-do/echo/1`: one bi stream, the DO echoes the bytes and finishes (note 006).
pub const ECHO_ALPN: &[u8] = b"iroh-do/echo/1";
/// `iroh-do/ctl/1`: line-oriented diagnostic control stream plus one unprompted uni stream (note 006).
pub const CTL_ALPN: &[u8] = b"iroh-do/ctl/1";
/// `iroh-do/notify/1`: the DO dials *us* and writes one line on a uni stream (note 006).
pub const NOTIFY_ALPN: &[u8] = b"iroh-do/notify/1";
/// `iroh-do/room/1`: the chat room. Either side may open the connection (note 008).
pub const ROOM_ALPN: &[u8] = b"iroh-do/room/1";

/// How long to wait for address lookup to return E's relay URL.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);

/// Parses an endpoint id from its 52-char z-base-32 form.
///
/// Only `PublicKey::from_z32` is used: `EndpointId`'s `FromStr` accepts hex or
/// RFC 4648 base32 and silently misparses some z32 strings (note 004).
pub fn parse_endpoint_id(s: &str) -> Result<EndpointId> {
    let s = s.trim();
    PublicKey::from_z32(s).map_err(|err| {
        anyerr!(
            "invalid endpoint id {s:?}: expected the 52-char z-base-32 form printed by `just mint` ({err})"
        )
    })
}

/// Whether `s` is an EndpointId in z-base-32 form (52 characters).
pub fn is_z32_endpoint_id(s: &str) -> bool {
    s.len() == 52 && PublicKey::from_z32(s).is_ok()
}

/// How the demo reaches the DO.
#[derive(Debug, Clone)]
pub struct Target {
    /// The DO's endpoint id.
    pub id: EndpointId,
    /// Explicit relay URL; `None` means default discovery (pkarr + DNS).
    pub relay_url: Option<RelayUrl>,
}

impl Target {
    /// Builds a target from CLI flags.
    pub fn new(
        id: EndpointId,
        relay_url: Option<&str>,
        local: bool,
        local_port: u16,
    ) -> Result<Self> {
        let relay_url = match (relay_url, local) {
            (Some(url), _) => Some(parse_relay_url(url)?),
            (None, true) => Some(parse_relay_url(&format!(
                "http://{}.localhost:{local_port}/",
                id.to_z32()
            ))?),
            (None, false) => None,
        };
        Ok(Self { id, relay_url })
    }

    /// The address handed to `Endpoint::connect`.
    pub fn addr(&self) -> EndpointAddr {
        match &self.relay_url {
            Some(url) => EndpointAddr::new(self.id).with_relay_url(url.clone()),
            None => EndpointAddr::new(self.id),
        }
    }

    /// The z32 form of the target id.
    pub fn z32(&self) -> String {
        self.id.to_z32()
    }

    /// One line for humans.
    pub fn describe(&self) -> String {
        match &self.relay_url {
            Some(url) => format!("{} via explicit relay URL {url}", self.z32()),
            None => format!("{} via default discovery (pkarr + DNS)", self.z32()),
        }
    }
}

impl Target {
    /// E's relay URL: the explicit one, or the first one address lookup finds.
    ///
    /// The lookup uses the resolvers of `presets::N0` (pkarr over HTTPS and
    /// DNS) on a short-lived endpoint without relays, so nothing is published
    /// for it.
    pub async fn relay_url(&self) -> Result<RelayUrl> {
        if let Some(url) = &self.relay_url {
            return Ok(url.clone());
        }
        let ep = Endpoint::builder(presets::Minimal)
            .address_lookup(PkarrResolver::n0_dns())
            .address_lookup(DnsAddressLookup::n0_dns())
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await?;
        let lookup = async {
            let mut items = ep.address_lookup()?.resolve(self.id);
            while let Some(item) = items.next().await {
                if let Ok(Ok(item)) = item
                    && let Some(url) = item.to_endpoint_addr().relay_urls().next()
                {
                    return Ok(url.clone());
                }
            }
            Err(anyerr!(
                "address lookup found no relay URL for {}",
                self.z32()
            ))
        };
        let res = tokio::time::timeout(RESOLVE_TIMEOUT, lookup)
            .await
            .map_err(|_| anyerr!("address lookup for {} timed out", self.z32()))
            .and_then(|r| r);
        ep.close().await;
        res
    }
}

/// Binds a stock endpoint for a chat room member: `presets::N0`, the member's
/// persistent key, and the room's relay URL as the only (so home) relay.
///
/// The home relay connection stays open while the endpoint runs; that open
/// socket is what makes the member present and lets the room dial it back
/// (design note 008). The endpoint accepts `iroh-do/room/1` only.
pub async fn bind_room_endpoint(key: SecretKey, room_relay: RelayUrl) -> Result<Endpoint> {
    let ep = Endpoint::builder(presets::N0)
        .secret_key(key)
        .relay_mode(RelayMode::custom([room_relay]))
        .alpns(vec![ROOM_ALPN.to_vec()])
        .bind()
        .await?;
    Ok(ep)
}

fn parse_relay_url(s: &str) -> Result<RelayUrl> {
    s.parse::<RelayUrl>()
        .map_err(|err| anyerr!("invalid relay URL {s:?}: {err}"))
}

/// Binds a stock endpoint (`presets::N0`: n0 relays, pkarr publish/resolve, DNS lookup)
/// that also accepts `iroh-do/notify/1`, so the DO can dial us back.
pub async fn bind_endpoint() -> Result<Endpoint> {
    let ep = Endpoint::builder(presets::N0)
        .alpns(vec![NOTIFY_ALPN.to_vec()])
        .bind()
        .await?;
    Ok(ep)
}

/// Whether 0-RTT was used for a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroRtt {
    /// No resumption ticket for this server, so 0-RTT was not attempted.
    NoTicket,
    /// The server accepted early data.
    Accepted,
    /// The server rejected early data; the handshake fell back to 1-RTT.
    Rejected,
}

impl fmt::Display for ZeroRtt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ZeroRtt::NoTicket => "not attempted (no ticket)",
            ZeroRtt::Accepted => "accepted",
            ZeroRtt::Rejected => "rejected (fell back to 1-RTT)",
        })
    }
}

/// An established connection plus how it was established.
pub struct Connected {
    /// The connection.
    pub conn: Connection,
    /// 0-RTT outcome.
    pub zero_rtt: ZeroRtt,
    /// Time from `connect` until the handshake completed (includes discovery,
    /// relay dial, DO wake and the QUIC handshake).
    pub handshake: Duration,
}

/// Connects to `target`, attempting 0-RTT whenever a ticket is available.
///
/// No application data is sent as early data here; the outcome reflects whether
/// the server accepted the TLS early-data offer.
pub async fn connect(ep: &Endpoint, target: &Target, alpn: &[u8]) -> Result<Connected> {
    let t0 = Instant::now();
    let connecting = ep
        .connect_with_opts(target.addr(), alpn, Default::default())
        .await?;
    let (conn, zero_rtt) = match connecting.into_0rtt() {
        Ok(early) => match early.handshake_completed().await? {
            ZeroRttStatus::Accepted(conn) => (conn, ZeroRtt::Accepted),
            ZeroRttStatus::Rejected(conn) => (conn, ZeroRtt::Rejected),
        },
        Err(connecting) => (connecting.await.anyerr()?, ZeroRtt::NoTicket),
    };
    Ok(Connected {
        conn,
        zero_rtt,
        handshake: t0.elapsed(),
    })
}

/// Describes the selected path: connection type and its RTT estimate.
pub fn path_summary(conn: &Connection) -> (String, Option<Duration>) {
    let paths = conn.paths();
    for p in paths.iter() {
        if p.is_selected() {
            let kind = match p.remote_addr() {
                TransportAddr::Relay(url) => format!("relay {url}"),
                TransportAddr::Ip(addr) => format!("direct {addr}"),
                other => format!("{other}"),
            };
            return (kind, Some(p.rtt()));
        }
    }
    ("no selected path".to_string(), None)
}

/// A `notify/1` line delivered by a DO-initiated connection.
#[derive(Debug, Clone)]
pub struct Notice {
    /// Who dialed us.
    pub from: EndpointId,
    /// The line, without the trailing newline.
    pub line: String,
    /// When it arrived.
    pub at: Instant,
    /// Duration of the incoming handshake (DO -> us).
    pub handshake: Duration,
}

/// Accepts incoming `notify/1` connections on `ep` and forwards their lines.
///
/// Only one accept loop may run per endpoint; call this once.
pub fn spawn_notify_acceptor(ep: Endpoint) -> mpsc::UnboundedReceiver<Result<Notice>> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let t0 = Instant::now();
                let res = async {
                    let conn = incoming.await.anyerr()?;
                    let handshake = t0.elapsed();
                    if conn.alpn() != NOTIFY_ALPN {
                        conn.close(1u32.into(), b"unexpected alpn");
                        return Err(anyerr!(
                            "unexpected incoming ALPN {:?}",
                            String::from_utf8_lossy(conn.alpn())
                        ));
                    }
                    let mut recv = conn.accept_uni().await.anyerr()?;
                    let data = recv.read_to_end(64 * 1024).await.anyerr()?;
                    let at = Instant::now();
                    // The DO waits (<= 5 s) for us to close, then closes itself.
                    conn.close(0u32.into(), b"thanks");
                    Ok(Notice {
                        from: conn.remote_id(),
                        line: String::from_utf8_lossy(&data).trim_end().to_string(),
                        at,
                        handshake,
                    })
                }
                .await;
                let _ = tx.send(res);
            });
        }
    });
    rx
}

/// Formats a duration as milliseconds with one decimal.
pub fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}
