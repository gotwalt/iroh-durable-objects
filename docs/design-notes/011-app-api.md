# 011: iroh-do as a library; apps chosen per endpoint

**Status:** accepted (2026-10-22). Amends 006 (crate boundaries), 007 (where the
message log lives) and 009 (the endpoint list). No principle in `spec.md` §1
changes; the app API is justified under P6 by its second consumer, the echo app.

iroh-do is a feasibility study. This change keeps no compatibility with the
earlier deployment: Cloudflare resources are renamed, and every minted endpoint
and its history is destroyed when it ships.

## Problem

The plumbing that makes an iroh endpoint live in a Durable Object (relay
ingress, custom transport, Cold/Warm/Active lifecycle, hibernatable sessions,
identity, mint and destroy, pkarr, alarms, the timer fix, logs) was about 60% of
the code. Every seam it exposed to an application was shaped by the chat demo:

- `iroh_do_core::Host` had 14 methods. Three were general: `send_datagrams`,
  `schedule_callback` and `endpoint_built`. Eleven were the chat's log and
  members.
- `protocols::serve` dispatched on a fixed `match` over four ALPNs, and
  `endpoint::bind` repeated the list.
- `Node` owned `room::Room`, `dial_back` always used `NOTIFY_ALPN`, and `Stats`
  counted room deliveries.
- The idle close depended on handlers calling `Activity::stream` and `touch`.
  An iroh `ProtocolHandler` written for another host does neither, so its
  connections would have been closed as idle while in use.
- The Worker's alarm dispatch (`run_due`) knew `callback:` rows. Its schema
  included `log` and `members`. Its object class was `room`, its script
  `iroh-do-chat`.

A second app meant copying the Worker crate and editing all of the above.

## Decision

1. The library runs any set of iroh `ProtocolHandler`s in a Durable Object. An
   app supplies handlers, its own SQL schema and alarm work, and nothing else.
2. **The app is a property of the endpoint, not of the deployment.** One
   Worker script and one Durable Object class contain every app the deployment
   was built with. Minting an endpoint names its app; the object stores the
   name with its identity and runs that app on every wake. All endpoints share
   the flat namespace `https://<E>.<zone>/`. EndpointIds cannot collide, and
   routing does not change: label `E` → `idFromName(E)` → object.
3. The chat room is the first app. Its wire protocols do not change; its
   Cloudflare names and storage do.
4. An echo app is the second. Its protocol handler is copied unchanged from
   iroh's `examples/echo.rs`. It shows what an iroh developer writes to put an
   existing protocol in a Durable Object, and tests that the library needs
   nothing from the chat.

Why not one script per app: a Cloudflare route such as `*.<zone>/*` sends every
label to one script. Separate scripts under one namespace would need a
dispatcher Worker with an `E → app` lookup on every relay upgrade, or a hostname
level per app, which needs its own certificate. The costs of one script:
deploying any app redeploys all of them, which restarts every live object, and
all apps share one bundle and its size limit. Both are acceptable for this
study. Someone running their own apps builds their own script, on their own
zone.

## What an app author writes

The echo app (`apps/echo/src/lib.rs`), apart from the handler, which is iroh's:

```rust
use iroh_do::{App, Ctx, Protocols};

pub const ALPN: &[u8] = b"iroh-example/echo/0";

pub struct EchoApp;

impl App for EchoApp {
    const NAME: &'static str = "echo";
    fn new(_ctx: &Ctx) -> Self { EchoApp }
    fn protocols(&self, _ctx: &Ctx, p: &mut Protocols) {
        p.accept(ALPN, Echo);
    }
    fn join_hint(id: &str) -> String {
        format!("iroh-do-echo-client {id}")
    }
}
```

A deployment is a `cdylib` crate that lists its apps
(`crates/iroh-do-worker/src/lib.rs`):

```rust
iroh_do::worker! { apps: [iroh_do_chat_app::ChatApp, iroh_do_echo::EchoApp] }
```

