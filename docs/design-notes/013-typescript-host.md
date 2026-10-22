# 013: TypeScript owns the Durable Object; Rust is a wasm library

**Status:** proposed (2026-10-22). Replaces the app API of 011 and the glue
layout of 011 and 012. Narrows P5's explicit-clock claim in `spec.md` §1,
which needs review. Evidence: `docs/verification/ts-host.md`.

iroh-do is a feasibility study. This change keeps no compatibility with the
current deployment.

## Problem

Durable Objects are a TypeScript platform. Cloudflare documents its APIs
(hibernatable WebSockets, `ctx.storage.sql`, alarms, RPC) in TypeScript, and
the libraries people build objects with (partyserver, the Agents SDK) are
TypeScript base classes. iroh-do reverses this. workers-rs and worker-build
own the class, and an app implements the Rust `App` trait. Consequences:

- An app author must write Rust and build wasm, even when the protocol is a
  few lines.
- Every platform API reaches the app through a Rust wrapper (`Ctx::with_sql`,
  `Ctx::schedule`), not as the platform documents it.
- worker-build wraps the class in a JavaScript proxy that does not extend
  `DurableObject`, so Workers RPC fails, and the router talks to objects over
  `/_internal/*` fetch paths (`design.md`).

## Decision

1. **The Durable Object class is TypeScript.** A package `iroh-do` exports an
   `IrohDurableObject<Env>` base class and an `irohRouter`. The class owns the
   platform:
   - `fetch` and the relay upgrade;
   - `ctx.acceptWebSocket`, socket tags and attachments;
   - the alarm and its schedule table;
   - the identity row and counters, in `iroh_*` tables through
     `ctx.storage.sql`;
   - Workers RPC for mint, status and destroy.

   Apps use `this.ctx`, `this.env` and `ctx.storage.sql` directly.
2. **Rust is a wasm library** (`crates/iroh-do-wasm`, `wasm-bindgen
   --target web`, no workers-rs). It keeps the parts that have to be iroh:
   - the ingress session and its snapshot format;
   - the node, the endpoint, the transport and frame encoding;
   - connection tracking and the idle and silent close;
   - key generation and z-base-32 parsing;
   - the JSON log layer.

   It exposes no Cloudflare types: TypeScript passes socket ids, bytes and
   snapshot strings, and receives frames through a `sendFrame(sid, frame)`
   callback.
3. **The JS API follows `@number0/iroh`**, iroh's own Node bindings, so an iroh
   developer's stream code carries over. Same class and method names and
   signatures: `Connection` (`remoteId()`, `alpn()`, `acceptBi()`, `openBi()`,
   `acceptUni()`, `openUni()`, `close(code, reason)`, `closed()`,
   `stableId()`), `BiStream` with `send` and `recv`, `SendStream` (`write`,
   `writeAll`, `finish`, `reset`), `RecvStream` (`read`, `readExact`,
   `readToEnd`, `stop`), `EndpointId`, `EndpointAddr`. Accessors are methods.
4. **Apps register handlers by ALPN**, in the shape of iroh's
   `ProtocolHandler`:

   ```ts
   export class Echo extends IrohDurableObject<Env> {
     protocols() {
       return { "iroh-example/echo/0": { accept: (c: Connection) => this.serve(c) } };
     }
     async serve(conn: Connection) {
       const bi = await conn.acceptBi();
       await bi.send.writeAll(await bi.recv.readToEnd(65536));
       await bi.send.finish();
       await conn.closed();
     }
   }
   export default { fetch: irohRouter({ binding: "ENDPOINTS" }) };
   ```

### Differences from `@number0/iroh`

| `@number0/iroh` | iroh-do | Why |
|---|---|---|
| `Array<number>` for bytes and ALPNs | `Uint8Array` | Workers APIs use `Uint8Array`; an array of numbers costs a copy and boxing per call |
| `Endpoint.acceptNext()` loop | `protocols()` returning `{ [alpn]: { accept } }` | An object cannot wait in a loop across hibernation. This is the shape of iroh's own `ProtocolHandler` |
| `Endpoint.bind`, builder, presets | none; `this.endpoint` has `id()`, `addr()`, `connect()` | The host binds on wake, from stored identity |
| relays, discovery, paths, network watches | not exposed | One relay path, no UDP |
| `EndpointId.toString()` only | adds `toZ32()`, `fromZ32()` | URLs and DNS labels use z-base-32 (004) |
| — | no stream guard API | The wasm streams count themselves open until finished, reset, at the end or stopped, and feed the idle and silent close (005, 011) |

### Connection lifetime

