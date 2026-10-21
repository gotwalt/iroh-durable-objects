//! Pure request classification for the router `fetch` handler.
//!
//! The router never touches a Durable Object unless the leftmost Host label
//! is a valid z-base-32 EndpointId (note 004: `PublicKey::from_z32` only,
//! never `FromStr`, which misparses z32) or the reserved `admin` label.

use iroh_base::PublicKey;

/// Reserved leftmost label for the admin surface (note 001, spec §13.4).
pub const ADMIN_LABEL: &str = "admin";

/// Path prefix reserved for router → DO calls. Never forwarded from outside.
pub const INTERNAL_PREFIX: &str = "/_internal/";

/// Path the stock relay client dials (`iroh-relay-1.4.0/src/http.rs:13`, `RELAY_PATH`).
pub const RELAY_PATH: &str = "/relay";

/// What the router should do with a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `POST admin.<zone>/mint`.
    Mint,
    /// `GET admin.<zone>/`: the endpoint list page (design note 009).
    ListPage,
    /// `GET admin.<zone>/endpoints.json`: the endpoint list as JSON.
    ListJson,
    /// `GET admin.<zone>/status/<E>`: forward to the object named E for its status.
    Status(PublicKey),
    /// `GET <E>.<zone>/relay` with `Upgrade: websocket`: forward to the DO named E.
    Relay(PublicKey),
    /// `GET <E>.<zone>/ping` or `/generate_204`: answer in the router.
    Probe(Probe),
    /// Anything else: 404 without touching a DO.
    NotFound,
    /// Wrong method on a known path.
    MethodNotAllowed,
    /// A known path that requires a WebSocket upgrade but has none.
    UpgradeRequired,
}

/// Probes answered by the router without touching an object, as iroh's relay
/// server answers them. Stock clients send them only to their home relay; the
/// room's members use the room's relay URL as theirs (design note 008), and
/// probe it whenever iroh's net_report runs (iroh 1.4.0,
/// `src/net_report/reportgen.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// `/ping`: 200 empty body.
    Ping,
    /// `/generate_204`: 204, echoing `X-Iroh-Challenge` as `X-Iroh-Response`.
    Generate204,
}

/// Leftmost label of `host` if the remainder equals `zone_suffix`
/// (case-insensitive, port ignored). Returns the label lower-cased.
pub fn label_for_zone(host: &str, zone_suffix: &str) -> Option<String> {
    let host = strip_port(host).trim_end_matches('.').to_ascii_lowercase();
    let zone = zone_suffix.trim_end_matches('.').to_ascii_lowercase();
    let (label, rest) = host.split_once('.')?;
    if label.is_empty() || rest != zone {
        return None;
    }
    Some(label.to_string())
}

fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        // IPv6 literal: never a valid endpoint host.
        return host;
    }
    match host.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    }
}

/// Parse an endpoint label. z-base-32, 52 chars, via `PublicKey::from_z32`.
pub fn parse_endpoint_label(label: &str) -> Option<PublicKey> {
    // Cheap pre-filter before any curve decompression.
    if label.len() != 52 {
        return None;
    }
    PublicKey::from_z32(label).ok()
}

/// Classify a public request.
pub fn classify(
    host: &str,
    method: &str,
    path: &str,
    is_upgrade: bool,
    zone_suffix: &str,
) -> Route {
    if path.starts_with(INTERNAL_PREFIX) {
        return Route::NotFound;
    }
    let Some(label) = label_for_zone(host, zone_suffix) else {
        return Route::NotFound;
    };
    if label == ADMIN_LABEL {
        return match (method, path) {
            ("POST", "/mint") => Route::Mint,
            (_, "/mint") => Route::MethodNotAllowed,
            ("GET", "/") => Route::ListPage,
            ("GET", "/endpoints.json") => Route::ListJson,
            ("GET", p) => match p.strip_prefix("/status/").and_then(parse_endpoint_label) {
                Some(id) => Route::Status(id),
                None => Route::NotFound,
            },
            (_, "/" | "/endpoints.json") => Route::MethodNotAllowed,
            _ => Route::NotFound,
        };
    }
    let Some(id) = parse_endpoint_label(&label) else {
        return Route::NotFound;
    };
    match path {
        RELAY_PATH => {
            if method != "GET" {
                Route::MethodNotAllowed
            } else if !is_upgrade {
                Route::UpgradeRequired
            } else {
                Route::Relay(id)
            }
        }
        "/ping" => Route::Probe(Probe::Ping),
        "/generate_204" => Route::Probe(Probe::Generate204),
        _ => Route::NotFound,
    }
}

/// `true` if the `Upgrade` header value names websocket.
pub fn is_websocket_upgrade(upgrade: Option<&str>) -> bool {
    upgrade.is_some_and(|u| {
        u.split(',')
            .any(|t| t.trim().eq_ignore_ascii_case("websocket"))
    })
}