`docs/writing-an-app.md` walks through the workflow.

## Library API

The API is in `iroh-do-core` and re-exported by `iroh-do`.

### `App`

`App: Send + Sync + 'static` has `NAME`, `new(ctx)`, `protocols(ctx, p)`, and
optional `schema()`, `on_alarm(ctx, key, payload)`, `status(ctx)` and
`join_hint(id)` (`crates/iroh-do-core/src/app.rs`).

The host builds one app value per object construction, so it lives across
activations but not across hibernation or eviction. Handlers are built on every
activation, before the endpoint binds, which matches iroh, where a `Router` is
built per `Endpoint`. The endpoint accepts exactly the ALPNs registered.

`AppEntry::of::<A>()` is a `const fn` holding `A`'s functions, so a host keeps
its apps in a `static` and picks one by name at run time. The node holds the
app as `Arc<dyn DynApp>`; `Node` is not generic.

### `Ctx`

A `Clone + Send + Sync` handle holding the local EndpointId and a weak
reference to the node, so an app that keeps one creates no cycle
(`crates/iroh-do-core/src/ctx.rs`).

| Method | Purpose |
|---|---|
| `id()` | The local EndpointId |
| `with_sql(\|sql\| …)` | Synchronous SQL on the object's database |
| `schedule(key, delay, payload)`, `cancel(key)` | Durable alarm work, delivered to `App::on_alarm` |
| `peers()`, `has_socket(peer)` | Peers with an authenticated relay socket: the only peers the object can dial |
| `connect(peer, alpn).await` | Dial a peer over its socket, binding the endpoint first if needed |
| `spawn(fut)` | A node task that keeps the endpoint bound while it runs |
| `activity(&conn)` | Opt-in activity reports for the idle close |
| `wakes()`, `state()`, `status()` | Lifecycle evidence |

`with_sql` takes a closure so that no SQL handle is held across an `.await`.
`Sql::exec(query, params)` returns rows of SQLite values. The Durable Object
implements it over `sql.exec` (`crates/iroh-do/src/glue/node.rs`), the native
harness over `rusqlite`.

The node prefixes app schedule keys with `app:` (`APP_KEY_PREFIX`), so they
cannot collide with the host's `republish` and `sweep`. The library owns the
tables `identity`, `counters`, `schedule` and `pkarr_state`
(`crates/iroh-do/src/schema.rs`).

### `Protocols` and dispatch

`p.accept(alpn, handler)` takes any `iroh::protocol::ProtocolHandler`, the trait
iroh's `RouterBuilder::accept` takes. The node keeps its own accept loop, because
it counts connections for the linger and tasks for the teardown check, and calls
`on_accepting` and then `accept` as `Router` does
(`crates/iroh-do-core/src/conn.rs`, `handle_incoming`). Before
closing the endpoint it calls each handler's `shutdown`.

Every connection, accepted or dialed, gets a watcher task that holds only a weak
handle. Dropping every clone of a connection therefore closes it, as under
`Router`, and a handler returning from `accept` drops its connection unless the
app kept a clone.

### The endpoint stays bound while there is work

The endpoint lingers only when nothing is busy: no open connection, no
handshake in progress, no pending `Ctx::connect` and no task spawned with
`Ctx::spawn`. Counting app tasks replaces the chat's explicit
`connection_started` around a delivery: a delivery task now keeps the endpoint
up because it is a task.

### Idle close

The node closes a connection after its idle limit without application data
(`crates/iroh-do-core/src/conn.rs`). It observes data in one of two ways:

- **By default, sampled STREAM frame counts.** The watcher reads
  `frame_tx.stream + frame_rx.stream` from `Connection::stats()` every quarter of
  the shorter limit. iroh's keep-alives are PING and ACK frames and leave the
  counts alone (phase 0 (a)). A connection is closed between one and one and a
  quarter limits after its last application data.
