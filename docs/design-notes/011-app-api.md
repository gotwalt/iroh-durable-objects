# 011: iroh-do as a library; apps chosen per endpoint

**Status:** proposed (2026-10-22). Amends 006 (crate boundaries) and 009 (the
endpoint list). No principle in `spec.md` §1 changes; the new API is justified
under P6 by its second consumer, the echo service below.

iroh-do is a feasibility study. This change keeps no compatibility with the
current deployment: Cloudflare resources are renamed, and every minted endpoint
and its history is destroyed when it ships.

## Problem

The plumbing that makes an iroh endpoint live in a Durable Object (relay
ingress, custom transport, Cold/Warm/Active lifecycle, hibernatable sessions,
identity, mint and destroy, pkarr, alarms, the timer fix, logs) is about 60% of
the code. Every seam it exposes to an application is shaped by the chat demo:

- `iroh_do_core::Host` has 14 methods. Three are general: `send_datagrams`,
  `schedule_callback` and `endpoint_built`. Eleven are the chat's log and
  members.
- `protocols::serve` dispatches on a fixed `match` over four ALPNs, and
  `endpoint::bind` repeats the list.
- `Node` owns `room::Room`, `dial_back` always uses `NOTIFY_ALPN`, and `Stats`
  counts room deliveries.
- The idle close depends on handlers calling `Activity::stream` and `touch`.
  An iroh `ProtocolHandler` written for another host does neither, so its
  connections would be closed as idle while in use.
- The Worker's alarm dispatch (`run_due`) knows `callback:` rows. Its schema
  includes `log` and `members`. Its object class is `room`, its script
  `iroh-do-chat`.

A second app today means copying the Worker crate and editing all of the above.

## Decision

1. The library runs any set of iroh `ProtocolHandler`s in a Durable Object. An
   app supplies handlers, its own SQL schema and alarm work, and nothing else.
2. **The app is a property of the endpoint, not of the deployment.** One
   Worker script and one Durable Object class contain every app the deployment
   was built with. Minting an endpoint names its app; the object stores the
   name with its identity and runs that app on every wake. All endpoints share
   the flat namespace `https://<E>.<zone>/`. EndpointIds cannot collide, and
   routing does not change: label `E` → `idFromName(E)` → object.
3. The chat room is the first app. Its wire protocols do not change, because
   nothing requires it, but its Cloudflare names and storage do.
4. An echo service is the second app. Its protocol handler is copied unchanged
   from iroh's `examples/echo.rs`. It shows what an iroh developer writes to
   put an existing protocol in a Durable Object, and tests that the library
   needs nothing from the chat.

Why not one script per app: a Cloudflare route such as `*.<zone>/*` sends
every label to one script. Separate scripts under one namespace would need a
dispatcher Worker with an `E → app` lookup on every relay upgrade, or a
hostname level per app, which needs its own certificate. The costs of one
script: deploying any app redeploys all of them, which restarts every live
object, and all apps share one bundle and its size limit. Both are acceptable
for this study. Someone running their own apps builds their own script, on
their own zone.

## What an app author writes

The echo app:

```rust
// apps/echo/src/lib.rs
use iroh::{endpoint::Connection, protocol::{AcceptError, ProtocolHandler}};
use iroh_do::{App, Ctx, Protocols};

/// Copied from iroh 1.4.0 `examples/echo.rs`.
pub const ALPN: &[u8] = b"iroh-example/echo/0";

#[derive(Debug, Clone)]
pub struct Echo;

impl ProtocolHandler for Echo {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = connection.accept_bi().await?;
        tokio::io::copy(&mut recv, &mut send).await?;
        send.finish()?;
        connection.closed().await;
        Ok(())
    }
}

pub struct EchoApp;

impl App for EchoApp {
    const NAME: &'static str = "echo";
    fn new(_ctx: &Ctx) -> Self { EchoApp }
    fn protocols(&self, _ctx: &Ctx, p: &mut Protocols) {
        p.accept(ALPN, Echo);
    }
}
```

The deployment, a separate crate that lists its apps:

```rust
// crates/iroh-do-worker/src/lib.rs
iroh_do::worker! { apps: [iroh_do_chat_app::ChatApp, iroh_do_echo::EchoApp] }
```

Someone deploying only their own app can put the `worker!` line in the app
crate itself.

The workflow, on a new app:

1. Create a crate that depends on `iroh-do` and `iroh =1.4.0`. Write or copy
   `ProtocolHandler`s and implement `App`.
2. Test natively against stock peers: `Harness::mint::<EchoApp>` creates an
   object, hibernates and evicts it on demand, and gives a stock `Endpoint` its
   relay URL (see [Native testing](#native-testing)).
3. Add the app to a `worker!` list.
4. `just local` builds and starts `wrangler dev`; `just mint echo` mints an
   endpoint running echo and prints `E`.
5. Dial it with a stock iroh client: in production by `E` alone, locally with
   `--relay http://<E>.localhost:<port>/`, because nothing is published for
   `localhost`.
6. `just up` deploys the script with OpenTofu; `just mint echo` works the same
   against production.

## Library API

### `App`

```rust
pub trait App: 'static {
    /// Name given at mint and stored with the identity; unique within a `worker!` list.
    const NAME: &'static str;
    /// Called once per object construction: after hibernation, eviction or a panic.
    fn new(ctx: &Ctx) -> Self;
    /// Called on every activation, before the endpoint binds. The endpoint accepts exactly
    /// the ALPNs registered here.
    fn protocols(&self, ctx: &Ctx, p: &mut Protocols);
    /// SQL run once per construction, before `new`, on minted objects only.
    fn schema() -> &'static [&'static str] { &[] }
    /// A row the app scheduled with `Ctx::schedule` is due.
    fn on_alarm(&self, ctx: &Ctx, key: &str, payload: &str) {}
    /// Extra fields for `/status` and `admin.<zone>/status/<E>`.
    fn status(&self, ctx: &Ctx) -> serde_json::Map<String, serde_json::Value> { Default::default() }
    /// Command shown for this endpoint on the admin page.
    fn join_hint(id: &str) -> String { id.to_string() }
}
```

`App` instances live for one object instance, so their state lasts across
activations. They hold what the chat currently keeps in `Shared::room` (rate
limit windows, open delivery connections). Handlers are built on every
activation, which matches iroh, where a `Router` is built per `Endpoint`.

`App` is synchronous: storage is synchronous, and async work goes through
`Ctx::spawn`.

`worker!` turns its list into a table of `(NAME, schema, constructor)`. The
object finds its stored name in the table and builds a boxed app. An object
whose stored name is not in the table refuses all traffic, as an object with a
mismatched key does now.

### `Ctx`

A cheap `Clone + Send + Sync` handle. On wasm32 it holds only the local
EndpointId and reaches runtime handles through thread-locals, as
`glue/node.rs` and `glue/links.rs` do now.

| Method | Purpose | Replaces |
|---|---|---|
| `id()` | The local EndpointId | |
| `with_sql(\|sql\| …)` | Synchronous SQL on the object's database | `Host` log and member methods |
| `schedule(key, at, payload)`, `cancel(key)` | Durable alarm work, delivered to `App::on_alarm` | `Host::schedule_callback`, `SCHED_CALLBACK` |
| `peers()`, `has_socket(peer)` | Peers with an authenticated relay socket: the only peers the object can dial | `Shared::has_socket` |
| `connect(peer, alpn).await` | Dial a peer over its socket, activating the endpoint if needed; counted for the linger like an accepted connection | `Node::dial_back`, `start_dial_locked`, room delivery dials |
| `spawn(fut)` | A counted node task, checked at teardown | `Shared::spawn` |
| `activity(&conn)` | Opt-in per-stream tracking for the `idle`/`silent` close | `protocols::Activity` |
| `wakes()`, `state()` | Lifecycle evidence | `Config::wakes`, `Node::state` |

`with_sql` takes a closure so that no SQL handle is held across an `.await`,
which a `Send` handler future could not do on wasm32 anyway. The `Sql` handle
offers `exec(query, params) -> Result<Vec<Row>>` with SQLite values only. The
Durable Object implements it over `sql.exec`, and the native harness over
`rusqlite`. This is the one new abstraction, and it has two consumers on
different runtimes.

The library owns the tables `identity` (which gains an `app` column),
`counters`, `schedule` and `pkarr_state`, and the schedule keys `republish` and
`sweep`. An app that creates or schedules one of those names gets an error.

### `Protocols`

`p.accept(alpn, handler)` takes any `iroh::protocol::ProtocolHandler`, the
same trait iroh's `RouterBuilder::accept` takes. The node keeps its own accept
loop instead of using iroh's `Router`, because it must count connections for
the linger and tasks for the teardown check. It calls `on_accepting` and then
`accept` as `Router` does, and calls `shutdown` on each handler before closing
the endpoint.

`p.accept_with(alpn, handler, Idle::after(d))` sets that ALPN's idle limit for
handlers that do not track their own streams (see below).

### Idle close without app cooperation

The node closes a connection after its ALPN's idle limit (default `t_idle`,
10 s) with no STREAM frames sent or received. It reads the counts from
`Connection::stats()` (`frame_tx.stream`, `frame_rx.stream`; noq-proto 1.4.0,
`src/connection/stats.rs`). The 5 s keep-alives are PING frames and ACKs, so
they do not reset the timer.

QUIC stats do not count open streams, so this default cannot tell a finished
connection from one holding a stream open between lines. The current
`idle`/`silent` split needs that distinction. It stays available as an opt-in:
a handler that calls `ctx.activity(&conn)` and holds a guard per open stream
gets `t_idle` with no streams open and `t_silent` with streams open, exactly as
now. The chat's handlers opt in. The lifecycle demo depends on it: its session
1 runs on ctl/1, finishes its stream, and expects the close with reason `idle`
after 10 s (`crates/iroh-do-demo/src/lifecycle.rs`, step 2), while interactive
ctl sessions pause longer than 10 s between lines. Stats-based activity and
`touch` both reset the same timer, so a handler that opts in loses nothing.

This rests on one unmeasured claim: that iroh's path keep-alives never carry
STREAM frames. Phase 0 tests it before anything else depends on it.

### The `worker!` macro

`iroh_do::worker! { apps: [A, B, …] }` expands, in the crate that invokes it,
to:

- the endpoint class `endpoint`, a `#[durable_object]` struct that delegates
  every method to `iroh_do::Object`, which holds what `glue/endpoint.rs`'s
  `room` holds now plus the boxed app;
- the registry class `registry` (design note 009);
- the app table;
- `#[event(fetch)]` calling `iroh_do::router::handle`.

The class names and the bindings `ENDPOINTS` and `REGISTRY` are fixed by the
library. They appear in the deployment's configuration (`wrangler.toml`,
`infra/main.tf`), not in app code.

