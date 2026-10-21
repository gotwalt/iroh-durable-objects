//! Installs the JSON `tracing` subscriber ([`crate::log`]) once per isolate.

use std::cell::Cell;

use tracing::Level;
use wasm_bindgen::prelude::*;
use worker::Env;

use crate::log::{DEFAULT_LOG_LEVEL, subscriber};

/// Plain-text binding holding the filter directives.
const LOG_LEVEL_BINDING: &str = "LOG_LEVEL";

thread_local! {
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = log)]
    fn console_log(v: &JsValue);
    #[wasm_bindgen(js_namespace = console, js_name = warn)]
    fn console_warn(v: &JsValue);
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(v: &JsValue);
}

/// Parses the line back into a JS object: Workers Logs indexes the fields of a logged object,
/// not of a string.
fn write(level: Level, line: &str) {
    let v = js_sys::JSON::parse(line).unwrap_or_else(|_| JsValue::from_str(line));
    match level {
        Level::ERROR => console_error(&v),
        Level::WARN => console_warn(&v),
        _ => console_log(&v),
    }
}

/// Installs the subscriber with the `LOG_LEVEL` binding's directives, the first time it is
/// called in this isolate.
pub(crate) fn install(env: &Env) {
    if INSTALLED.with(|i| i.replace(true)) {
        return;
    }
    let directives = env
        .var(LOG_LEVEL_BINDING)
        .map(|v| v.to_string())
        .unwrap_or_else(|_| DEFAULT_LOG_LEVEL.to_owned());
    let (sub, error) = subscriber(&directives, write);
    if tracing::subscriber::set_global_default(sub).is_err() {
        return;
    }
    if let Some(error) = error {
        tracing::warn!(
            event = "log_level_invalid",
            value = %directives,
            error = %error,
            using = DEFAULT_LOG_LEVEL,
        );
    }
}
