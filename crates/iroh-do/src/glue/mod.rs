//! workers-rs glue (wasm32 only).
//!
//! The Durable Object classes are exported to JS as `room` and `registry`. workers-rs 0.8.7's
//! `#[durable_object]` names the JS class after the Rust type and offers no `js_name`, so
//! the types themselves are lowercase.

// `#[durable_object]` generates undocumented public wasm-bindgen methods.
#[allow(missing_docs)]
mod endpoint;
mod links;
mod logging;
mod node;
mod pkarr_publish;
// `#[durable_object]` generates undocumented public wasm-bindgen methods.
#[allow(missing_docs)]
mod registry;
mod router;
mod store;
mod timers;

use worker::{Context, Env, Request, Response, Result, event};

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    logging::install(&env);
    timers::install();
    router::handle(req, env).await
}