/// Extract the bearer token from an `Authorization` header value.
pub fn bearer(authorization: Option<&str>) -> Option<&str> {
    let v = authorization?.trim();
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

/// Constant-time equality over the full length of both inputs (the length
/// itself is not secret: tokens are fixed-length random strings).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b) {
        acc |= x ^ y;
    }
    // Keeps the optimizer from short-circuiting on `acc`.
    std::hint::black_box(acc) == 0
}

/// Validate an `X-Iroh-Challenge` value the way upstream does
/// (`iroh-relay-1.4.0/src/server.rs:1054-1085`): non-empty, < 64 chars of
/// `[A-Za-z0-9._-]`. Returns the `X-Iroh-Response` value.
pub fn no_content_response(challenge: Option<&str>) -> Option<String> {
    let c = challenge?;
    let ok = !c.is_empty()
        && c.len() < 64
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_');
    ok.then(|| format!("response {c}"))
}

/// Relay URL for E, given the scheme and authority suffix the admin request
/// arrived on (`https://admin.<zone>` → `https://<E>.<zone>/`).
pub fn relay_url_for(id: &PublicKey, scheme: &str, zone_suffix: &str, port: Option<u16>) -> String {
    let z = id.to_z32();
    match port {
        Some(p) => format!("{scheme}://{z}.{zone_suffix}:{p}/"),
        None => format!("{scheme}://{z}.{zone_suffix}/"),
    }
}

