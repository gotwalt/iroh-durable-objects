# The TypeScript host: spike, step 1 and step 2 (design note 013)

Two rounds of verification of design note 013, both on 2026-10-22, with wrangler
4.130.0, Miniflare 5.20260908.0-alpha (the version wrangler bundles), wasm-bindgen
0.2.129 and iroh 1.4.0.

- **Spike** (sections "What was built" to "Open problems"): the stock
  `iroh-do-echo-client` echoed off `class Echo extends IrohDurableObject` under
  `wrangler dev`, and the eviction test passed its 18 checks in Miniflare. The spike lived
  in `spikes/ts-host/`; its files moved as listed under "Step 1: layout".
- **Step 1** (sections "Step 1: …"): the package, the echo app and the tests, with
  connection lifetime, panic recovery, pkarr at mint, destroy over RPC and the
  eviction marker.
- **Step 2** (sections "Step 2: …", 2026-10-22, same versions): the chat app as class
  `Chat`, the registry and routing by app, and the deployment `iroh-ts` with both apps.
  Not yet deployed to production. The chat class was later rewritten against
  `examples/chat/common/PROTOCOL.md` (Vitest 5.0.3, Biome 2.5.15); the step 2 sections
  give the results for both versions where they were measured.

Paths in the spike sections are the spike's.

## What was built

- `crates/iroh-do-wasm`: a `cdylib` on `wasm-bindgen` 0.2.129, without the
  `worker` crate or worker-build. It exports:
  - `WasmNode`, which the TypeScript class drives by socket id: `upgrade`,
    `opened`, `restore`, `message`, `textMessage`, `socketClosed`, `sweep`,
    `alarm`, `status`, `tasks`.
  - `Endpoint` (`id`, `addr`, `peers`, `connect`).
  - `Connection`, `BiStream`, `SendStream`, `RecvStream`, `EndpointId` and
    `EndpointAddr`, named as in `@number0/iroh` 1.1.0, with `Uint8Array` for
    bytes. `EndpointId` also has `toZ32`/`fromZ32`.
  - `generateSecretKey`, `publicKeyOf` and `initLogging`. The JSON log layer is
    `crates/iroh-do/src/log.rs`, included by `#[path]` so that the `worker`
    dependency stays out.
- `src/iroh-do/index.ts`: the `IrohDurableObject` base class and `irohRouter`.
  `src/worker.ts`: the echo app.
- `peer/`: a test peer on stock iroh with default features and no iroh-do crates.
  It holds one relay socket across the `echo`, `idle` and `silent` connections it
  reads from stdin.

## 1. wasm-bindgen output in a TypeScript Durable Object

It works with `wasm-bindgen --target web`. `src/iroh-do/wasm.ts` imports
`pkg/iroh_do_wasm_bg.wasm`, which wrangler's default rules turn into a compiled
`WebAssembly.Module`, and calls `initSync({ module })` when the module loads.
`wrangler deploy --dry-run --outdir dist` bundles it with esbuild. No
post-processing or shim class is needed. `tsc --noEmit --strict` passes against
the generated `.d.ts`. That relies on wasm-bindgen's `unchecked_return_type` and
`unchecked_param_type` attributes and a `typescript_custom_section`.

Bundle sizes, measured with `wc -c` and `gzip -9c | wc -c` on `dist/`:

| File | Raw | gzip -9 |
|---|---|---|
| `worker.js` (package, app and glue; not minified) | 89,258 B | 17,123 B |
| `<hash>-iroh_do_wasm_bg.wasm` | 4,298,725 B | 1,433,131 B |

The worker-build bundle in `build/worker/` (chat and echo apps, built
2026-10-22) is 42,273 B of JavaScript and 3,796,117 B of wasm (1,460,914 B
gzip -9). The two are not directly comparable: they hold different apps, and
`wasm-opt` was not run on the spike.

Workers RPC to the class works. The router mints with `stub.mint(secret,
relayUrl)` and reads status with `stub.status()`, both under `wrangler dev` and
in Miniflare. The `/_internal/*` fetch paths that worker-build's proxy class
required are gone.

## 2. Hibernation with TypeScript owning the WebSockets

It works.

- The class accepts each relay socket with
  `ctx.acceptWebSocket(ws, ["relay", "sid:<id>"])`.
- It stores `{v: 1, sid, s: <session snapshot>}` as the socket's attachment at
  upgrade, after authentication, and whenever the destination-violation count
  changes.
- On the first event after a wake, it calls `WasmNode.restore` for each socket in
  `ctx.getWebSockets()`.

`node test/evict-test.mjs` uses Miniflare 5.20260908.0-alpha, the version
wrangler 4.130.0 bundles, and forces hibernation with
`unsafeEvictDurableObject(..., { webSockets: "hibernate" })`:

```
PASS evict with a pending handshake: 80 ms
PASS restored session denied the bad signature (ServerDeniesAuth, 1008): frames=0,3 close=1008
PASS echo before eviction: echo: one ms=51
PASS Warm with no node tasks after the endpoint ran: state=Warm tasks=0 endpoint_builds=1
PASS forced eviction in Warm succeeds promptly: 7 ms
PASS woken object rebuilt the node from the attachment (wakes +1, same socket id): wakes 2 -> 3, sockets ["5645e75a0b6fb6a8"] -> ["5645e75a0b6fb6a8"], state Warm
PASS echo after eviction over the hibernated socket: echo: two ms=9
PASS same socket, new endpoint build: endpoint_builds 1 -> 2
PASS Warm again, no tasks: state=Warm tasks=0
PASS forced eviction in Warm after idle and silent closes: 5 ms
PASS echo after the second eviction: echo: three ms=14
```

