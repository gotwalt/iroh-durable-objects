//! Rounds every `setTimeout` delay up so that iroh's timers never fire early under
//! Cloudflare's clock.
//!
//! n0-future's wasm `Sleep` (n0-future 0.3.2, `src/time.rs`, `sleep_impl` and `Sleep::reset`)
//! passes `duration.as_millis() as i32` to `globalThis.setTimeout`, truncating the sub-ms
//! remainder. In production, workerd schedules a timer at `now + ms` and shows its callback
//! exactly that time; the clock does not move again until the next I/O. A noq timer for a
//! deadline 4.7 ms away therefore fires at `now + 4 ms`, sees `now < deadline`, and re-arms with
//! `setTimeout(0)`, which never advances the clock. The connection driver spins until a packet
//! arrives or a later timer fires: up to seconds of CPU per invocation
//! (`docs/measurements/cost-profile.md`, "CPU diagnosis"). Deadlines derived from RTT (PTO,
//! loss detection, draining, pacing) are fractional, so this happens on every connection close.
//!
//! The delay n0-future passes is already truncated, so the wrapper cannot recover the
//! remainder; adding 1 ms to every delay is the smallest change that guarantees
//! `when >= deadline`. Each timer then fires at most 1 ms late, which noq tolerates because it
//! compares deadlines with the clock, not with timer events. n0-future looks `setTimeout` up on
//! `globalThis` at every call, so replacing the property covers every iroh and noq timer.
//!
//! `tests/workerd/clock-test.mjs` emulates the production clock and fails without this.

use std::cell::Cell;

use wasm_bindgen::{JsCast, JsValue, closure::Closure};

thread_local! {
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// Replaces `globalThis.setTimeout` with a wrapper that adds 1 ms to the delay, once per
/// isolate.
pub(crate) fn install() {
    if INSTALLED.with(|i| i.replace(true)) {
        return;
    }
    let global = js_sys::global();
    let key = JsValue::from_str("setTimeout");
    let Ok(original) =
        js_sys::Reflect::get(&global, &key).and_then(|f| f.dyn_into::<js_sys::Function>())
    else {
        tracing::error!(event = "timer_round_up_failed", error = "no setTimeout");
        return;
    };
    let this = global.clone();
    let wrapper = Closure::<dyn Fn(JsValue, JsValue) -> JsValue>::new(
        move |callback: JsValue, ms: JsValue| {
            // Negative and non-numeric delays mean 0 to `setTimeout`.
            let ms = ms
                .as_f64()
                .filter(|m| m.is_finite() && *m > 0.0)
                .unwrap_or(0.0);
            let delay = JsValue::from_f64(ms.floor() + 1.0);
            original
                .call2(&this, &callback, &delay)
                .unwrap_or(JsValue::UNDEFINED)
        },
    );
    match js_sys::Reflect::set(&global, &key, wrapper.as_ref()) {
        Ok(true) => wrapper.forget(),
        _ => tracing::error!(
            event = "timer_round_up_failed",
            error = "setTimeout not writable"
        ),
    }
}
