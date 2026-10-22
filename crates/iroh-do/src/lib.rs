//! iroh-do: iroh endpoints in Cloudflare Durable Objects, as a library.
//!
//! A deployment is one Worker script built from a crate that names its apps:
//!
//! ```ignore
//! iroh_do::worker! { apps: [iroh_do_chat_app::ChatApp, my_app::MyApp] }
//! ```
//!
//! That crate is a `cdylib` and depends on `worker` and `wasm-bindgen` directly, because the
//! workers-rs macros the expansion uses name them by absolute path. The script then contains a
//! stateless router `fetch` handler and the Durable Object classes `endpoint` and `registry`.
//! Each `endpoint` object hosts one iroh endpoint `E`, running the app it was minted with, and
//! serves the iroh relay protocol for it at `https://<E>.<zone>/relay`, so stock iroh peers
//! reach `E` with no changes. Build instructions and the bundle layout are in
//! `crates/iroh-do-worker/BUILD.md`.
//!
//! The relay protocol server side is `iroh-relay-ingress`, the `CustomTransport` that feeds it
//! into an iroh `Endpoint` is `iroh-ingress-transport`, and the Cold/Warm/Active lifecycle and
//! the app API are `iroh-do-core`. This crate wires them to workers-rs.
//!
//! # Request path
//!
//! 1. The router classifies each request by its leftmost Host label
//!    ([`route::classify`]). A label that is not a valid 52-char z-base-32
//!    EndpointId or the reserved `admin` label gets a 404 without touching an
//!    object.
//! 2. `GET <E>.<zone>/relay` with a WebSocket upgrade is forwarded unchanged
//!    to the object named `E` (the object id is `id_from_name(E as z32)`).
//! 3. `POST admin.<zone>/mint` with the `ADMIN_TOKEN` bearer token generates a
//!    secret key in the router and forwards it, with the app's name, to the
//!    object named by its public key, on the internal path `/_internal/mint`.
//!    The object stores both, publishes its pkarr record, records itself in the
//!    registry, and returns `E` and its relay URL. The key is never returned and
//!    there is no export path. The body may be `{"app": "…", "label": "…"}`; with
//!    no app, the deployment's first app is used.
//! 4. `GET admin.<zone>/` and `/endpoints.json` list the minted endpoints from
//!    the `registry` object ([`registry`], design note 009).
//!    `GET admin.<zone>/status/<E>` returns the status JSON of the object
//!    named `E`, or 404 if it was never minted.
//!
//! Router-to-object calls use `stub.fetch`, not Durable Object RPC:
//! worker-build 0.8.7 wraps the class in a JS `Proxy` instead of extending
//! `DurableObject`, and RPC fails at runtime (design note 002 §1). The router
//! never forwards `/_internal/*` from the public internet, so that path is
//! reachable only through a stub.
//!
//! # Inside the object
//!
//! Each peer relay connection is a hibernatable WebSocket. Its ingress session
//! state is snapshotted into the socket's attachment, so it survives
//! hibernation. Authenticated datagrams go to `iroh-do-core`'s `Node`, which
//! builds an `Endpoint` on demand and drops it when idle.
//!
//! The constructor rebuilds all in-memory state from SQLite and
//! `getWebSockets()` attachments. It runs after hibernation, after eviction,
//! and also in place after a wasm panic (the worker-build shim re-instantiates
//! the module), so an in-memory `Endpoint` can vanish at any point.
//!
//! The library's SQLite tables are in [`schema`]; the app's come from its `App::schema`.
//! Every SQLite call is a synchronous `sql.exec`. An object writes nothing until it has been
//! minted, so probing an unminted name does not create storage.
//!
//! Every log line is a `tracing` event written as one JSON object ([`log`],
//! design note 010).
//!
//! Only [`log`], [`route`], [`pkarr`], [`schema`] and [`registry`] build natively and
//! carry the unit tests; the workers-rs glue is compiled for `wasm32` only.
#![deny(missing_docs)]

pub mod log;
pub mod pkarr;
pub mod registry;
pub mod route;
pub mod schema;

#[cfg(target_arch = "wasm32")]
mod glue;

pub use iroh_do_core::{App, AppEntry};

#[cfg(target_arch = "wasm32")]
pub use glue::{Object, Registry, handle};

/// Declares a deployment: the Durable Object classes `endpoint` and `registry`, and the
/// Worker's `fetch` handler, for the apps listed.
///
/// ```ignore
/// iroh_do::worker! { apps: [iroh_do_chat_app::ChatApp, my_app::MyApp] }
/// ```
///
/// Minting without an app name uses the first. Invoke it once, in the deployment's `cdylib`
/// crate, which must depend on `worker` and `wasm-bindgen` directly. Off wasm32 it expands to
/// nothing.
///
/// The classes are declared here, in the caller, so that the app list can be a list of
/// types; workers-rs names a class after a concrete Rust type, so the library cannot declare
/// it generically. The expansion is a named module because `#[event(fetch)]` refers to its
/// glue through `super`, which an anonymous `const _` block would hide (design note 011,
/// phase 0).
#[macro_export]
macro_rules! worker {
    { apps: [$($app:ty),+ $(,)?] } => {
        #[cfg(target_arch = "wasm32")]
        #[doc(hidden)]
        pub mod __iroh_do_worker {
            #[allow(unused_imports)]
            use super::*;
            use ::worker::{
                Context, DurableObject, Env, Error, Request, Response, Result, State, WebSocket,
                WebSocketIncomingMessage, durable_object, event,
            };

            static APPS: &[$crate::AppEntry] = &[$($crate::AppEntry::of::<$app>()),+];

            /// One iroh endpoint per object.
            #[allow(non_camel_case_types)]
            #[durable_object]
            pub struct endpoint($crate::Object);

            impl DurableObject for endpoint {
                fn new(state: State, env: Env) -> Self {
                    Self($crate::Object::new(state, env, APPS))
                }

                async fn fetch(&self, req: Request) -> Result<Response> {
                    self.0.fetch(req).await
                }

                async fn websocket_message(
                    &self,
                    ws: WebSocket,
                    msg: WebSocketIncomingMessage,
                ) -> Result<()> {
                    self.0.websocket_message(ws, msg).await
                }

                async fn websocket_close(
                    &self,
                    ws: WebSocket,
                    code: usize,
                    reason: String,
                    clean: bool,
                ) -> Result<()> {
                    self.0.websocket_close(ws, code, reason, clean).await
                }

                async fn websocket_error(&self, ws: WebSocket, error: Error) -> Result<()> {
                    self.0.websocket_error(ws, error).await
                }

                async fn alarm(&self) -> Result<Response> {
                    self.0.alarm().await
                }
            }

            /// The list of minted endpoints (design note 009).
            #[allow(non_camel_case_types)]
            #[durable_object]
            pub struct registry($crate::Registry);

            impl DurableObject for registry {
                fn new(state: State, env: Env) -> Self {
                    Self($crate::Registry::new(state, env))
                }

                async fn fetch(&self, req: Request) -> Result<Response> {
                    self.0.fetch(req).await
                }
            }

            #[event(fetch)]
            async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
                $crate::handle(req, env, APPS).await
            }
        }
    };
}