The classes are emitted into the invoking crate rather than defined in the
library so that the app table, a list of concrete types, can be built there.
worker-build 0.8.7 exports every class it finds in the bundle (`src/main.rs`,
`add_export_wrappers`). The expanded code uses `::worker::` paths and a bare
`worker::Context`, so the invoking crate depends on `worker =0.8.7` and
`wasm-bindgen =0.2.129` directly; a `$crate` re-export cannot replace them.
Off wasm32 the macro expands to nothing.

The macro emits into a named module, `pub mod __iroh_do_worker { use super::*; … }`.
An anonymous `const _: () = { … }` block works for the classes but not for
`#[event(fetch)]`, which generates `use super::main_fetch_glue` and fails with
E0432 because `super` skips the anonymous block.

## Minting and the endpoint list

`POST https://admin.<zone>/mint` takes `{"app": "<NAME>", "label": "…"}`. An
unknown app is a 400. The router passes the app name to the object with the
key, and the object stores both. The registry row gains an `app` column, and
the admin page shows it with the app's `join_hint`. `just mint APP [LABEL]`
replaces `just mint [LABEL]`.

## Crates after the change

| Crate | Target | Owns | Change |
|---|---|---|---|
| `iroh-relay-ingress` | any | Relay protocol server side | none |
| `iroh-ingress-transport` | any | Custom transport | none |
| `iroh-do-core` | any | `Node`, `App`, `Ctx`, `Protocols`, `Sql`, lifecycle, idle close, dial | loses `room.rs`, `protocols.rs`'s handlers, log types, the chat parts of `Host` |
| `iroh-do` | wasm32 lib | `Object`, router, sessions, mint and destroy, pkarr, alarms, registry, logs, timer fix, `worker!` | becomes an rlib; schema loses `log` and `members` |
| `iroh-do-worker` | wasm32 cdylib | This repo's deployment: `worker! { apps: [ChatApp, EchoApp] }` | new; script `iroh-do` |
| `iroh-do-harness` | native lib | `Harness`, rusqlite `Sql` | `mint::<A>()`; lifecycle tests move to `EchoApp` |
| `apps/chat` (`iroh-do-chat-app`) | any | room, ctl, echo/1, notify, `log` and `members` schema, `/callback` | new; code moved from `iroh-do-core` and `glue/store.rs` |
| `apps/echo` (`iroh-do-echo`) | any | iroh's echo handler | new, about 40 lines |
| `apps/echo-client` (`iroh-do-echo-client`) | native | stock iroh client: `iroh-do-echo-client <E> [text]` | new |
| `iroh-do-demo` | native | the `iroh-do-chat` client | none |

