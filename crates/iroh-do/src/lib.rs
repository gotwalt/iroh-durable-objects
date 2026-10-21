//! The iroh-do Worker: one Cloudflare Worker script, `iroh-do-chat`, containing a
//! stateless router `fetch` handler and the Durable Object classes `room` and
//! `registry`. Each `room` object hosts one iroh endpoint `E` and serves the
//! iroh relay protocol for it at `https://<E>.<zone>/relay`, so stock iroh
//! peers reach `E` with no changes. Build instructions and the bundle layout
//! are in `BUILD.md`.
//!
//! The crate is glue. The relay protocol server side is `iroh-relay-ingress`,
//! the `CustomTransport` that feeds it into an iroh `Endpoint` is
//! `iroh-ingress-transport`, and the Cold/Warm/Active lifecycle, the demo
//! ALPNs and dial-back are `iroh-do-core`. This crate wires them to workers-rs.
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
//!    secret key in the router and forwards it to the object named by its
//!    public key, on the internal path `/_internal/mint`. The object stores the
//!    key, publishes its pkarr record, records itself in the registry, and
//!    returns `E` and its relay URL. The key is never returned and there is no
//!    export path. The body may be `{"label": "…"}` (at most 64 characters).
//! 4. `GET admin.<zone>/` and `/endpoints.json` list the minted endpoints from
//!    the `registry` object ([`registry`], design note 009).
//!    `GET admin.<zone>/status/<E>` returns the `/status` JSON of the object
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
//! The SQLite schema is in [`schema`]. Every SQLite call is
//! a synchronous `sql.exec`. An object writes nothing until it has been
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