/// A close code the DO may send back when completing a peer-initiated close
/// (RFC 6455 §7.4): the peer's code if it is sendable, else 1000. 1004, 1005,
/// 1006 and 1015 are reserved (1005/1006/1015 only report local conditions), and
/// codes outside the defined ranges are invalid; `WebSocket.close` throws on them.
pub fn sendable_close_code(code: usize) -> u16 {
    let c = u16::try_from(code).unwrap_or(0);
    if matches!(c, 1000..=1003 | 1007..=1014 | 3000..=4999) {
        c
    } else {
        1000
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh_base::SecretKey;

    const ZONE: &str = "iroh-do.example.com";

    fn key() -> PublicKey {
        SecretKey::from_bytes(&[7u8; 32]).public()
    }

    #[test]
    fn z32_label_routes() {
        let id = key();
        let host = format!("{}.{ZONE}", id.to_z32());
        assert_eq!(
            classify(&host, "GET", "/relay", true, ZONE),
            Route::Relay(id)
        );
        let host_port = format!("{}.localhost:8787", id.to_z32());
        assert_eq!(
            classify(&host_port, "GET", "/relay", true, "localhost"),
            Route::Relay(id)
        );
    }

    #[test]
    fn uppercase_host_is_normalised() {
        let id = key();
        let host = format!("{}.{}", id.to_z32().to_uppercase(), ZONE.to_uppercase());
        assert_eq!(
            classify(&host, "GET", "/relay", true, ZONE),
            Route::Relay(id)
        );
    }

    #[test]
    fn hex_and_rfc4648_labels_rejected() {
        // `Display` is 64-char hex: too long for a label and not accepted.
        let id = key();
        let host = format!("{id}.{ZONE}");
        assert_eq!(
            classify(&host, "GET", "/relay", true, ZONE),
            Route::NotFound
        );
        // A 52-char RFC 4648 base32 string is what `FromStr` accepts; we must not.
        let b32 = data_encoding::BASE32_NOPAD
            .encode(id.as_bytes())
            .to_ascii_lowercase();
        assert_eq!(b32.len(), 52);
        let parsed = parse_endpoint_label(&b32);
        assert_ne!(parsed, Some(id));
    }

    #[test]
    fn from_str_is_not_used_for_labels() {
        // Pin note 004: the z32 form does not round-trip through FromStr.
        let id = key();
        let z = id.to_z32();
        let via_from_str: Result<PublicKey, _> = z.parse();
        assert_ne!(via_from_str.ok(), Some(id));
        assert_eq!(parse_endpoint_label(&z), Some(id));
    }

    #[test]
    fn admin_list_and_status() {
        let admin = format!("admin.{ZONE}");
        assert_eq!(classify(&admin, "GET", "/", false, ZONE), Route::ListPage);
        assert_eq!(
            classify(&admin, "GET", "/endpoints.json", false, ZONE),
            Route::ListJson
        );
        assert_eq!(
            classify(&admin, "POST", "/", false, ZONE),
            Route::MethodNotAllowed
        );
        let id = key();
        assert_eq!(
            classify(
                &admin,
                "GET",
                &format!("/status/{}", id.to_z32()),
                false,
                ZONE
            ),
            Route::Status(id)
        );
        // Only a valid z-base-32 id reaches an object.
        for bad in ["/status/", "/status/registry", "/status/../mint"] {
            assert_eq!(
                classify(&admin, "GET", bad, false, ZONE),
                Route::NotFound,
                "{bad}"
            );
        }
        let hex = format!("/status/{}", "0".repeat(64));
        assert_eq!(classify(&admin, "GET", &hex, false, ZONE), Route::NotFound);
    }

    #[test]
    fn admin_surface() {
        assert_eq!(
            classify(&format!("admin.{ZONE}"), "POST", "/mint", false, ZONE),
            Route::Mint
        );
        assert_eq!(
            classify(&format!("admin.{ZONE}"), "GET", "/mint", false, ZONE),
            Route::MethodNotAllowed
        );
        assert_eq!(
            classify(&format!("admin.{ZONE}"), "GET", "/relay", true, ZONE),
            Route::NotFound
        );
    }

    #[test]
    fn internal_never_routed() {
        let id = key();
        let host = format!("{}.{ZONE}", id.to_z32());
        assert_eq!(
            classify(&host, "POST", "/_internal/mint", false, ZONE),
            Route::NotFound
        );
        assert_eq!(
            classify(
                &format!("admin.{ZONE}"),
                "POST",
                "/_internal/mint",
                false,
                ZONE
            ),
            Route::NotFound
        );
    }

    #[test]
    fn foreign_zone_and_garbage() {
        let id = key();
        assert_eq!(
            classify(
                &format!("{}.evil.com", id.to_z32()),
                "GET",
                "/relay",
                true,
                ZONE
            ),
            Route::NotFound
        );
        assert_eq!(classify(ZONE, "GET", "/relay", true, ZONE), Route::NotFound);
        assert_eq!(
            classify(&format!("a.b.{ZONE}"), "GET", "/relay", true, ZONE),
            Route::NotFound
        );
        assert_eq!(
            classify(&format!("nothex.{ZONE}"), "GET", "/relay", true, ZONE),
            Route::NotFound
        );
        assert_eq!(classify("", "GET", "/relay", true, ZONE), Route::NotFound);
    }

    #[test]
    fn relay_requires_upgrade_and_get() {
        let id = key();
        let host = format!("{}.{ZONE}", id.to_z32());
        assert_eq!(
            classify(&host, "GET", "/relay", false, ZONE),
            Route::UpgradeRequired
        );
        assert_eq!(
            classify(&host, "POST", "/relay", true, ZONE),
            Route::MethodNotAllowed
        );
        assert_eq!(
            classify(&host, "GET", "/ping", false, ZONE),
            Route::Probe(Probe::Ping)
        );
        assert_eq!(
            classify(&host, "GET", "/generate_204", false, ZONE),
            Route::Probe(Probe::Generate204)
        );
        assert_eq!(classify(&host, "GET", "/", false, ZONE), Route::NotFound);
    }

    #[test]
    fn bearer_and_ct_eq() {
        assert_eq!(bearer(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer(Some("bearer  abc ")), Some("abc"));
        assert_eq!(bearer(Some("Basic abc")), None);
        assert_eq!(bearer(None), None);
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
    }

    #[test]
    fn upgrade_header() {
        assert!(is_websocket_upgrade(Some("websocket")));
        assert!(is_websocket_upgrade(Some("WebSocket")));
        assert!(!is_websocket_upgrade(Some("h2c")));
        assert!(!is_websocket_upgrade(None));
    }

    #[test]
    fn no_content_challenge() {
        assert_eq!(
            no_content_response(Some("abc-1.2_3")),
            Some("response abc-1.2_3".into())
        );
        assert_eq!(no_content_response(Some("a b")), None);
        assert_eq!(no_content_response(Some("")), None);
        assert_eq!(no_content_response(None), None);
    }

    #[test]
    fn relay_url_shape() {
        let id = key();
        let u = relay_url_for(&id, "https", ZONE, None);
        assert_eq!(u, format!("https://{}.{ZONE}/", id.to_z32()));
        assert!(!u.contains('='), "TXT parser truncates at '='");
        let l = relay_url_for(&id, "http", "localhost", Some(8787));
        assert_eq!(l, format!("http://{}.localhost:8787/", id.to_z32()));
    }

    #[test]
    fn close_codes_echoed_only_if_sendable() {
        for ok in [1000, 1001, 1003, 1008, 1011, 1014, 3000, 4999] {
            assert_eq!(sendable_close_code(ok), ok as u16);
        }
        for reserved in [0, 999, 1004, 1005, 1006, 1015, 1016, 2999, 5000, 70_000] {
            assert_eq!(sendable_close_code(reserved), 1000, "{reserved}");
        }
    }
}
