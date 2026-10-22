//! iroh-do's node as a wasm library for a TypeScript Durable Object (design note 013).
//!
//! The TypeScript package `packages/iroh-do` owns the platform: it accepts the hibernatable
//! WebSockets and keeps their attachments, and it owns SQLite, the alarm and `env`. This crate
//! keeps the parts that have to be iroh: the relay ingress sessions ([`iroh_relay_ingress`]),
//! the iroh endpoint over `iroh_ingress_transport`, the Cold/Warm/Active lifecycle of
//! [`iroh_do_core::Node`], key generation, z-base-32, the pkarr record and the JSON log layer.
//! It exposes no Cloudflare types: TypeScript passes socket ids, bytes and snapshot strings,
//! and receives frames through `NodeHost.sendFrame`.
//!
//! The JavaScript API follows `@number0/iroh` 1.1.0 (`Connection`, `BiStream`, `SendStream`,
//! `RecvStream`, `EndpointId`, `EndpointAddr`), with `Uint8Array` for bytes.
//!
//! All objects of an isolate share one instance of this module. With `panic = "abort"`, a
//! panic traps that instance; see [`set_panic_handler`] for how the host recovers.
//!
//! wasm32-unknown-unknown only. Natively the crate is empty, so that workspace builds pass.
#![cfg(target_arch = "wasm32")]

mod app;
mod host;
mod ids;
mod node;
mod streams;

// The JSON tracing layer of design note 010, shared with the workers-rs host. Included by
// path because that crate depends on `worker`, which this one must not.
#[path = "../../iroh-do/src/log.rs"]
#[allow(dead_code)]
mod log;

use std::cell::RefCell;

pub use ids::{EndpointAddr, EndpointId};
use iroh_do_core::pkarr;
use js_sys::{Object, Reflect};
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
/// note 010). Only the first call in an instance has an effect.
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

thread_local! {
    static ON_PANIC: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
}

/// Calls `handler(message)` when this instance panics, before it traps.
///
/// With `panic = "abort"` the panic ends in a `WebAssembly.RuntimeError` (`unreachable`),
/// which reaches JavaScript wherever the wasm call came from: a method the object called, or
/// a microtask or timer of a node task, where no object can catch it. The handler is how the
/// host learns of the second kind. It must not call into this module: the instance is about
/// to trap, and its state is not consistent.
#[wasm_bindgen(js_name = setPanicHandler)]
pub fn set_panic_handler(handler: js_sys::Function) {
    ON_PANIC.with(|p| *p.borrow_mut() = Some(handler));
    std::panic::set_hook(Box::new(|info| {
        let message = info.to_string();
        let handler = ON_PANIC.with(|p| p.try_borrow().ok().and_then(|h| h.clone()));
        if let Some(h) = handler {
            let _ = h.call1(&JsValue::NULL, &JsValue::from_str(&message));
        }
    }));
}

/// Panics, for the panic-recovery tests. Built only with the `test-panic` feature, which
/// production builds do not enable. With `inTask` the panic happens in a node-style task
/// (`spawn_local`) on a later microtask, out of reach of the caller's `try`.
#[cfg(feature = "test-panic")]
#[wasm_bindgen(js_name = testPanic)]
pub fn test_panic(in_task: bool) {
    if in_task {
        wasm_bindgen_futures::spawn_local(async { panic!("test panic in a task") });
    } else {
        panic!("test panic");
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
    Ok(EndpointId(secret_key(secret)?.public()))
}

fn secret_key(secret: &[u8]) -> Result<iroh::SecretKey, JsError> {
    let arr: [u8; 32] = secret
        .try_into()
        .map_err(|_| JsError::new("secret key must be 32 bytes"))?;
    Ok(iroh::SecretKey::from_bytes(&arr))
}

#[wasm_bindgen(typescript_custom_section)]
const PKARR_TS: &str = r#"
/** A signed pkarr record and where to publish it (`pkarrRecord`). */
export interface PkarrRecord {
  /** `<pkarr relay>/<z32>`: PUT the body here, then GET it back. */
  url: string;
  /** The signed packet without its leading public key. */
  body: Uint8Array;
  /** The packet's timestamp in microseconds, for `pkarrConfirm`. */
  timestampUs: number;
}
"#;

/// Builds the endpoint's discovery record: its relay URL, signed with `secret`, in the format
/// iroh's own `PkarrPublisher` writes (`iroh_do_core::pkarr`, design note 004).
#[wasm_bindgen(js_name = pkarrRecord, unchecked_return_type = "PkarrRecord")]
pub fn pkarr_record(secret: &[u8], relay_url: &str, pkarr_relay: &str) -> Result<JsValue, JsError> {
    let req = pkarr::build_publish(&secret_key(secret)?, relay_url, pkarr_relay)
        .map_err(|e| JsError::new(&format!("pkarr record: {e:?}")))?;
    let out = Object::new();
    let set = |k: &str, v: JsValue| Reflect::set(&out, &JsValue::from_str(k), &v);
    set("url", req.url.into()).map_err(|_| JsError::new("set url"))?;
    set("body", js_sys::Uint8Array::from(req.body.as_slice()).into())
        .map_err(|_| JsError::new("set body"))?;
    set("timestampUs", JsValue::from_f64(req.timestamp_us as f64))
        .map_err(|_| JsError::new("set timestampUs"))?;
    Ok(out.into())
}

/// Checks the body of the GET that follows the PUT: a record signed by `secret`'s key, at least
/// as recent as `minTimestampUs`, advertising `relayUrl`. A stale PUT also answers 204, so only
/// this confirms the publish.
#[wasm_bindgen(js_name = pkarrConfirm)]
pub fn pkarr_confirm(
    secret: &[u8],
    body: &[u8],
    min_timestamp_us: f64,
    relay_url: &str,
) -> Result<bool, JsError> {
    Ok(pkarr::confirm(
        &secret_key(secret)?,
        body,
        min_timestamp_us as u64,
        relay_url,
    ))
}

pub(crate) fn js_err(e: impl std::fmt::Display) -> JsValue {
    JsError::new(&e.to_string()).into()
}