- **Reported activity.** A handler that calls `Ctx::activity(&conn)` and touches
  it, or holds an `Activity::stream` guard per open stream, gets exact timing:
  once a handler has reported, the watcher stops sampling. Reports also
  distinguish a connection with a stream open, which closes after `t_silent`,
  from one without, which closes after `t_idle`. QUIC statistics cannot.

`p.accept_with(alpn, handler, Idle::after(d))` sets an ALPN's idle limit in
place of `t_idle`, for accepted and dialed connections alike.

The chat reports activity, so its timing is the same as before: on local workerd
the lifecycle demo's ctl connection closed after 10.0 s. Before reported
activity skipped sampling it closed after 12.5 s, which is why it does.

### The `worker!` macro

`iroh_do::worker! { apps: [A, B, …] }` expands, in the crate that invokes it
(`crates/iroh-do/src/lib.rs`), to a named module containing:

- the class `endpoint`, a `#[durable_object]` struct delegating every handler to
  `iroh_do::Object` (`crates/iroh-do/src/glue/endpoint.rs`);
- the class `registry`, delegating to `iroh_do::Registry`;
- `static APPS: &[AppEntry]`, the app table;
- `#[event(fetch)]`, calling `iroh_do::handle(req, env, APPS)`.

The classes are emitted into the invoking crate so that the app table, a list of
concrete types, is built there. worker-build 0.8.7 exports every class it finds
in the bundle (`src/main.rs`, `add_export_wrappers`). The workers-rs macros use
`::worker::` paths and a bare `worker::Context`, so the invoking crate depends
on `worker =0.8.7` and `wasm-bindgen =0.2.129` directly. The module is named
because `#[event(fetch)]` generates `use super::main_fetch_glue`, which fails
with E0432 inside an anonymous `const _: () = { … }` block. Off wasm32 the macro
expands to nothing.

## Minting and the endpoint list

`POST https://admin.<zone>/mint` takes `{"app": "<NAME>", "label": "…"}`. With no
app, the deployment's first app is used; an unknown app is a 400 that lists the
deployment's apps. The router passes the name to the object with the key; the
object refuses a name not in its table, stores it in `identity.app`, creates
that app's schema and returns it in the response. An object whose stored app is
missing from the running build refuses all traffic, as an object with a
mismatched key does.

The registry row gains an `app` column. The admin page shows it and each app's
`join_hint`, and its status button shows the node's state and wakes plus any
fields the app adds.

`just mint APP [LABEL]` replaces `just mint [LABEL]`; `LOCAL=1 just mint APP`
mints in `just local-up`'s workerd. `just echo E [TEXT]` runs the echo client.

## Crates

| Crate | Target | Owns |
|---|---|---|
| `iroh-relay-ingress` | any | Relay protocol server side (unchanged) |
| `iroh-ingress-transport` | any | Custom transport (unchanged) |
| `iroh-do-core` | any | `Node`, `App`, `AppEntry`, `Ctx`, `Protocols`, `Idle`, `Activity`, `Sql`, lifecycle, dispatch, idle close |
| `iroh-do` | rlib; glue on wasm32 | `Object`, `Registry`, the router, sessions, mint and destroy, pkarr, alarms, logs, the timer fix, `worker!`; the library's tables |
| `iroh-do-worker` | wasm32 cdylib | This repository's deployment, script `iroh-do`; `build-worker.sh`, `BUILD.md` |
| `iroh-do-harness` | native | `Harness::mint::<A>()`, rusqlite `Sql`, hibernate and evict |
| `apps/chat` (`iroh-do-chat-app`) | any | room, ctl, echo/1, notify; the `log`, `log_meta` and `members` tables |
| `apps/echo` (`iroh-do-echo`) | any | iroh's echo handler |
| `apps/echo-client` (`iroh-do-echo-client`) | native | stock iroh peer for the echo app |
| `iroh-do-demo` | native | the `iroh-do-chat` client (unchanged) |