The garbage collector, not the handler, decides when a JS object holding a
connection or stream is freed. To keep iroh's `Router` semantics, where
returning from `accept` drops the connection, the node closes a connection
when its handler's promise settles, unless another handler or a dial still
holds it. An app that keeps a connection, for example the chat room reusing it
for deliveries, keeps its `accept` pending until `conn.closed()`. Stream halves
abandoned without being finished are released when their connection closes,
not when they are collected. *Implemented in step 1* (`crates/iroh-do-wasm/src/app.rs`,
`iroh_do_core::Activity`): the close uses code 0 and reason `done`. An accepted connection
has exactly one handler and a dial creates a different connection, so nothing else can hold
it: the close happens whenever the handler's promise settles. A connection the app
dials with `endpoint.connect` has no handler scope; it stays open until the app or the peer
closes it, or the idle close does. Tested by `packages/iroh-do/test/lifetime-test.mjs`.

### Panics

With `panic = "abort"`, a panic traps the isolate's one wasm instance, which
every object in the isolate shares. The base class catches the
`WebAssembly.RuntimeError` and reinstantiates the module. It then marks every
object's node lost, and each object rebuilds its node on its next event from
storage and socket attachments, the same path as a wake. *Implemented and tested in
step 1* (`packages/iroh-do/test/panic-test.mjs`). The glue is generated with
`wasm-bindgen --experimental-reset-state-function`, whose `__wbg_reset_state`
re-instantiates the module and makes objects and closures of the old instance throw
instead of reaching the new one. A Rust panic hook tells the host before the trap, which
covers panics in node tasks, where no object catches the `RuntimeError`. Relay sockets
survive; open QUIC connections are lost, and their peers redial after their idle
timeout.

## P5

P5 currently reads: "Protocol logic (relay handshake, framing, lifecycle) is a
pure state machine with explicit `now` and explicit I/O, testable natively
with no Cloudflare dependency. Runtime glue is thin."

"Runtime glue is thin" still holds. The boundary between Rust and the
platform is narrower than before: Rust takes socket ids, bytes and snapshot
strings, and returns frames through one callback. The TypeScript class is not
glue in this sense. It is the platform layer, written as Cloudflare documents
it, and it is what lets app authors write Durable Objects without crossing the
Rust/TypeScript boundary themselves.

One correction is needed, and it is true before this change too: **explicit
`now` holds only for the ingress.**

- noq, inside iroh, reads the clock through n0-future and arms its own
  `setTimeout` timers. iroh 1.4.0 has no hook to inject either.
- `iroh-do-core` also reads the clock directly: the linger sleeps with
  `n0_future::time::sleep` (`src/lib.rs`), and the idle close samples
  `Instant::now()` (`src/conn.rs`).
- This is correct only for the two reasons given in `design.md` § Time:
  1. Cloudflare's clock, which advances only between events, matches how noq
     samples `now`.
  2. No QUIC connection survives a suspension, because pending timers keep the
     object from hibernating.

Proposed wording:

> **P5. Sans-io first.** The relay ingress (handshake, framing, session) is a
> pure state machine with explicit `now` and explicit I/O, testable natively
> with no Cloudflare dependency. The iroh endpoint and the node's lifecycle read
> the platform clock; `design.md` § Time states why that clock is sufficient.
> Runtime glue is thin.

### Upstream: a host-provided QUIC clock