workerd refuses an eviction while timers or fetches are pending. An eviction
that returns at once therefore means nothing was left running in Warm.

## 3. Counting open streams without a guard API

It works. The wasm stream wrappers report to `iroh_do_core::Activity`
themselves:

- `acceptBi`, `openBi`, `acceptUni` and `openUni` take one `StreamGuard` per
  stream half.
- A send half's guard is released by `finish` or `reset`.
- A receive half's guard is released when it reaches the end of the stream or is
  stopped.
- Every read and write calls `touch`.

The app code does nothing for this. With `T_SILENT_MS=20000`, the same run gives:

```
PASS idle connection closed by the object with reason idle after ~10 s: after_ms=10014
PASS connection with an open stream closed with reason silent after ~20 s: after_ms=20021
```

## What moved from `crates/iroh-do/src/glue/` to TypeScript

- **WebSockets** (from `endpoint.rs`): accept, tags, attachments and restore;
  close handling; the 101 response and `Sec-WebSocket-Protocol`.
- **Storage** (from `endpoint.rs` and `store.rs`): the schedule table and the
  alarm, the handshake sweep, the identity row, and the `wakes` and
  `endpoint_builds` counters, all in `iroh_*` tables through `ctx.storage.sql`.
- **The name invariant** (`idFromName(name) == ctx.id`).
- **`links.rs`** is split. TypeScript maps socket id → WebSocket. Rust maps
  peer → socket id and calls `NodeHost.sendFrame(sid, frame)`.
- **`timers.rs`**: the `setTimeout` +1 ms round-up, as 11 lines of TypeScript.
- **`router.rs` and `route.rs`**: the host label, the probes, the admin token and
  mint.
- **`node.rs`**: `DoSql` is dropped, because TypeScript apps use
  `ctx.storage.sql`.

Not ported: pkarr publishing, the registry and endpoint list, and destroy.
pkarr would split the same way: Rust signs the packet, and TypeScript does the
`fetch` and the retries.

## What stays in Rust

- The ingress session and its snapshot format. Its wire format is
  conformance-tested against upstream.
- The node, the endpoint, the transport and frame encoding.
- Key generation and z-base-32 parsing (`PublicKey::from_z32`). Reimplementing
  them in TypeScript would risk the encoding confusion of note 004.
- The JSON log layer, so that iroh's own `tracing` events keep the format of
  note 010.

## Friction with Send and Sync

- **`ProtocolHandler::accept` must return a `Send` future** (iroh 1.4.0,
  `src/protocol.rs:228`), and awaiting a JS promise is not `Send`. `app.rs`
  wrapped the future in `AssertSend` with `unsafe impl Send`, sound only because
  the crate builds for wasm32 without threads. Step 1 removed it (below).
- **`Host` must be `Send + Sync`.** `JsHost` therefore carries only the
  EndpointId. The JS host object, the sessions and the peer map live in a
  `thread_local` keyed by EndpointId, because objects in one isolate share its
  wasm instance.
- **wasm-bindgen limits:**
  - Exported `async fn` methods cannot borrow `&self`. Async methods clone `Rc`
    handles and return `future_to_promise`.
  - One `tokio::sync::Mutex` serializes the operations on each stream half.
  - `TryFromJsValue` detaches the JS object it converts. `connect` therefore reads
    the id through `toBytes()` or `id()`.

## Open problems

As the spike left them. Step 1 settled 1 (connection lifetime), 2 (panics) and the
dial-back part of 6; 3, 4, 5 and the rest of 6 stand.

1. **The garbage collector decides when a connection is dropped.** Under iroh's
   `Router`, returning from `accept` drops the connection. Here, the JS
   `Connection` holds a clone until wasm-bindgen's `FinalizationRegistry` frees
   it, at an unspecified time. Stream halves that JavaScript abandons stay
   counted as open until then. The idle and silent limits bound both effects,
   but the design needs an explicit rule.
2. **Panics were not tested.** With `panic = "abort"`, a panic traps the
   isolate's single wasm instance, and with it every object in the isolate.
3. **The node is built on the first event, not in the constructor.** Subclass
   fields do not exist yet while the base constructor runs. A subclass that
   overrides `fetch` must call `super.fetch`.
4. **The upgrade takes three calls** (`upgrade` → `acceptWebSocket` →
   `opened(sid)`), because every outbound frame goes out through
   `sendFrame(sid)`.
5. **Frames are sent as views into wasm memory.** This depends on `ws.send`
   copying the bytes synchronously. worker-build's `send_with_bytes` does the
   same.
6. **Not exercised:** dial-back (`Endpoint.connect` is implemented but untested),
   production, CPU time per event, and size after `wasm-opt`.

## Step 1: layout