`iroh-do-echo-client` does not depend on the iroh-do crates: cargo would unify
their iroh features (`unstable-custom-transports`) into the peer's iroh, and the
peer must stay stock (P3).

The chat's log moved from the library's tables to the app's. Its counters
`log_seq` and `room_seq` moved from `counters` to the app's own `log_meta`.
Pruning by age, formerly run from every alarm, is scheduled by the chat itself:
each append schedules a prune for when the newest record expires.

## Cloudflare names

Script `iroh-do`, classes `endpoint` and `registry`, bindings `ENDPOINTS` and
`REGISTRY`, migration `v1` with `new_sqlite_classes = ["endpoint", "registry"]`.
The route and certificate are unchanged. The earlier script `iroh-do-chat` and
its objects are deleted with `just down` before `just up` creates the new one.

## Results

### Phase 0: spikes

- **(a) Keep-alives and STREAM frames.** A stock iroh 1.4.0 peer held an echo
  connection to a harness object (`t_idle` raised to 120 s) for 30 s with no
  application data. Measured at the peer: PING went from 4 to 10 in each
  direction, STREAM stayed at 1 sent and 1 received, and the only other frame
  that moved was PATH_ACK. One more echo moved STREAM to 2 in each direction.
  Measured natively over loopback.
- **(b) iroh's echo handler on wasm32.** Built verbatim for
  `wasm32-unknown-unknown` with `tokio` `io-util` only; the `accept` future is
  `Send` on wasm32.
- **(c) The macro across crates.** worker-build 0.8.7 exported both classes,
  and `wrangler dev` 4.130.0 routed requests to each.

### Phases 1–3

- `cargo test -p iroh-do-harness`: all tests pass, including the new
  `tests/library.rs` (echo across hibernation; idle close of a handler that
  reports nothing, 2 s limit; scheduled dial after hibernation;
  `Ctx::connect` without a socket).
- `node harness/workerd/{evict,clock,destroy,registry}-test.mjs`: all pass. The
  eviction test's new case E: an echo round trip, forced eviction in 8 ms, and a
  round trip on the new object.
- `LOCAL_PORT=8799 just local`: all 26 checks pass on workerd.
- Echo on local workerd with the stock client: first connection 39 ms, round
  trip 52 ms; a second run 13 ms and 17 ms. The object returned to Cold once the
  client's socket closed.
- Adding the echo app grew `index_bg.wasm` from 3,781,864 to 3,795,846 bytes.

One defect was found and fixed on the way: the watcher's first version slept a
quarter of the current limit, so a connection first seen with a delivery stream
open (limit `t_silent`, 60 s) was rechecked only after 15 s, after the stream
had long closed. `members_exchange_messages` failed two runs in three. The step
is now a quarter of the shorter limit.

### Phase 5: production

Pending the deployment owner's approval: `just down`, then `just up` with a
saved plan shown first, then `just mint chat`, `just mint echo`, `just demo`,
`just demo --room` and the echo client by EndpointId alone.

## Risks

- **Handlers that spawn their own tasks.** A handler that spawns with
  `tokio::spawn` or `n0_future::task::spawn` instead of `Ctx::spawn` is not
  counted. If its task outlives its connection, the endpoint may close under
  it, and any timer it holds keeps the object from hibernating.
- **Protocols that dial out.** `Ctx::connect` reaches only peers with a socket
  to the object. Protocols that dial arbitrary peers (gossip, fetching from
  other providers) do not work.
- **One bundle for all apps.** A redeploy restarts every object of every app.

## Open questions

- Library crate names, if the crates are offered to n0: `iroh-do` and
  `iroh-do-core` versus a less Cloudflare-specific prefix.
- Whether `Ctx` should expose the bound `Endpoint`. Exposing it lets apps call
  `Endpoint::connect` themselves, bypassing the busy count that the linger
  depends on.