[iroh#4459](https://github.com/n0-computer/iroh/issues/4459) proposes an
unstable endpoint-level time source: a host-provided `now()` and a sleep future
for a deadline. Its draft implementation is
[iroh#4466](https://github.com/n0-computer/iroh/pull/4466) (open and unreviewed
on 2026-10-22). If a release includes it, the TypeScript host can supply iroh's
clock. The host would read `Date.now()` at each event and arm one timeout for
the earliest QUIC deadline. That would make explicit `now` hold for the endpoint
too, and would replace the `setTimeout` round-up described in `design.md` §
Time. Until then this repository uses stock iroh 1.4.0 (P7, AGENTS.md).

The proposer reports QUIC stalling after a carrier interruption in deployed
Workers. That has not been seen here. The case that would expose this design
to such a stall is production evicting an object while a connection is open,
for example for a code update or under memory pressure. That has not been
tested in production. The peer would then reconnect to a fresh object, so the
connection is dropped rather than left stalled.

## Consequences

- **Removed:** `crates/iroh-do` (`glue/`, the `worker!` macro, router), the
  `App` trait and `AppEntry` of 011, `examples/deployment` as a Rust crate,
  worker-build and `deploy/build-worker.sh`.
- **An app is chosen per deployment, not per endpoint** (011, decision 2): each
  app is a DO class, and its Worker binds that class. One deployment can still
  export several classes. The router then needs `E → class`, which mint
  records in the registry. *Implemented in step 2*:
  - `IrohRegistry` (`packages/iroh-do/src/registry.ts`) is the registry object
    of note 009 with the same table, `endpoints(id, app, label, minted_at)`. The
    deployment exports it as class `Registry`.
  - `irohRouter({ apps, registry })` maps app names to class bindings.
    `POST /mint` takes the body the Rust router takes, `{"app", "label"}`, with
    the first app as the default. The router writes `E → app` to the registry
    before the object stores its identity, and removes it again if the mint
    fails. The Rust router registers after the mint and retries on the next
    wake; here the router cannot reach `E` without the entry, so it must exist
    before `E` is handed out.
  - Relay, status and destroy requests ask the registry for `E`'s app over
    Workers RPC. Each isolate caches the answer in a map, bounded at 10,000
    entries and cleared when full. The cache needs no invalidation because
    `E → app` never changes after mint. A destroyed endpoint's stale entry in
    another isolate routes to an object without an identity, which answers 404.
  - Alternatives not taken: encoding the app in the hostname would change the
    relay URL, which is a function of the key alone (`design.md`, Addressing).
    Trying each class in turn would wake one object per class per request.
  - Cost: a request for a valid id that is not cached wakes the registry, also
    for ids that were never minted. Step 1 woke the endpoint object instead.
    Only registered ids are cached, so repeated requests for unminted ids wake
    the registry each time.
  - Endpoints minted in step 1's `iroh-ts` are not in the registry and are not
    reachable after step 2. There is no backfill: an object that is not
    reachable cannot register itself, and this deployment keeps no
    compatibility (above).
  - Measured in Miniflare (`examples/ts-deployment/test/router-test.mjs`): in
    a new isolate, the first status request for `E` made one registry lookup,
    and a second status request and a relay session made none.
- **Build:** `cargo build` for wasm32 plus `wasm-bindgen --target web`, then
  `wrangler deploy --dry-run --outdir`. wrangler names the wasm file by its
  content hash. The build writes a manifest naming the main module and the
  wasm part, and `deploy/tofu/worker.tf` reads it for the `files` key and
  `BUNDLE_SHA256`. The provider (5.28.1) does not check part names against
  imports, so a mismatch would fail only at apply.
- **Tests:**
  - `iroh-relay-ingress` and `iroh-do-harness` keep the native tests.
  - The harness tests the node with a Rust handler, as `library.rs` does.
  - TypeScript apps are tested in Miniflare against stock peers, as
    `examples/echo/server/test/evict-test.mjs` does.
  - `tests/workerd/` is rewritten for the new bundle.
- **Examples:**
  - `examples/echo` becomes a TypeScript class.
  - `examples/chat/server` becomes a TypeScript class. *Done in step 2*: class
    `Chat`, a port of `examples/chat/app` with the same replies, limits,
    error strings, log records and SQL. It schedules dial-backs and the log
    prune through `schedule` and `onAlarm`, and adds `records`, `members` and
    `present` to the status through `appStatus`, the base class's equivalents
    of `Ctx::schedule`, `App::on_alarm` and `App::status`. The Rust app stays
    deployed on the Rust host until step 3.
  - *Step 2*: one deployment, `examples/ts-deployment` (script `iroh-ts`),
    exports `Echo`, `Chat` and `Registry`. `examples/echo/server` and
    `examples/chat/server` hold the classes; each also has a single-class
    Worker for its own checks.
  - `examples/chat/common` holds the room protocol as a written spec plus golden
    request and reply fixtures. The TypeScript server's tests and the Rust
    client's tests both check the fixtures.
  - The clients do not change (P3).
- **Ported in step 1:** pkarr publishing at mint (Rust builds, signs and checks the
  record in `iroh_do_core::pkarr`; TypeScript does the `fetch`, the retries and the
  republish alarm) and destroy, as the RPC `destroy()`. Step 2 ported the registry and
  the endpoint list (above).
- **Migrations (step 2).** Adding `Chat` and `Registry` to the live `iroh-ts`
  script needs a second Durable Object migration. `deploy/tofu` takes a list of
  migrations per module and sends only the steps after the live script's tag:
  `v2` with `old_tag = "v1"` for `iroh-ts`, nothing for `iroh-do`. Reading the
  provider and OpenTofu showed that the `ignore_changes = [migrations]` of
  note 012 would make any update of `iroh-ts` fail the after-apply
  consistency check, so it is removed (`deploy/tofu/README.md`, "Verified and
  unverified").
- **`unsafe`:** removed in step 1. `iroh-do-core` has `LocalProtocolHandler`, whose
  `accept` future must be `Send` only natively; on wasm32 the node spawns handler futures
  with `spawn_local` and needs no bound. `iroh-do-wasm` registers its handlers with
  `Protocols::accept_local`.