| Spike | Now |
|---|---|
| `spikes/ts-host/src/iroh-do/*.ts` | `packages/iroh-do/src/` (`object.ts`, `router.ts`, `wasm.ts`, `log.ts`, `index.ts`) |
| `spikes/ts-host/build-wasm.sh` | `packages/iroh-do/build-wasm.sh` |
| `spikes/ts-host/src/worker.ts`, `wrangler.toml` | `examples/echo/server/` (script `iroh-ts`, class `Echo`, binding `ENDPOINTS`) |
| `spikes/ts-host/test/evict-test.mjs` | `examples/echo/server/test/evict-test.mjs` |
| `spikes/ts-host/peer` (`iroh-do-ts-spike-peer`) | `crates/iroh-do-test-peer` |
| `spikes/ts-host/FINDINGS.md` | this file |
| — | `packages/iroh-do/test/` (fixture app, lifetime and panic tests, shared harness), `examples/echo/server/test/admin-test.mjs` |

npm workspaces at the repository root (`package.json`, one `package-lock.json`) hold
`packages/iroh-do` and `examples/echo/server`. `npm run build` in `examples/echo/server`
writes `dist/` and `dist/manifest.json` (`packages/iroh-do/README.md`, "Building a
bundle").

Sizes of that build, from `scripts/bundle.mjs` (`wc -c` equivalent) and `build-wasm.sh`
(`gzip -9`):

| File | Raw | gzip -9 |
|---|---|---|
| `worker.js` (not minified) | 117,409 B | — |
| `<hash>-iroh_do_wasm_bg.wasm` | 4,473,484 B | 1,478,967 B |

The wasm grew by 174,759 B over the spike's 4,298,725 B. Part of the difference is pkarr
(iroh-dns's packet encoding) and the panic hook; it was not broken down further.
`--experimental-reset-state-function` makes the glue 94,247 B instead of 73,319 B.

## Step 1: connection lifetime

Rule (`crates/iroh-do-wasm/src/app.rs`, `packages/iroh-do/README.md`):

- An accepted connection is closed with code 0 and reason `done` when the promise its
  handler's `accept` returned settles. The close goes through
  `iroh_do_core::Activity::close`, so the `conn_close` event carries `reason: 'done'`.
- A dialed connection is held by the app until it or the peer closes it, or the idle
  close fires.
- `iroh_do_core::Activity` stops counting a connection's stream guards when the
  connection closes, and the node keeps a node-wide count (`Stats::open_streams`,
  reported as `open_streams` in the status).

`node test/lifetime-test.mjs` in `packages/iroh-do`:

```
PASS settled handler: connection closed with code 0, reason done, before t_idle: closed: closed by peer: done (code 0) after_ms=5
PASS abandoned halves: connection closed with reason done: closed: closed by peer: done (code 0) after_ms=4
PASS abandoned halves still referenced by the app no longer count as open: kept=1 open_streams=0 connections=0
PASS Warm after t_linger, no tasks: state=Warm tasks=0
PASS dial-back: endpoint.connect reaches the peer over its relay socket: {"reply":"ok:back"}
PASS dialed connection stays open after the dialing call returned: connections=1 state=Active
PASS dialed connection closed by the idle close after ~10 s: back: closed: closed by peer: idle (code 0) after_ms=10020
PASS conn_open and conn_close events logged: conn_open=4 conn_close=4
```

The second case keeps both halves of an unfinished stream referenced from the object;
before this change they would have counted as open until the garbage collector freed them.
This run also exercised dial-back (`Endpoint.connect`) for the first time.

## Step 1: panic recovery

The glue is generated with `wasm-bindgen --experimental-reset-state-function`.
`__wbg_reset_state` instantiates the module again and bumps an instance id; objects and
closures of the old instance check it and throw instead of calling into the new instance,
and finalizers of old objects do nothing. A Rust panic hook (`setPanicHandler`) reports the
panic to the host before the trap; the host replaces the instance at the next event or in a
microtask after the trapping call has unwound, and each object rebuilds its node from
storage and socket attachments on its next event (`packages/iroh-do/README.md`, "Panics").
The test-only trigger is `testPanic(inTask)`, exported only with the crate's `test-panic`
feature; `scripts/bundle.mjs` refuses a build with any feature unless it is the test
bundle.

`node test/panic-test.mjs` in `packages/iroh-do`, two endpoints A and B in one isolate,
each with a stock peer:

```
PASS panic in A's call: A rebuilt its node on generation 1, same socket: rpc=200 generation=1 rebuilds=1 sockets ["01a9e3639f7e7b92"] -> ["01a9e3639f7e7b92"]
PASS B rebuilt on its next event: generation 1, same socket, wakes unchanged: generation=1 rebuilds=1 sockets ["3919d28dbd491d64"] -> ["3919d28dbd491d64"] wakes 1 -> 1
PASS echo on B and A after the panic: echo: b1 ms=5 | echo: a1 ms=4
PASS panic in a task: recorded by the panic hook: rpc=200 { level: 'error', target: 'iroh_do::host', message: 'panicked at crates/iroh-do-wasm/src/lib.rs:104:51:\ntest panic in a task', event: 'wasm_panic', generation: 1 }
PASS echo on B and A after the task panic: echo: b2 ms=26 | echo: a2 ms=16
PASS both rebuilt on generation 2, relay sockets kept: A gen=2 sockets=["17084863cc9160b9"]; B gen=2 sockets=["9b27d1dbf41fa281"]
PASS A Active with one connection before the panic: state=Active connections=1
PASS after the panic A is rebuilt with no connection: generation=3 connections=0 state=Warm
PASS the peer's open connection ends by its own idle timeout: closed: timed out after_ms=34994
PASS the peer redials over the same relay socket: echo: a3 ms=25 sockets=["17084863cc9160b9"]
info stale-instance errors logged: 0
```

(Lines from two runs; socket ids differ between them.) Relay sockets survive a panic; QUIC
connections do not. A peer with a connection open at the panic waited for its idle timeout,
35 s here (the endpoint advertises 30 s), before redialing.

## Step 1: pkarr and destroy

`crates/iroh-do/src/pkarr.rs` moved to `crates/iroh-do-core/src/pkarr.rs`
(`iroh_do::pkarr` re-exports it). The wasm library exports `pkarrRecord` and
`pkarrConfirm`; the object does the PUT, the GET and the retry schedule. Destroy is the RPC
`destroy()` behind `DELETE admin.<zone>/endpoints/<E>`. The endpoint list and the registry
object are not part of step 1.

`node test/admin-test.mjs` in `examples/echo/server`, against a fake pkarr relay in the
test process:

```
PASS mint publishes the pkarr record and confirms it by GET: {..."published":true}
PASS PUT then GET at <relay>/<z32>: PUT /pkarr/u1oy…p47o,GET /pkarr/u1oy…p47o
PASS republish scheduled a day later: {"failures":0,"status":"ok","ok_ms":1792702125546,"next_ms":1792788525546}
PASS PUT 503: not published, retry in 60-90 s: {"failures":1,"status":"put status 503","ok_ms":null,"next_ms":1792702195019} retry_in_ms=69462
PASS GET returns another record: not published: {"failures":1,"status":"get returned a different record",...}
PASS destroy: 200 {id, destroyed}: {"id":"u1oy…p47o","destroyed":true}
PASS destroy closed the relay socket: { ... event: 'destroy', endpoint: 'u1oy…p47o', sockets: 1, ms: 1 }
PASS status after destroy: 404
PASS second destroy: 404: 404
PASS relay upgrade after destroy: 404: 404
```

Publishing to the real `dns.iroh.link` was not exercised.

## Step 1: eviction marker and session events

The object sets `bound_at_ms` when the node binds an endpoint (`Host::endpoint_built`) and
clears it when the node closes it (`Host::endpoint_closed`, new in iroh-do-core). A
construction that finds it set logs `evicted_active`. workerd does not evict an object
with pending timers, so `examples/echo/server/test/evict-test.mjs` simulates the eviction
by restarting Miniflare on the same storage while a connection is open:

```
PASS no evicted_active after hibernations in Warm
PASS Active with a connection open: state=Active connections=1
PASS construction after an eviction while Active logs evicted_active: { level: 'warn', target: 'iroh_do::host', message: 'evicted_active', event: 'evicted_active', endpoint: 'trzq…96xy', since_bind_ms: 1111, wakes: 5 }
```

The rest of that test (hibernation mid-handshake and in Warm, idle 10,022 ms, silent
20,020 ms) passes as in the spike.

`conn_open`, `conn_close`, `endpoint_bind` and `endpoint_close` come from iroh-do-core
through the wasm log layer, as in the Rust deployment; the lifetime test checks the first
two, and the `wrangler dev` run below logged all four. Per-invocation CPU time in
production was not measured.

## Step 1: `unsafe`

`iroh_do_core::LocalProtocolHandler` is a handler whose `accept` future is `MaybeSend`:
`Send` natively, unbounded on wasm32, where the node spawns with `spawn_local`.
`Protocols::accept_local` registers one; `DynHandler`'s boxed futures drop the `Send` bound
on wasm32 only. `iroh-do-wasm` uses it and has no `unsafe` left. No iroh change was needed.

## Step 1: end to end under `wrangler dev`

```
$ npx wrangler dev --local --ip 127.0.0.1 --port 8799 --var ADMIN_TOKEN:local-e2e
$ curl -s --resolve admin.localhost:8799:127.0.0.1 -X POST -H "Authorization: Bearer local-e2e" http://admin.localhost:8799/mint
{"id":"bu9rgi9gxpfoqzsuazitgt714bjqcsrbduicruhkxj14ik5bcnjy","relay_url":"http://bu9r…cnjy.localhost:8799/","published":false}
$ cargo run -p iroh-do-echo-client -- bu9rgi9gxpfoqzsuazitgt714bjqcsrbduicruhkxj14ik5bcnjy hello --local 8799
hello
connected in 40 ms, echoed 5 bytes in 55 ms
```

## Step 1: production (2026-10-22)

`just apply` added the deployment `iroh-ts` (4 resources) and changed nothing in
`module.iroh_do`; the plan showed 0 to change, 0 to destroy. Afterwards the chat
endpoint on `iroh-do` still reported bundle `e1073cc3b6ef`, and
`iroh-do-chat ping` and `log` succeeded over default discovery.

On `iroh-ts`:

- Mint returned `published: true`.
- `iroh-do-echo-client <E> <text>` with no relay option, so the peer found `E`
  through pkarr, echoed 8 times. Connects took 248–321 ms, and 642–732 ms for
  the first connect of each run.
- Afterwards the status read `state: Cold`, `tasks: 0`, `open_streams: 0`, and
  pkarr `ok` with the next publish a day later.

CPU, from `wrangler tail iroh-ts --format json` during 5 echoes:

- The tail recorded all 5 relay upgrades at the router, but the object's events
  for only one session; the others are missing for a reason not found.
- That session: the router's upgrade took 17 ms of CPU, and the object's 13
  events (wake, authentication, endpoint bind, the echo connection, close) took
  33 ms in total.
- No event exceeded 17 ms, and none raised an exception.

Before the `setTimeout` round-up, the Rust host measured 1.5–3.6 s of CPU per
session (`design.md` § Time). One session is not a distribution; a CPU figure
for the TypeScript host should come from Workers observability over many
sessions.

`evicted_active` was not logged. It can appear only when production evicts an
object with a connection open, which needs observation over days.

## Step 2: what was built

- `examples/chat/server`: class `Chat`, which implements
  `examples/chat/common/PROTOCOL.md`. The first version was a transliteration of the Rust
  app and copied its Rust-specific behavior (Unicode `trim`, `usize` parsing, the
  hand-written JSON escaping). PROTOCOL.md now states those rules on their own terms (its
  section "Changes"), the Rust app follows them, and the class was rewritten against the
  specification:

  | File | Lines | Holds |
  |---|---|---|
  | `src/chat.ts` | 126 | `class Chat extends IrohDurableObject`: builds storage and the protocols once per construction, creates the schema in the constructor, maps ALPNs to handlers, runs the alarm work, adds the status fields |
  | `src/protocol.ts` | 330 | the wire format without I/O: ALPNs and limits, request framing and grammar, number and white-space rules, nick rules, the room and ctl lines as discriminated unions, encoding with `JSON.stringify` |
  | `src/room.ts` | 369 | `iroh-do/room/1`: request handling, the rate limit, deliveries and the connections kept for reuse |
  | `src/ctl.ts`, `src/lines.ts` | 143, 43 | `iroh-do/ctl/1` and its line framing |
  | `src/echo.ts`, `src/notify.ts` | 38, 89 | `iroh-do/echo/1`; dial-backs over `iroh-do/notify/1` |
  | `src/storage.ts` | 311 | the tables, typed row decoders that throw on values the schema rules out, and a transaction around each multi-statement write |
  | `src/events.ts`, `src/rate-limit.ts`, `src/log.ts` | 24, 29, 61 | records as protocol lines; the sliding window; JSON log lines with targets `chat`, `chat.room`, `chat.ctl`, `chat.echo`, `chat.notify` |

  The Rust app relies on its room lock to make a nick check and the writes that depend on
  it atomic. The class handles a request synchronously from the check to the writes, so no
  other event interleaves, and writes the member row and the event in one
  `transactionSync`. Promises that are not awaited go through one helper that logs a
  rejection with its stack. Unit tests (Vitest, `npm test` in `examples/chat/server`)
  cover the parts without I/O; Biome lints the package (`npm run lint`).
- `packages/iroh-do`:
  - `IrohDurableObject` gained `schedule`, `cancel`, `onAlarm`, `appStatus`,
    `endpointStatus` and `static app`.
  - `IrohRegistry` and the endpoint list (`registry.ts`) are new.
  - `irohRouter({ apps, registry })` is the multi-class form; `irohRouter({ binding })` is
    unchanged.
- `examples/ts-deployment`: the script `iroh-ts`, exporting `Echo` (binding `ENDPOINTS`),
  `Chat` (`CHAT`) and `Registry` (`REGISTRY`).

Sizes of `npm run build` in `examples/ts-deployment`, from `just bundle-info-ts`:

| File | Raw | gzip -9 |
|---|---|---|
| `worker.js` (not minified), first chat class | 164,995 B | 34,360 B |
| `worker.js` (not minified), rewritten chat class | 173,326 B | 36,233 B |
| `<hash>-iroh_do_wasm_bg.wasm`, first measurement | 4,473,484 B | 1,482,299 B |
| `<hash>-iroh_do_wasm_bg.wasm`, with the rewritten class | 4,481,587 B | 1,481,207 B |

The first `worker.js` grew by 47,586 B over step 1's 117,409 B; the chat app and the
registry are TypeScript. The rewrite added 8,331 B. `crates/iroh-do-wasm` and its path
dependencies did not change between the two wasm measurements; why the builds differ in
size was not investigated.

## Step 2: the stock client's lifecycle under `wrangler dev`

`iroh-do-chat lifecycle --room` against a chat endpoint of the deployment, with
`wrangler dev --local` on port 8799. The run includes every row of the plain `lifecycle`
plus the room rows. Two members exchange lines after the object hibernated, both by
dial-back:

```
 #  step                                                       result            t     measured  records  detail
--  ---------------------------------------------------------  ------  -----------  -----------  -------  ------
 1  s1 connect ctl/1 (first contact: possibly cold)            PASS          0.0 s      38.6 ms           relay http://1pay…ecky.localhost:8799/; 0-RTT not attempted (no ticket)
 2  s1 echo (logged line)                                      PASS          0.1 s      15.0 ms           echo: lifecycle yt1pohwn 1792706462426 s1 (#1, total 1, yours 1)
 3  s1 unprompted uni stream from DO                           PASS          0.1 s                        hello from 1pay…ecky: you are yt1p…f8oy
 4  s1 /status                                                 PASS          0.1 s       3.0 ms        1  state=Active wakes=1 endpoint_builds=1 connections=1 since_wake=4459ms bundle=local records=1
 5  DO closes idle connection (T_idle)                         PASS         10.1 s   10012.0 ms           application close code 0 reason "idle"
 6  wait for Warm (T_linger) + hibernation                     info         25.1 s   15000.0 ms           Endpoint dropped, isolate idle
 7  s2 connect ctl/1 (cold wake)                               PASS         25.1 s      21.6 ms           relay http://1pay…ecky.localhost:8799/
 8  s2 0-RTT after Warm                                        info         25.1 s                        rejected: expected on stock iroh 1.4.0 (…)
 9  s2 echo (logged line)                                      PASS         25.1 s       9.6 ms           echo: lifecycle yt1pohwn 1792706462426 s2 (#2, total 2, yours 2)
10  s2 /status                                                 info         25.1 s       3.0 ms        2  state=Active wakes=2 endpoint_builds=2 connections=1 since_wake=28ms bundle=local records=2
11  s2 wakes increased (isolate was evicted/hibernated)        PASS         25.1 s                        wakes 1 -> 2
12  s2 endpoint_builds increased (Active -> Warm -> Active)    PASS         25.1 s                        endpoint_builds 1 -> 2
13  s2 records increased across hibernation (wakes increased)  PASS         25.1 s                     2  records 1 -> 2, wakes 1 -> 2
14  s2 /log after hibernation contains the s1 line             PASS         25.1 s                        #1 ctl "lifecycle yt1pohwn 1792706462426 s1"
15  s2 DO-side time since wake                                 info         25.1 s      28.0 ms           reported by the DO in /status
16  s2 unprompted uni stream from DO                           PASS         25.1 s                        hello from 1pay…ecky: you are yt1p…f8oy
17  s2 /callback 20                                            PASS         25.1 s       3.2 ms           ok: callback in 20s
18  s2 closed by peer; idle until dial-back                    info         25.1 s
19  notify/1 dial-back from DO received                        PASS         45.1 s      36.5 ms           callback from 1pay…ecky wakes=3 records=2 (measured from the scheduled time)
20  notify/1 incoming handshake                                info         45.1 s      18.8 ms           object -> peer over the peer's own relay socket to the object
21  notify/1 records >= s2 records                             PASS         45.1 s                     2  records 2 -> 2 (the callback row is written after delivery)
22  DO hibernated before dial-back                             info         45.1 s                        yes: wakes 2 -> 3
23  s3 connect ctl/1                                           PASS         46.2 s      12.5 ms           0-RTT rejected (fell back to 1-RTT)
24  s3 /status                                                 info         46.2 s      11.5 ms        3  state=Active wakes=3 endpoint_builds=3 connections=1 since_wake=6053ms bundle=local records=3
25  s3 records increased (callback logged)                     PASS         46.2 s                     3  records 2 -> 3
26  s3 /log has the callback row                               PASS         46.2 s                        #3 "delivered"
27  room member a joins, then exits (stays a member)           PASS         46.2 s      42.0 ms           6sk7…qx7y
28  room member b joins and stays present                      PASS         47.3 s      60.6 ms           7oc6…pe3o (home relay: the room)
29  room wait for hibernation, b quiet                         info         78.3 s   30000.0 ms           wakes before: 3
30  room member a returns (same key)                           PASS         78.4 s      59.3 ms
31  room object hibernated while b was present                 PASS         78.4 s                        wakes 3 -> 4
32  room a says, b gets it delivered (dial-back)               PASS         78.5 s      61.6 ms
33  room b says, a gets it delivered                           PASS         78.5 s      56.8 ms
34  room members a and b leave (/quit)                         PASS         79.6 s

result: PASS
```

(Ids shortened here.) The server log of that run had no error lines. It logged
`room_delivery` three times, once `via: 'dial'` and twice `via: 'open'`, and `dial_back` once
with `ok: true`. On the same endpoint the client's `ping -c 2` echoed (0-RTT accepted on the
second connection, within one Active period). `log -n 4` and a `ctl` session over stdin
answered as the Rust app does. That session sent a line, `/count`, `/log 2`, `/callback 99`
and `/log x`, and the last two returned `error: usage /callback <secs>, secs <= 50` and
`error: usage /log [n], n <= 50`. `GET /endpoints.json`, `GET /` and `GET /status/<E>`
listed the endpoint with app `chat`, its label and the join hint `iroh-do-chat <E>`.

The same command against the rewritten class (`wrangler dev --local` on port 8821, after
the fixture run below on the same server) gave the same 34 rows and `result: PASS`, with
the session 1 connect in 12.7 ms, the session 2 connect after hibernation in 14.4 ms, and
the dial-back 6.5 ms after its scheduled time. Over both runs the server logged 933 events
at level `info` and none at `warn` or `error`.

## Step 2: router and registry in Miniflare

`node test/router-test.mjs` in `examples/ts-deployment`. The chat endpoint is reached with
`iroh-do-chat ping`, which uses `iroh-do/echo/1`, an ALPN that only `Chat` serves. The echo
endpoint is reached with the test peer on `iroh-example/echo/0`, which only `Echo` serves.

```
PASS mint with no body: the first app, chat: {"id":"rgiq…utky","app":"chat","relay_url":"http://rgiq…utky.localhost:8830/","published":false}
PASS mint {app: echo, label}: {"id":"memi…sdho","app":"echo",…}
PASS unknown app: 400 naming the apps: 400 unknown app; this deployment has ["chat", "echo"]
PASS label of 65 characters: 400
PASS label with a control character: 400
PASS body that is not JSON: 400
PASS mint without the token: 401
PASS endpoints.json: both, newest first, with app and label: …
PASS list page: join hints per app, label escaped
PASS POST /endpoints.json: 405
PASS status of the chat endpoint comes from class Chat: {"app":"chat","records":0,"members":0,"present":0}
PASS status of the echo endpoint comes from class Echo: app=echo
PASS status of a valid id that was never minted: 404
PASS relay upgrade for a valid id that was never minted: 404
PASS iroh-do-chat ping reaches class Chat (iroh-do/echo/1): ping 1/1: echo 32 B ok | connect 37.0 ms | …
PASS test peer ready on the echo endpoint
PASS test peer echoes off class Echo (iroh-example/echo/0): echo: routed ms=9
PASS first request for E in an isolate asks the registry, later ones use the cache: lookups after first status 1, after second status and a relay session 1
PASS another endpoint: one more lookup: lookups 2
PASS destroy the chat object directly over RPC: {"id":"rgiq…utky","destroyed":true}
PASS stale cache entry: status and relay reach the object, which answers 404: status 404, relay 404, lookups 2
PASS DELETE /endpoints/<echo>: 200 {id, destroyed}: …
PASS destroyed endpoint left the list: …
PASS status after destroy: 404
PASS second destroy: 404
PASS destroy of an entry whose object is gone: 404, entry removed: 404, 0 left
ALL PASS
```

The cache checks restart Miniflare on the same storage, which gives a new isolate with an
empty cache and a new registry object whose lookup count starts at 0.

## Step 2: two present members across a forced hibernation

`node test/room-test.mjs` in `examples/ts-deployment`, with three stock `iroh-do-chat`
clients without a UI. `ana` and `bo` run `--listen-for`; `cy` runs `--say` twice.

```
PASS ana joined (her join is in the /join history): {"t":"join","seq":1,"prev":0,…,"nick":"ana","hist":true}
PASS bo's join delivered live to ana: {"t":"join","seq":2,"prev":1,…,"nick":"bo"}
PASS Warm with both members present, no tasks: state=Warm tasks=0 members=2 present=2 sockets=2
PASS forced eviction in Warm succeeds promptly: 97 ms
PASS woken object: wakes +1, same two sockets, both present: wakes 1 -> 2, sockets 2, present 2
PASS new construction: no deliveries yet: {…"deliveries_ok":0,…"delivery_dials":0,…}
PASS cy's --say printed the stored message: {"t":"msg","seq":4,"prev":3,…,"nick":"cy","text":"after the eviction: \"quoted\" é"}
PASS ana and bo got the line delivered by dial-back: … | …
PASS each delivery dialed the member (no connection was open): {…"deliveries_ok":2,"deliveries_failed":0,"delivery_dials":2,"delivery_reuses":0,…}
PASS the next deliveries reuse the dialed connections: reuses 0 -> 2, dials 2 -> 2
PASS room_delivery events: via dial, then via open: open,dial,dial,open,open
PASS ana's events chain by prev (no gap): join#2<-1 join#3<-2 msg#4<-3 msg#5<-4
PASS status: 3 members, 2 present, 5 records: members=3 present=2 records=5
ALL PASS
```

The reuse check shows that a dialed room connection stays open after the delivery that
dialed it: the room keeps serving the connection, with its handler pending, until it
closes. Each connection was reused for the second line within `t_idle`.

The test is now `test/room.test.mjs`, run by Vitest with the same checks as `expect`
assertions (the counters are camelCase: `deliveriesOk`, `deliveryDials`, ...). Against the
rewritten class:

```
 ✓ … > two listeners: bo's join is delivered to ana 1401ms
 ✓ … > Warm with both members present, then a prompt forced eviction 12213ms
 ✓ … > deliveries dial the members back, then reuse the dialed connections 3207ms
 ✓ … > every member saw the same events, chained by prev 6ms

 Test Files  1 passed (1)
      Tests  4 passed (4)
```

## Step 2: differences from the Rust chat app

- `app` in the status is `chat`, from `static app`. The node's own status reports the wasm
  library's placeholder `ts`, which the class replaces. The status keys are sorted, as
  serde_json writes them.
- The counters (`ChatStats`) and the rate limiter are fields of the object, so they last
  for one construction, as the Rust app's do. A node rebuilt after a wasm panic keeps
  them; only the connections kept for reuse are dropped, since they belonged to the old
  node. The counters are not on the wire. Tests read them with the RPC `chatStats()`.
- Storage failures are not turned into defaults. The Rust app logs `chat_sql_failed` and
  goes on with an empty result or 0. The class answers a room request whose write was
  rolled back with `storage error` and logs `room_request_failed` with the cause; a ctl
  request whose storage fails gets `error: <reason>`, its stream ends, and the cause is
  logged as `ctl_request_failed`. A member row and the event that goes with it (`join`,
  `nick`, `part`) are written in one transaction, so a failure leaves neither. Once an
  event is stored the reply is `ok`: a failure to queue it for delivery is logged as
  `room_queue_failed`, since a member told `storage error` would send it again.
- A wasm trap (`WebAssembly.RuntimeError`) is never caught as a closed connection or
  stream: the handlers rethrow it, and in a task no event awaits it is logged as
  `task_failed`.
- JSON lines come from `JSON.stringify`, which escapes control characters as `\n`, `\t` and
  so on where the Rust app writes `\u000a`. PROTOCOL.md allows both.
- When the notify stream fails, the dial-back closes the connection at once instead of
  waiting up to 5 s for the peer to close it.
- At most 500 event lines queue for one member while its previous batch is being written;
  past that the oldest are dropped, and `room_delivery` reports them as `dropped`. The Rust
  app's queue has no bound.
- Log targets are `chat`, `chat.room`, `chat.ctl`, `chat.echo` and `chat.notify`, not the
  Rust module paths. Debug-level `tracing` lines of the Rust app, such as "delivery
  stream: …", have no counterpart; they are below the default `LOG_LEVEL`.
- The registry, and so routing, is per deployment. Endpoints minted in step 1's `iroh-ts`
  are not registered and are not reachable after step 2 (design note 013, Consequences).

## Step 2: the OpenTofu module

Not applied; the user applies. `tofu fmt -check` and `tofu validate` pass on a scratch
copy of `deploy/tofu` and `examples/deployment/infra`. The migration expressions,
evaluated with `tofu console` on the same locals:

| Live tag | Configuration | `migrations` sent |
|---|---|---|
| `v1` | `iroh_do` (default list) | null |
| none | `iroh_do` (default list) | `{new_tag v1, new_sqlite_classes [endpoint, registry]}` |
| `v1` | `iroh_ts` | `{old_tag v1, new_tag v2, new_sqlite_classes [Chat, Registry]}` |
| `v2` | `iroh_ts` | null |
| none | `iroh_ts` | `{new_tag v2, steps [[Echo], [Chat, Registry]]}` |
| `vX` | `iroh_ts` | plan fails (precondition) |

Expected first `just plan` after step 2:
- `module.iroh_do`: no change. Its bindings, content and bound classes are unchanged, the
  migration it sends is null as before, and its state's `migrations` is null (design note
  011, phase 5).
