# Spike: iroh-do as a wasm library behind a TypeScript Durable Object

The stock `iroh-do-echo-client` echoes off `class Echo extends IrohDurableObject`
(`src/worker.ts`) under `wrangler dev`, and `test/evict-test.mjs` passes its 18
checks in Miniflare. No existing crate changed.

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
  wraps the future in `AssertSend` with `unsafe impl Send`. That is sound only
  because the crate builds for wasm32 without threads. It is the crate's one
  `unsafe`. An iroh-do-core entry point for handlers without the `Send` bound
  would remove it.
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

## Reproduce

```sh
export CARGO_TARGET_DIR=<repo>/target
cd spikes/ts-host
npm ci
./build-wasm.sh                      # wasm32 build (Homebrew LLVM) + wasm-bindgen --target web into pkg/
npx wrangler types && npx tsc --noEmit
npx wrangler deploy --dry-run --outdir dist

# End to end with the stock client:
npx wrangler dev --local --ip 127.0.0.1 --port 8799
curl -s --resolve admin.localhost:8799:127.0.0.1 -X POST http://admin.localhost:8799/mint
cargo run -p iroh-do-echo-client -- <E> hello --local 8799

# Forced hibernation and idle/silent closes (about a minute):
cargo build -p iroh-do-ts-spike-peer
node test/evict-test.mjs
```