App crates are ordinary rlibs that build natively, so harness tests run their
handlers against stock peers.

## Native testing

```rust
let h = Harness::start().await?;
let obj = h.mint::<EchoApp>(Timings::default());
let peer = Endpoint::builder(presets::N0).relay_mode(RelayMode::Disabled).bind().await?;
let conn = peer.connect(h.addr(&obj), echo::ALPN).await?;
// ... echo a message ...
obj.hibernate()?;                     // drops the app, Node and sessions; keeps SQLite and sockets
// ... echo again: wakes == 2, endpoint_builds == 2 ...
```

The lifecycle tests now in `harness/native/tests/node.rs` (idle close, linger,
hibernation between sessions, dial-back after hibernation, the
leftover-task check) are rewritten against `EchoApp` and a small test app that
uses `Ctx::connect` and `Ctx::schedule`. They then test the library alone. The
room tests stay with the chat app.

## Deployment

One script, `iroh-do`, built from `iroh-do-worker`, with the classes
`endpoint` and `registry` and the bindings `ENDPOINTS` and `REGISTRY`, on the
existing route `*.<zone>/*` and the existing certificate.

The script and class renames replace the deployment rather than update it:
`just down` deletes the `iroh-do-chat` script with every object and key, and
`just up` creates `iroh-do` with a fresh `v1` migration. The rooms minted so
far are gone, and are minted again with `just mint chat`.

## Plan

Each phase leaves `cargo test -p iroh-relay-ingress`,
`cargo test -p iroh-do-harness`, `node harness/workerd/evict-test.mjs` and
`LOCAL_PORT=8799 just local` passing.

0. **Spikes**, each a test or a build that is thrown away if it fails:
   - a. A native harness test that holds an idle connection for 30 s and
     checks that `frame_rx.stream` and `frame_tx.stream` do not move, then
     that one stream write moves them.
   - b. iroh's echo handler, copied unchanged (with `tokio::io::copy`), builds
     for wasm32 inside a `Send` `ProtocolHandler` future.
   - c. A `macro_rules!` in one crate that emits `#[durable_object]` and
     `#[event(fetch)]` into another, built with worker-build 0.8.7. Check that
     the bundle exports the classes and loads in `wrangler dev`.

   If (a) fails, there is no default: a handler that does not opt in to
   `ctx.activity` is closed only by `max_idle_timeout` or its peer, and the
   guide says to opt in. If (c) fails, the deployment crate writes the
   delegating structs by hand (about 30 lines) and the guide shows them.
1. **Core API.** Add `App`, `Ctx`, `Protocols`, `Sql` and stats-based idle
   close to `iroh-do-core`. Move the room, ctl, echo/1 and notify handlers into
   `apps/chat` as `ProtocolHandler`s on `Ctx`. Give the harness `mint::<A>()`
   and rusqlite `Sql`. Room and node tests pass unchanged apart from setup.
