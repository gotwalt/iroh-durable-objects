//! Spike: iroh-do as a wasm library for a TypeScript Durable Object (`spikes/ts-host`).
//!
//! The TypeScript class owns the platform: it accepts the hibernatable WebSockets, keeps
//! their attachments, owns SQLite, the alarm and `env`. This crate keeps what has to be
//! Rust: the relay ingress sessions ([`iroh_relay_ingress`]), the iroh endpoint over
//! [`iroh_ingress_transport`](iroh_do_core) and the lifecycle in [`iroh_do_core::Node`].
//!
//! The JavaScript API follows `@number0/iroh` 1.1.0 (`Connection`, `BiStream`, `SendStream`,
//! `RecvStream`, `EndpointId`, `EndpointAddr`), with `Uint8Array` for bytes.
//!
//! wasm32-unknown-unknown only. Natively the crate is empty.
#![cfg(target_arch = "wasm32")]

mod app;
mod host;
mod ids;
mod node;
mod streams;

// The JSON tracing layer of design note 010, shared with the workers-rs host without
// depending on that crate (which pulls in `worker`).
#[path = "../../iroh-do/src/log.rs"]
#[allow(dead_code)]
mod log;

pub use ids::{EndpointAddr, EndpointId};
pub use node::{Endpoint, WasmNode};
pub use streams::{BiStream, Connection, RecvStream, SendStream};

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = log)]
    fn console_log(v: &JsValue);
    #[wasm_bindgen(js_namespace = console, js_name = warn)]
    fn console_warn(v: &JsValue);
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(v: &JsValue);
}

fn write_log(level: tracing::Level, line: &str) {
    // Workers Logs indexes the fields of a logged object, not of a string.
    let v = js_sys::JSON::parse(line).unwrap_or_else(|_| JsValue::from_str(line));
    match level {
        tracing::Level::ERROR => console_error(&v),
        tracing::Level::WARN => console_warn(&v),
        _ => console_log(&v),
    }
}

/// Installs the JSON `tracing` subscriber with `directives` (the `LOG_LEVEL` syntax of design
/// note 010). Only the first call in an isolate has an effect.
#[wasm_bindgen(js_name = initLogging)]
pub fn init_logging(directives: Option<String>) {
    let directives = directives.unwrap_or_else(|| log::DEFAULT_LOG_LEVEL.to_owned());
    let (sub, error) = log::subscriber(&directives, write_log);
    if tracing::subscriber::set_global_default(sub).is_ok()
        && let Some(error) = error
    {
        tracing::warn!(event = "log_level_invalid", value = %directives, error = %error);
    }
}

/// Returns 32 random bytes from the isolate's CSPRNG, for a new endpoint's secret key.
#[wasm_bindgen(js_name = generateSecretKey)]
pub fn generate_secret_key() -> Result<Vec<u8>, JsError> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| JsError::new(&e.to_string()))?;
    Ok(b.to_vec())
}

/// Returns the EndpointId of a 32-byte secret key.
#[wasm_bindgen(js_name = publicKeyOf)]
pub fn public_key_of(secret: &[u8]) -> Result<EndpointId, JsError> {
    let arr: [u8; 32] = secret
        .try_into()
        .map_err(|_| JsError::new("secret key must be 32 bytes"))?;
    Ok(EndpointId(iroh::SecretKey::from_bytes(&arr).public()))
}

pub(crate) fn js_err(e: impl std::fmt::Display) -> JsValue {
    JsError::new(&e.to_string()).into()
}
