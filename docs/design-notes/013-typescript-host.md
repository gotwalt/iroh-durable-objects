# 013: TypeScript owns the Durable Object; Rust is a wasm library

**Status:** proposed (2026-10-22). Replaces the app API of 011 and the glue
layout of 011 and 012. Rewords P5 in `spec.md` §1, which needs review.
Evidence: `spikes/ts-host/FINDINGS.md`.

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
not when they are collected. *To be implemented: the spike relies on the
idle/silent limits instead.*

### Panics

With `panic = "abort"`, a panic traps the isolate's one wasm instance, which
every object in the isolate shares. The base class catches the
`WebAssembly.RuntimeError` and reinstantiates the module. It then marks every
object's node lost, and each object rebuilds its node on its next event from
storage and socket attachments, the same path as a wake. *Not yet tested.*

## P5

P5 currently reads: "Protocol logic (relay handshake, framing, lifecycle) is a
pure state machine with explicit `now` and explicit I/O, testable natively
with no Cloudflare dependency. Runtime glue is thin."

Under this design the glue is TypeScript and is the product surface, not thin.
The principle's substance holds: protocol logic stays in Rust, sans-io and
tested natively. Proposed wording:

> **P5. Sans-io first.** Protocol logic (relay handshake, framing, lifecycle)
> is a pure state machine with explicit `now` and explicit I/O, testable
> natively with no Cloudflare dependency. The platform host (TypeScript) holds
> no protocol logic: it moves bytes, persists what the Rust side asks for, and
> exposes the platform to apps.

## Consequences

- **Removed:** `crates/iroh-do` (`glue/`, the `worker!` macro, router), the
  `App` trait and `AppEntry` of 011, `examples/deployment` as a Rust crate,
  worker-build and `deploy/build-worker.sh`.
- **An app is chosen per deployment, not per endpoint** (011, decision 2): each
  app is a DO class, and its Worker binds that class. One deployment can still
  export several classes. The router then needs `E → class`, which mint
  records in the registry.
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
    `spikes/ts-host/test/evict-test.mjs` does.
  - `tests/workerd/` is rewritten for the new bundle.
- **Examples:**
  - `examples/echo` becomes a TypeScript class.
  - `examples/chat/server` becomes a TypeScript class.
  - `examples/chat/common` holds the room protocol as a written spec plus golden
    request and reply fixtures. The TypeScript server's tests and the Rust
    client's tests both check the fixtures.
  - The clients do not change (P3).
- **Not yet ported:** pkarr publishing, the registry and endpoint list, and
  destroy. pkarr splits the same way as the rest: Rust builds and signs the
  packet, and TypeScript does the `fetch` and the retries.
- **`unsafe`:** `iroh-do-wasm` asserts `Send` on handler futures, because
  iroh's `ProtocolHandler::accept` requires it and a JS promise is not `Send`.
  This is sound only on wasm32 without threads.