2. **Worker library.** Move `glue/endpoint.rs` into `Object`, the chat store
   into `apps/chat`, and alarm dispatch to `App::on_alarm`. Add `worker!`, the
   app table, the `app` column and mint's `app` field. Add `iroh-do-worker`;
   `iroh-do` becomes an rlib. Rename the script, classes and bindings in
   `build-worker.sh`, `wrangler.toml`, `infra/` and the `justfile`.
3. **Echo.** `apps/echo`, `apps/echo-client`, lifecycle tests on `EchoApp`,
   and a Miniflare eviction test against an echo endpoint. `just local` with
   one chat and one echo endpoint side by side.
4. **Documents.** This note to accepted, `design.md` (Overview, The router,
   The transport, Lifecycle's idle close, Persistence, Minting, Endpoint list),
   README, and a guide `docs/writing-an-app.md` that walks through the echo
   service from `cargo new` to a deployed endpoint.
5. **Production, with the user's approval for each run.** `just down` (shown
   first; it destroys every endpoint), then `just up` with a saved
   `tofu plan -out` shown before apply. Then mint one chat and one echo
   endpoint and run `just demo`, `just demo --room` and the echo client
   against them.

## Phase 0 results (2026-10-22)

All three spikes passed. The fallbacks above are not needed.

- **(a) Keep-alives and STREAM frames.** A stock iroh 1.4.0 peer held an echo
  connection to a harness object (`t_idle` raised to 120 s) for 30 s with no
  application data. Measured at the peer, which sees the object's frames as
  received: PING went from 4 to 10 in each direction, and STREAM stayed at 1
  sent and 1 received. One further echo moved STREAM to 2 in each direction.
  Other frames that moved while idle were PATH_ACK only. Measured natively over
  loopback, not on workerd.
- **(b) iroh's echo handler on wasm32.** The `Echo` struct and its
  `ProtocolHandler` impl from iroh 1.4.0 `examples/echo.rs`, verbatim
  (including its `println!`s), build for `wasm32-unknown-unknown` and natively
  with `iroh` (workspace, `unstable-custom-transports`) and
  `tokio = { default-features = false, features = ["io-util"] }`. The `accept`
  future is `Send` on wasm32. A type-erased `Arc<dyn DynHandler>` returning
  `Pin<Box<dyn Future<…> + Send + '_>>` compiles; the node spawns
  `async move { handler.accept(conn).await }` with an `Arc` clone, which is
  `'static`. Built, not run.
- **(c) The macro across crates.** A `macro_rules!` in one crate emitted two
  `#[durable_object]` classes, a runtime app table and `#[event(fetch)]` into a
  cdylib crate. worker-build 0.8.7 exported both classes, and `wrangler dev`
  (4.130.0) routed requests to each, with stable ids per name. Needed the
  named-module form described under [The `worker!` macro](#the-worker-macro).

## Risks

- **Idle timing for the chat.** The chat keeps its exact timing only if every
  handler that currently calls `Activity` moves to `ctx.activity`. `just demo`
  in phase 2 (locally) and phase 5 (production) checks this.
- **`ProtocolHandler::accept` returning early.** Under `Router`, returning
  drops the connection. The node must do the same, or connections whose
  handler returned would stay open until the idle close. Phase 1 has a test
  for it.
- **Handlers that spawn timers.** Third-party code may spawn work the node does
  not count, which would keep the object out of hibernation. The teardown
  check reports leftover node tasks only. The guide states the rule: no
  background tasks outside `Ctx::spawn`, and no timers that outlive a
  connection.
- **Protocols that dial out.** `Ctx::connect` reaches only peers with a socket
  to the object. Protocols that dial arbitrary peers (gossip, fetching from
  other providers) do not work. The guide says so first.

## Open questions

- Library crate names, if the crates are offered to n0: `iroh-do` and
  `iroh-do-core` versus a less Cloudflare-specific prefix.
- Whether `Ctx` should expose the bound `Endpoint`. Exposing it lets apps call
  `Endpoint::connect` themselves, bypassing the connection count that the
  linger depends on.
