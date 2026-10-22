# iroh-do (TypeScript)

iroh endpoints as TypeScript Durable Objects, reachable by stock iroh peers (design note
013). The TypeScript class owns the Durable Object; `crates/iroh-do-wasm` is a wasm library
that keeps what has to be iroh: the relay ingress, the endpoint and its Cold/Warm/Active
lifecycle, keys, the pkarr record and the JSON log layer.

```ts
import { type Connection, IrohDurableObject, irohRouter } from "iroh-do";

export class Echo extends IrohDurableObject {
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
export default irohRouter({ binding: "ENDPOINTS" });
```

`examples/echo/server` is this app, with its `wrangler.toml`. `examples/chat/server` is the
chat app (class `Chat`), and `examples/ts-deployment` is the deployment `iroh-ts`, which
serves both from one script (see [Several apps](#several-apps)).

## The package

`iroh-do` is a package in the npm sense: `npm pack` in this directory produces a tarball
that installs and bundles outside this repository.

- `exports` points at compiled JavaScript and declarations in `dist/` (`tsc`), not at
  TypeScript sources.
- The wasm module and its wasm-bindgen glue ship prebuilt in `pkg/`. `dist/wasm.js` imports
  `../pkg/iroh_do_wasm_bg.wasm`, which wrangler's default module rules load as a compiled
  `WebAssembly.Module`.
- The declarations need the Workers runtime types (`wrangler types`, or
  `@cloudflare/workers-types`) in the consumer's program; `cloudflare:workers` is the only
  module they import.
- `wrangler` is an optional peer dependency, used only by the `iroh-do-bundle` command.
- The test helpers are a separate workspace package, `packages/iroh-do-testing`, with
  `miniflare` as a declared dependency.

The package is `private` in `package.json`: publishing it to npm is a decision for the
maintainers, not something the build does.

In this repository, `npm ci` at the root, then:

```sh
export CARGO_TARGET_DIR=<repo>/target     # optional
npm run build                              # build-wasm.sh, then tsc, for iroh-do and iroh-do-testing
```

`build-wasm.sh` runs `cargo build --locked -p iroh-do-wasm --target wasm32-unknown-unknown
--release` and `wasm-bindgen 0.2.129 --target web --experimental-reset-state-function` into
`pkg/`. On macOS it uses Homebrew LLVM's `clang` and `llvm-ar`, which ring needs
(`deploy/BUILD.md`). `build-wasm.sh --test-panic` writes a build with the `test-panic`
feature to `pkg-test/` instead, for the panic tests; `pkg/` is never a test build.

## Building a bundle

In an app's directory, `npm run build` builds the package, then runs `iroh-do-bundle`
(`scripts/bundle.mjs`):

1. `wrangler deploy --dry-run --outdir dist --metafile`. esbuild bundles the app, this
   package and the wasm-bindgen glue into one module; wrangler copies the wasm next to it,
   named by its content hash.
2. `dist/manifest.json`:

```json
{
  "main_module": "worker.js",
  "modules": [
    { "name": "worker.js", "path": "worker.js", "content_type": "application/javascript+module" },
    { "name": "6cec56f8590acde11a922e13d42e08ac58c266cb-iroh_do_wasm_bg.wasm",
      "path": "6cec56f8590acde11a922e13d42e08ac58c266cb-iroh_do_wasm_bg.wasm",
      "content_type": "application/wasm" }
  ],
  "bundle_sha256": "d93ee36fde3a8e07245df1fc7f2fce2dc8e86e639e48520ec11d197d01bf8ee3"
}
```

- `main_module` is named after the config's `main` (`src/worker.ts` → `worker.js`).
- `name` is the module's name as the main module imports it, which is the part name an
  upload must use; the command checks each one against the imports esbuild's metafile
  lists. `path` is relative to `dist/`.
- `modules` lists the main module first, then the others sorted by name.
- `bundle_sha256` is the SHA-256 of the concatenated lowercase hex SHA-256 digests of the
  module files, in `modules` order: the scheme `deploy/build-worker.sh` uses for the Rust
  bundle. Bind it as `BUNDLE_SHA256`; the status reports its first 12 characters.

`dist/` also holds `worker.js.map`, `bundle-meta.json` and wrangler's `README.md`, which are
not uploaded. The command refuses a wasm module built with features (`pkg/FEATURES`).

## Bindings

| Binding | Used by | |
|---|---|---|
| `ENDPOINTS` (or the class's `static namespace`) | router, object | The Durable Object namespace of the endpoint class. One object per endpoint, named by its EndpointId in z-base-32. A missing or wrong binding is logged as `config_error`. |
| `ZONE_SUFFIX` | router | Endpoints are `<E>.<ZONE_SUFFIX>`, the admin surface `admin.<ZONE_SUFFIX>`. |
| `ADMIN_TOKEN` (secret) | router | Bearer token for mint, destroy and the full status. Unset: mint and destroy answer 503. |
| `PKARR_RELAY` | router, at mint | pkarr relay base URL. Unbound: `https://dns.iroh.link/pkarr`. `""` disables publishing. |
| `LOG_LEVEL` | wasm | tracing directives (design note 010). |
| `BUNDLE_SHA256` | object | Reported in the status. |

The library reads these from `env` with a type check at run time, so an app's `Env` needs
no particular shape. `examples/ts-deployment` binds `Echo` as `ENDPOINTS`, `Chat` as `CHAT`
and `Registry` as `REGISTRY` in the script `iroh-ts`.

## Router

`irohRouter(opts)` returns `{ fetch, handle }`. `export default irohRouter(...)` makes it the
Worker's handler. `handle(req, env)` resolves to null for a host outside the zone, for a
Worker that serves other hosts too; `fetch` answers those with 404.

| Request | |
|---|---|
| `GET <E>.<zone>/relay`, WebSocket upgrade | Forwarded to the object named `E`. |
| `GET <E>.<zone>/ping`, `/generate_204` | Answered in the router, as iroh's relay server answers net_report's probes. |
| `POST admin.<zone>/mint` (token) | Generates a key in the isolate and calls `irohMint(secret, relayUrl, pkarrRelay)` on the object named by its public key. Returns `{id, relay_url, published}`. The key never leaves Cloudflare. |
| `DELETE admin.<zone>/endpoints/<E>` (token) | Calls `irohDestroy()` on `E`. Returns `{id, destroyed: true}`, or 404 if `E` was never minted or is already destroyed. |
| `GET admin.<zone>/status/<E>` | `E`'s status, or 404. Without the token: the node's and the app's fields (`EndpointStatus`). With it, also the host's (`HostStatus`: `sockets`, `tasks`, `open_streams`, `generation`, `rebuilds`, `isolate_nodes`, `pkarr`). |

Anything else is 404 without touching an object. Router-to-object calls are Workers RPC. A
missing binding, or a registry or object call that throws, is logged as `route_failed` and
answered 503.

The token is compared as SHA-256 digests with `crypto.subtle.timingSafeEqual`, so neither
its content nor its length leaks through timing. Failed checks are logged at most once a
minute per isolate (`admin_auth`, with the count). The router does no rate limiting; a
deployment that needs it puts Cloudflare's rate limiting rules in front of the admin host.

### Several apps

A deployment with several apps exports one class per app and the registry:

```ts
import { IrohRegistry, irohRouter } from "iroh-do";
export { Chat } from "iroh-do-chat-server";
export { Echo } from "iroh-do-echo-server";
export class Registry extends IrohRegistry {}

export default irohRouter({
  apps: {
    chat: { binding: "CHAT", joinHint: (id) => `iroh-do-chat ${id}` },
    echo: { binding: "ENDPOINTS", joinHint: (id) => `iroh-do-echo-client ${id}` },
  },
  defaultApp: "chat",
  registry: "REGISTRY",
});
```

`IrohRegistry` is the registry object of design note 009: one instance, named `registry`,
with the table `endpoints(id, app, label, minted_at)`. It checks the type and range of every
field it stores. The router then also serves:

| Request | |
|---|---|
| `POST admin.<zone>/mint` (token) | The body may name the app and a label, `{"app": "echo", "label": "…"}` (label at most 64 characters, no control characters). No app: `defaultApp`, else the first. An unknown app is a 400 naming the deployment's apps. Records `E → app` in the registry, then mints the object; if the mint fails, the object is destroyed and the entry removed. Returns `{id, app, relay_url, published}`. |
| `GET admin.<zone>/`, `/endpoints.json` | The endpoint list, newest first, 500 per page; `?before=<minted_at>` pages, and `/endpoints.json` gives the next page in a `Link: <…>; rel="next"` header. Public, as on the Rust host. |
| relay, status, destroy for `E` | The class is `E`'s app's, read from the registry over RPC and cached in the isolate (`E → app` never changes after mint). A valid id that is not registered is a 404, and is cached as such for 60 s. Destroy removes `E` from the registry after the object; if that fails, 502 and a retry removes it. |

Both caches are bounded at 10,000 entries per isolate and cleared when full.

## The object

`IrohDurableObject` owns `fetch`, the hibernatable WebSockets (`ctx.acceptWebSocket`, tags,
attachments), the alarm, and the tables below. Apps use `this.ctx`, `this.env` and
`ctx.storage.sql` directly. A subclass that overrides an event method (`fetch`,
`webSocketMessage`, `webSocketClose`, `webSocketError`, `alarm`) must call the base method.

The node is built on the first event after construction, not in the constructor, because
`protocols()` belongs to the subclass, whose fields do not exist while the base constructor
runs. Building it restores the ingress session of every socket from its attachment. The
storage writes of a build, the app's `protocols()` and the node's construction are one
transaction: if any throws, nothing is written, the build is logged as `node_build_failed`,
and the next event tries again (`fetch` answers 503 meanwhile).

| Table | Contents |
|---|---|
| `iroh_identity` | Secret key, EndpointId, relay URL, pkarr relay, mint time. Written once, at mint. |
| `iroh_state` | `wakes`, `endpoint_builds`, `node_rebuilds`, `bound_at_ms`, `pkarr_failures`, `pkarr_status`, `pkarr_ok_ms`, and `attempts:<key>` for alarm work that failed. |
| `iroh_schedule` | Alarm work: `sweep` (handshake deadlines), `republish` (pkarr), and the app's keys under `app:`. |

An object writes nothing until it is minted.

### What an app uses

| Member | Rust host equivalent | |
|---|---|---|
| `protocols()` | `App::protocols` | Handlers by ALPN. Called on every node build (each construction, and again after a wasm reset), only in a minted object, so an app also creates its tables here (`App::schema`). |
| `onAlarm(key, payload, attempt)` | `App::on_alarm` | Runs work scheduled with `this.schedule(key, delayMs, payload)` (`Ctx::schedule`); `this.cancel(key)` removes it. At least once: if it throws, or the instance traps, it runs again after 30 s, doubling up to an hour; `attempt` counts the runs of the current schedule. A run cut short by an eviction runs again when the runtime retries the alarm. |
| `onMinted()` | | Runs once, inside the mint transaction after the node build. A throw fails the mint. |
| `appStatus()` | `App::status` | Fields added to the status. `this.endpointStatus()` returns the whole status (`EndpointStatus`) as a Rust app's `Ctx::status` does, with keys sorted. |
| `endpoint` | `Ctx` | `id()`, `addr()`, `peers()`, `connect(peer, alpn)`. Exists once the node is built: not inside `protocols()`. |
| `endpointId` | `Ctx::id` | The id in z-base-32, available also inside `protocols()` and `onMinted()`. |
| `nodeSignal` | | An `AbortSignal` per node build, aborted with a `NodeLostError` when the node is lost: after a wasm reset and on destroy. |
| `withNode(body)` | | Runs `body(endpoint)` for the app's own RPC methods; after a trap it rebuilds the node and throws `NodeTrappedError`. |
| `static app` | `App::NAME` | Reported as `app` in the status. |
| `static namespace` | | The binding of the class's namespace, for the name check. Default `ENDPOINTS`. |
| `timings()` | `Config` | `t_idle`, `t_silent`, `t_linger`. A value that is not a finite non-negative number is logged as `config_error` and ignored. |

`schedule`, `cancel`, `endpointStatus`, `endpoint`, `endpointId` and `nodeSignal` throw
`NotMintedError` before mint.

Methods whose names start with `iroh` (`irohMint`, `irohDestroy`, `irohStatus`,
`irohTestPanic`) are the router's RPC surface. As every public method of a Durable Object,
they are reachable by any code of the deployment that holds the namespace binding.
`irohTestPanic` throws unless the module was built with `test-panic`, which production
bundles refuse.

There is no equivalent of `Ctx::spawn`: an app's promises keep the endpoint bound only while
they hold a connection, or until the linger (2 s) after the last one closes.

### After a wasm reset

A panic in any object traps the wasm instance that every object of the isolate shares (see
[Panics](#panics)). Each object's node is then rebuilt on its next entry point, including an
access to `this.endpoint` from the app's own RPC methods, and every `Endpoint`, `Connection`
and stream obtained from the old node is stale: calls on it throw. An app that caches
connections drops them when `nodeSignal` aborts, or in `protocols()`, which the rebuild
calls again:

```ts
protocols() {
  this.nodeSignal.addEventListener("abort", () => this.#connections.clear(), { once: true });
  return { ... };
}
```

### pkarr

At mint the object publishes its record: `pkarrRecord` (Rust, `iroh_do_core::pkarr`)
builds and signs it, the object `PUT`s it to `<PKARR_RELAY>/<z32>`, reads it back with a
`GET`, and `pkarrConfirm` (Rust) checks that what came back is the record it signed. A stale
PUT also answers 204, so only the GET confirms (design note 004). Each request has a 10 s
timeout, and response bodies that are not read are cancelled. The next attempt is scheduled
as the `republish` alarm: a day later after a success, otherwise after 60 s × consecutive
failures (at most a day) plus up to 30 s of jitter. A publish that was waiting when the
endpoint was destroyed writes nothing.

### Connection lifetime

- **Accepted connections.** When the promise returned by a handler's `accept` settles,
  resolved or rejected, the node closes the connection with code 0 and reason `done`. This
  is iroh's `Router` behaviour, where returning from `accept` drops the connection. The
  garbage collector cannot provide it: it frees the JS `Connection` at an unspecified time.
  A handler that keeps using its connection keeps its promise pending, for example until
  `conn.closed()`. As in iroh, a handler that writes a reply and returns at once may have the
  close overtake the reply; wait for the peer's close or `send.stopped()`.
- **Dialed connections** (`this.endpoint.connect(peer, alpn)`) have no handler scope. They
  stay open until the app calls `close`, the peer closes them, or the idle close does (after
  `t_idle`, or `t_silent` while a stream is open).
- **Streams.** Each stream half counts as open until it reaches a terminal outcome: for a
  send half `finish`, `reset`, a failed write, or `stopped()` resolving; for a receive half
  the end of the stream, a failed read (the peer reset it, the connection closed), or
  `stop`. While a half is open the idle close waits `t_silent` instead of `t_idle`. When a
  connection closes its halves stop counting, even if JavaScript still references them.
- **Concurrent calls on one half.** Reads, writes, `finish` and `receivedReset` on one half
  run in the order they were called. `stop` and `reset` act at once: a read or write pending
  on the same half fails with an error named `ClosedStream`.

### Panics

The crate builds with `panic = "abort"`, so a panic traps the wasm instance. Recovery:

1. A panic hook in Rust calls the host before the trap (`wasm_panic`). A trap that is not a
   panic is caught where the object called into wasm (`wasm_trap`).
2. At the start of the next entry point, or in a microtask once the trapping call has
   unwound, the host instantiates the module again with wasm-bindgen's `__wbg_reset_state`
   (`wasm_reset`). Objects and closures of the old instance are stale from then on.
3. Each object finds, on its next entry point, that its node belongs to an older instance,
   aborts `nodeSignal`, and rebuilds the node from storage and socket attachments, as a wake
   does (`node_rebuild`). The event that trapped is dropped; alarm work it was running is
   retried.

Relay sockets survive: the WebSockets belong to the object, not to the instance. QUIC
connections do not: a peer with a connection open learns of it when its idle timeout fires,
then redials over the same relay socket.

### Node state and garbage collection

The wasm side keeps each node's sockets and peers in a per-instance map keyed by the node,
not by its EndpointId, so a node left over from an earlier construction cannot reach a newer
node's sockets. The map holds the object's callbacks weakly, and a `WasmNode` that the
garbage collector frees removes its entry: an object the runtime drops does not stay alive
through wasm memory. `isolate_nodes` in the full status counts the entries.

### `setTimeout`

Importing the package makes every `setTimeout` in the isolate fire 1 ms later than asked:
`setTimeout(f, ms, ...args)` runs after `max(0, floor(ms)) + 1` ms. The handle, the callback
and its arguments are unchanged; `clearTimeout`, `setInterval` and `scheduler.wait` are not
touched.

n0-future's wasm `sleep` (0.3.2, `src/time.rs`) passes `duration.as_millis()` to
`setTimeout`, which truncates. Under Cloudflare's clock, which advances only between events,
a noq timer then fires before its deadline, finds it not reached, and re-arms with a delay
that truncates to 0: a loop that never advances the clock (`docs/design.md`, "Time"). One
more millisecond guarantees the deadline has passed. n0-future looks `setTimeout` up on
`globalThis` at every call, so no stock dependency lets the package give it a private timer,
and the patch is global; the Rust host patches the same property. The fix belongs upstream:
n0-future rounding the delay up (`as_micros().div_ceil(1000)`) instead of truncating.

## Logs

The object logs in the format of design note 010 (`target: "iroh_do::host"`). The node's
events (`state`, `endpoint_bind`, `endpoint_close`, `conn_open`, `conn_close`,
`ingress_auth`, ...) come from the wasm side unchanged.

| `event` | Level | Fields | When |
|---|---|---|---|
| `wake` | info | `endpoint`, `wakes`, `cause`, `sockets`, `sockets_closed`, `state` | Node built: `cause` `construct` or `wasm_reset`. |
| `evicted_active` | warn | `endpoint`, `since_bind_ms`, `wakes` | A construction found the bind marker: the previous instance was evicted with its endpoint bound. |
| `node_rebuild` | warn | `endpoint`, `generation`, `rebuilds`, `was_active` | The node belonged to a trapped instance. |
| `node_build_failed` | error | `endpoint`, `error` | `protocols()` or the node's construction threw; nothing was stored. |
| `wasm_panic`, `wasm_trap`, `wasm_reset` | error, error, warn | `generation`, `message` / `error` / `reason` | See [Panics](#panics). |
| `mint` | info | `endpoint`, `published`, `ms` | Identity stored. |
| `destroy` | info | `endpoint`, `sockets`, `ms` | Endpoint destroyed. |
| `pkarr_publish` | info, warn | `endpoint`, `ok`, `ms`, `status`, `failures`, `next_in_s` | Publish and confirm. |
| `alarm` | info | `endpoint`, `due`, `ran`, `failed`, `ms` | One alarm run. `due` lists kinds, never a peer. |
| `alarm_failed` | error, warn | `endpoint`, `due`, `error`, `trap`, `retry_in_s` | Alarm work threw or trapped; it runs again later. |
| `socket_close` | info, warn | `endpoint`, `by`, `code`, `reason`, `clean`, `error` | A relay socket closed by the peer or with an error. |
| `config_error` | error | `missing` or `invalid`, `value` | A binding is missing, or a timing is invalid. |
| `storage_failed` | error | `op`, `error` | `setAlarm` or `deleteAlarm` failed. |
| `admin_auth` | info, warn | `ok`, `path`, `failures` | A token was checked; failures at most once a minute. |
| `route_failed` | error | `path`, `error`, `remote`, `retryable`, `overloaded` | A binding or RPC failed; 503. |
| `registry_register`, `registry_entry`, `registry_remove` | info | `endpoint`, `app` | Registry changes. |
| `mint_failed` | error | `endpoint`, `app`, `error` | The object refused the mint; it is destroyed and the entry removed. |
| `route_unknown_app` | warn | `endpoint`, `app` | The registry names an app this deployment does not have; 404. |

## Differences from `@number0/iroh`

The Connection and stream API follows `@number0/iroh` 1.1.0, iroh's Node bindings, so that
stream code carries over. The types are written in `src/types.ts`, not taken from the
wasm-bindgen output, whose classes also carry `free()` and `[Symbol.dispose]`.

| `@number0/iroh` | iroh-do | Why |
|---|---|---|
| `Array<number>` for bytes and ALPNs | `Uint8Array` | Workers APIs use `Uint8Array`; an array of numbers costs a copy and boxing per call |
| `Endpoint.acceptNext()` loop | `protocols()` returning `{ [alpn]: { accept } }` | An object cannot wait in a loop across hibernation. This is the shape of iroh's own `ProtocolHandler` |
| `Endpoint.bind`, builder, presets, `close()` | none; `this.endpoint` has `id()`, `addr()`, `peers()`, `connect()` | The host binds on wake, from stored identity, and closes after the linger |
| `connect(addr: EndpointAddr, alpn: Array<number>)` | `connect(peer: EndpointId \| EndpointAddr, alpn: string \| Uint8Array)` | Only the id matters: the object reaches a peer only over the peer's relay socket to it |
| relays, discovery, paths, datagrams, `stats()`, `rtt()`, network watches | not exposed | One relay path, no UDP |
| `EndpointId.toString()` only | adds `toZ32()`, `fromZ32()` | URLs and DNS labels use z-base-32 (004) |
| `BiStream` class with `send`/`recv` getters | a frozen object with `send` and `recv` | `bi.send === bi.send`; no wrapper per access |
| errors are `Error` with a Debug message | `Error` whose `name` is the Rust error type (`ReadError`, `WriteError`, `ConnectionError`, `ConnectError`, `ClosedStream`, ...) | Callers can tell failures apart |
| `stop`/`reset` wait behind a pending read or write | they act at once; the pending call fails | A stuck read must not block abandoning the stream |
| — | no stream guard API | The stream wrappers count themselves (above) |
| — | returning from `accept` closes the connection | iroh's `Router` semantics, without waiting for the garbage collector |

`closeReason()`, `relayUrl()`, `stopped()` and `receivedReset()` return null as
`@number0/iroh` does. `read()` resolves with an empty array at the end of the stream, as
`@number0/iroh` does, and only then: `read(0)` is refused.

## Tests

```sh
npm ci && npm run build                                        # repository root
cargo build -p iroh-do-test-peer -p iroh-do-chat               # the stock peers
npm run check && npm run lint                                  # tsc in every package, Biome
npm run test:unit -w iroh-do                                   # workerd and Node unit tests
npm run test:e2e -w iroh-do                                    # lifetime, streams, panics
npm test -w iroh-do-echo-server                                # unit; admin, eviction
npm test -w iroh-do-ts-deployment                              # router and registry; chat room
```

- `test/unit` runs in workerd (`@cloudflare/vitest-plugin`): the alarm under failures and
  traps, staleness after another object's trap, failed builds and mints, the router's caches,
  503s and status subset, the registry.
- `test/node` runs the wasm library in Node with `--expose-gc`: node state per node, and its
  release when the object is collected.
- `test/e2e` runs the test app (`test/fixture`, built with `test-panic` from `pkg-test/`) in
  Miniflare against the stock test peer: connection lifetime, dial-back, streams released
  by a peer's reset or stop, panic recovery.
- `examples/echo/server/test`: mint with pkarr against a fake relay, destroy, forced
  hibernation, idle and silent closes, `evicted_active`.
- `examples/ts-deployment/test`: mint by app, the list, class resolution, destroy and stale
  entries with stock peers; the chat room across a forced hibernation.

The peers are `crates/iroh-do-test-peer` and `examples/chat/client` (`iroh-do-chat`): stock
iroh with default features, no iroh-do crate. The test peer uses `presets::Minimal` with the
object as its only relay, so no test reaches an n0 service. Every Miniflare listens on a
port the OS picks; every wait polls a condition with a deadline.
