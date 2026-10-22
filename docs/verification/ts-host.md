# The TypeScript host: spike and step 1 (design note 013)

Two rounds of verification of design note 013, both on 2026-10-22, with wrangler
4.130.0, Miniflare 5.20260908.0-alpha (the version wrangler bundles), wasm-bindgen
0.2.129 and iroh 1.4.0.

- **Spike** (sections "What was built" to "Open problems"): the stock
  `iroh-do-echo-client` echoed off `class Echo extends IrohDurableObject` under
  `wrangler dev`, and the eviction test passed its 18 checks in Miniflare. The spike lived
  in `spikes/ts-host/`; its files moved as listed under "Step 1: layout".
- **Step 1** (last sections): the package, the echo app and the tests as they are now,
  with connection lifetime, panic recovery, pkarr at mint, destroy over RPC and the
  eviction marker.

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
```

`npm test` in `packages/iroh-do` leaves `pkg/` built with `test-panic`; `npm run build` in
the app rebuilds it without features before bundling.