- `module.iroh_ts`: one in-place update of the script, with new content, the `CHAT` and
  `REGISTRY` bindings, a new `BUNDLE_SHA256`, and `migrations` from step 1's `v1` object
  to the `v2` step.
- After that apply, a second plan shows `iroh_ts`'s `migrations` going to null. One more
  apply clears it.

## Step 2: golden fixtures against the TypeScript chat app

The fixtures in `examples/chat/common/fixtures/` were recorded from the Rust chat
app. `FIXTURE_TARGET=8811 FIXTURE_ADMIN_TOKEN=<token> cargo test -p
iroh-do-chat-app --test fixtures fixtures` replayed all 12 of them (about 140
steps) against `examples/ts-deployment` under `wrangler dev` on port 8811, with
one stock iroh endpoint per member, and passed:

```
test result: ok. 1 passed; 0 failed; ... finished in 23.25s
```

The `wrangler dev` log for the run shows 12 mints, 130 `room_request`, 56
`room_delivery` and 172 `conn_open`/`conn_close` events, so the run reached the
TypeScript server, not the native harness. Without the two variables the same
test runs against the Rust app, where it also passes.

After the protocol rules were rewritten (PROTOCOL.md, "Changes"), two steps of
`02-history-and-since.json` were re-recorded from the Rust app and a 13th fixture,
`13-arguments.json`, was added. All 13 passed against the rewritten class under
`wrangler dev` on port 8821:

