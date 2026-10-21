//! Stateless router `fetch` handler (spec §5.1, §13.4).
//!
//! Classifies each request with [`route::classify`] and either answers it or
//! forwards it to one object. On the admin host it also serves the endpoint
//! list (design note 009) from the registry object. Forwarding uses `stub.fetch`: Durable Object RPC
//! fails at runtime with worker-build 0.8.7, whose shim does not extend
//! `DurableObject` (design note 002 §1). Minting therefore goes to the object
//! as `POST https://do/_internal/mint`, a path the router refuses from outside.

use iroh_base::SecretKey;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use worker::{Env, Headers, Method, Request, RequestInit, Response, Result};

use super::registry;
use crate::{
    registry::{mint_label, page},
    route::{self, Probe, Route},
};

/// Durable Object namespace binding of the `room` class (one object per endpoint).
pub const ROOMS_BINDING: &str = "ROOMS";
/// Internal mint path, reachable only through a DO stub.
pub const INTERNAL_MINT: &str = "/_internal/mint";
/// Internal status path of an endpoint object: its `/status` JSON, or 404 if unminted.
pub const INTERNAL_STATUS: &str = "/_internal/status";
/// Header carrying the router-computed z32 name on internal calls.
pub const NAME_HEADER: &str = "x-iroh-do-name";

/// Default pkarr relay (`iroh-1.4.0/src/address_lookup/pkarr.rs:127`, `N0_DNS_PKARR_RELAY_PROD`).
pub const DEFAULT_PKARR_RELAY: &str = "https://dns.iroh.link/pkarr";

/// Router → DO mint request body.
#[derive(Serialize, Deserialize)]
pub struct MintRequest {
    /// Secret key, lowercase hex. Never logged.
    pub secret_hex: String,
    /// Relay URL to publish and return.
    pub relay_url: String,
    /// pkarr relay base URL; empty disables publishing.
    pub pkarr_relay: String,
    /// Label for the endpoint list; may be empty.
    #[serde(default)]
    pub label: String,
}

/// Mint response (to the admin caller and from the DO).
#[derive(Serialize, Deserialize)]
pub struct MintResponse {
    /// EndpointId, z-base-32.
    pub id: String,
    /// Relay URL (= home relay advertised for `id`).
    pub relay_url: String,
    /// Whether the pkarr record was published and confirmed by GET.
    pub published: bool,
}

fn var(env: &Env, name: &str) -> Option<String> {
    env.var(name).ok().map(|v| v.to_string())
}

/// The router entry point.
pub async fn handle(req: Request, env: Env) -> Result<Response> {
    let Some(zone) = var(&env, "ZONE_SUFFIX") else {
        error!(event = "config_error", missing = "ZONE_SUFFIX");
        return Response::error("misconfigured", 500);
    };
    let headers = req.headers();
    // The request URL's host is authoritative (workerd derives it from Host;
    // Miniflare's dispatchFetch only preserves it in the URL).
    let host = req.url()?.host_str().unwrap_or_default().to_owned();
    let upgrade = headers.get("upgrade")?;
    let path = req.path();
    let method = req.method().to_string();
    match route::classify(
        &host,
        &method,
        &path,
        route::is_websocket_upgrade(upgrade.as_deref()),
        &zone,
    ) {
        Route::Relay(id) => {
            let stub = env
                .durable_object(ROOMS_BINDING)?
                .id_from_name(&id.to_z32())?
                .get_stub()?;
            stub.fetch_with_request(req).await
        }
        Route::Mint => mint(req, &env, &zone).await,
        Route::ListPage => {
            let html = page(&registry::list(&env).await?);
            let h = Headers::new();
            h.set("content-type", "text/html; charset=utf-8")?;
            Ok(Response::ok(html)?.with_headers(h))
        }
        Route::ListJson => Response::from_json(&registry::list(&env).await?),
        Route::Status(id) => {
            let stub = env
                .durable_object(ROOMS_BINDING)?
                .id_from_name(&id.to_z32())?
                .get_stub()?;
            stub.fetch_with_str(&format!("https://do{INTERNAL_STATUS}"))
                .await
        }
        Route::Probe(Probe::Ping) => {
            let h = Headers::new();
            h.set("Access-Control-Allow-Origin", "*")?;
            Ok(Response::empty()?.with_headers(h))
        }
        Route::Probe(Probe::Generate204) => {
            let h = Headers::new();
            if let Some(v) = route::no_content_response(headers.get("x-iroh-challenge")?.as_deref())
            {
                h.set("X-Iroh-Response", &v)?;
            }
            Ok(Response::empty()?.with_status(204).with_headers(h))
        }
        Route::UpgradeRequired => Response::error("upgrade required", 426),
        Route::MethodNotAllowed => Response::error("method not allowed", 405),
        Route::NotFound => Response::error("not found", 404),
    }
}

async fn mint(mut req: Request, env: &Env, zone: &str) -> Result<Response> {
    let Some(expected) = env
        .secret("ADMIN_TOKEN")
        .ok()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
    else {
        error!(event = "config_error", missing = "ADMIN_TOKEN");
        return Response::error("minting disabled", 503);
    };
    let auth = req.headers().get("authorization")?;
    let ok = route::bearer(auth.as_deref())
        .is_some_and(|t| route::ct_eq(t.as_bytes(), expected.as_bytes()));
    if !ok {
        warn!(event = "admin_auth", ok = false, path = %req.path());
        return Response::error("unauthorized", 401);
    }
    info!(event = "admin_auth", ok = true, path = %req.path());

    let label = match mint_label(&req.text().await?) {
        Ok(l) => l,
        Err(e) => return Response::error(e, 400),
    };

    // Generated in the isolate (crypto.getRandomValues). The secret travels only
    // from router to object inside Cloudflare and is never returned.
    let secret = SecretKey::generate();
    let id = secret.public();
    let url = req.url()?;
    let relay_url = route::relay_url_for(&id, url.scheme(), zone, url.port());
    let pkarr_relay = var(env, "PKARR_RELAY").unwrap_or_else(|| DEFAULT_PKARR_RELAY.to_string());
    let body = MintRequest {
        secret_hex: data_encoding::HEXLOWER.encode(&secret.to_bytes()),
        relay_url,
        pkarr_relay,
        label,
    };
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    headers.set(NAME_HEADER, &id.to_z32())?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(serde_json::to_string(&body)?.into()));
    let inner = Request::new_with_init(&format!("https://do{INTERNAL_MINT}"), &init)?;
    let stub = env
        .durable_object(ROOMS_BINDING)?
        .id_from_name(&id.to_z32())?
        .get_stub()?;
    stub.fetch_with_request(inner).await
}