```
test fixtures ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 24.12s
```

That run logged 13 mints, 140 `room_request`, 57 `room_delivery` and 184 each of
`conn_open` and `conn_close`.

## Reproduce

```sh
export CARGO_TARGET_DIR=<repo>/target
npm ci                                   # repository root
cargo build -p iroh-do-test-peer

cd packages/iroh-do
npm run check                            # wrangler types + tsc --noEmit (strict)
npm test                                 # test bundle with test-panic, lifetime and panic tests

cd ../../examples/echo/server
npm run check
npm run build                            # dist/ and dist/manifest.json, no wasm features
npm test                                 # admin-test.mjs, evict-test.mjs (about 75 s)

# End to end with the stock client:
npx wrangler dev --local --ip 127.0.0.1 --port 8799 --var ADMIN_TOKEN:local
curl -s --resolve admin.localhost:8799:127.0.0.1 -X POST -H "Authorization: Bearer local" http://admin.localhost:8799/mint
cargo run -p iroh-do-echo-client -- <E> hello --local 8799

# Step 2: the deployment with both apps.
cargo build -p iroh-do-chat -p iroh-do-test-peer
cd ../../chat/server                     # examples/chat/server
npm run check && npm run lint && npm test   # tsc (strict), Biome, Vitest unit tests
cd ../../ts-deployment                   # examples/ts-deployment
npm run check
npm run build                            # or, from the root: just build-ts
npm test                                 # router-test.mjs, then room.test.mjs under Vitest (about 70 s)
npx wrangler dev --local --ip 127.0.0.1 --port 8799 --var ADMIN_TOKEN:local
curl -s --resolve admin.localhost:8799:127.0.0.1 -X POST -H "Authorization: Bearer local" \
  -H 'Content-Type: application/json' --data-binary '{"app":"chat"}' http://admin.localhost:8799/mint
cargo run -p iroh-do-chat -- --local --local-port 8799 lifecycle <E> --room
# The golden fixtures against that server (from the repository root):
FIXTURE_TARGET=8799 FIXTURE_ADMIN_TOKEN=local cargo test -p iroh-do-chat-app --test fixtures fixtures
```

`npm test` in `packages/iroh-do` leaves `pkg/` built with `test-panic`; `npm run build` in
the app rebuilds it without features before bundling.
