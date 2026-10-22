# iroh-do (TypeScript)

iroh endpoints as TypeScript Durable Objects, reachable by stock iroh peers. This is
design note 013, steps 1 and 2: the TypeScript class owns the Durable Object, and
`crates/iroh-do-wasm` is a wasm library that keeps what has to be iroh (the relay
ingress, the endpoint and its Cold/Warm/Active lifecycle, keys, the pkarr record, the
JSON log layer).

```ts
import { IrohDurableObject, irohRouter, type Connection } from "iroh-do";

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

`examples/echo/server` is this app, with its `wrangler.toml`. `examples/chat/server` is
the chat app (class `Chat`), and `examples/ts-deployment` is the deployment `iroh-ts`, which
serves both from one script (see [Several apps](#several-apps)).

## Building a bundle

From the repository root, once: `npm ci`. Then, in the app's directory:

```sh
export CARGO_TARGET_DIR=<repo>/target    # optional
cd examples/echo/server
npm run build
```

`npm run build` runs `iroh-do-bundle` (`scripts/bundle.mjs`):

1. `build-wasm.sh`: `cargo build -p iroh-do-wasm --target wasm32-unknown-unknown
   --release` and `wasm-bindgen 0.2.129 --target web --experimental-reset-state-function`
   into `packages/iroh-do/pkg/`. On macOS it uses Homebrew LLVM's `clang` and `llvm-ar`,
   which ring needs (`deploy/BUILD.md`).
2. `wrangler deploy --dry-run --outdir dist`. esbuild bundles the app, this package and the
   wasm-bindgen glue into one module; wrangler copies the wasm next to it, named by its
   content hash.
3. `dist/manifest.json`:

```json
{
  "main_module": "worker.js",
  "modules": [
    { "name": "worker.js", "path": "worker.js", "content_type": "application/javascript+module" },
    { "name": "6e1bcb3b63437e712d53aaab969c933f2a2158ff-iroh_do_wasm_bg.wasm",
      "path": "6e1bcb3b63437e712d53aaab969c933f2a2158ff-iroh_do_wasm_bg.wasm",
      "content_type": "application/wasm" }
  ],
  "bundle_sha256": "a610ae48c8a9d12a9e3fa0c3b3af446a4436b4332088418c0461add85f1cf0d2"
}
```

- `main_module` is named after the config's `main` (`src/worker.ts` → `worker.js`).
- `name` is the module's name as the main module imports it, which is the part name an
  upload must use. `path` is relative to `dist/`.
- `modules` lists the main module first, then the others sorted by name.
- `bundle_sha256` is the SHA-256 of the concatenated lowercase hex SHA-256 digests of the
  module files, in `modules` order: the scheme `deploy/build-worker.sh` uses for the Rust
  bundle. Bind it as `BUNDLE_SHA256`; the status reports its first 12 characters.

`dist/` also holds `worker.js.map` and wrangler's `README.md`, which are not uploaded.

The build refuses a wasm module built with features (`pkg/FEATURES`). Only the test
bundle (`npm run build:test` here, into `test/dist`) is built with `test-panic`.

## Bindings

| Binding | Used by | |
|---|---|---|
| `ENDPOINTS` (or the class's `static namespace`) | router, object | The Durable Object namespace of the endpoint class. One object per endpoint, named by its EndpointId in z-base-32. |
| `ZONE_SUFFIX` | router | Endpoints are `<E>.<ZONE_SUFFIX>`, the admin surface `admin.<ZONE_SUFFIX>`. |
| `ADMIN_TOKEN` (secret) | router | Bearer token for mint and destroy. Unset: both answer 503. |
| `PKARR_RELAY` | router, at mint | pkarr relay base URL. Unbound: `https://dns.iroh.link/pkarr`. `""` disables publishing. |
| `LOG_LEVEL` | wasm | tracing directives (design note 010). |
| `BUNDLE_SHA256` | object | Reported in the status. |

`examples/ts-deployment` binds `Echo` as `ENDPOINTS`, `Chat` as `CHAT` and `Registry` as
`REGISTRY` in the script `iroh-ts`, so the namespaces are `iroh-ts_Echo`, `iroh-ts_Chat` and
`iroh-ts_Registry`.

## Router

`irohRouter({ binding })` is the Worker's `fetch`:

| Request | |
|---|---|
| `GET <E>.<zone>/relay`, WebSocket upgrade | Forwarded to the object named `E`. |
| `GET <E>.<zone>/ping`, `/generate_204` | Answered in the router, as iroh's relay server answers net_report's probes. |
| `POST admin.<zone>/mint` (token) | Generates a key in the isolate, calls `mint(secret, relayUrl, pkarrRelay)` on the object named by its public key. Returns `{id, relay_url, published}`. The key never leaves Cloudflare. |
| `DELETE admin.<zone>/endpoints/<E>` (token) | Calls `destroy()` on `E`. Returns `{id, destroyed: true}`, or 404 if `E` was never minted or is already destroyed. |
| `GET admin.<zone>/status/<E>` | Calls `status()` on `E`, or 404. |

Anything else is 404 without touching an object. Router-to-object calls are Workers RPC.

### Several apps

A deployment with several apps exports one class per app and the registry:

```ts
import { IrohRegistry, irohRouter } from "iroh-do";
export { Chat } from "iroh-do-chat-server";
export { Echo } from "iroh-do-echo-server";
export class Registry extends IrohRegistry {}

export default {
  fetch: irohRouter({
    apps: {
      chat: { binding: "CHAT", joinHint: (id) => `iroh-do-chat ${id}` },
      echo: { binding: "ENDPOINTS", joinHint: (id) => `iroh-do-echo-client ${id}` },
    },
    registry: "REGISTRY",
  }),
};
```

`IrohRegistry` is the registry object of design note 009: one instance, named `registry`,
with the table `endpoints(id, app, label, minted_at)`. The router then also serves:

| Request | |
|---|---|
| `POST admin.<zone>/mint` (token) | The body may name the app and a label, `{"app": "echo", "label": "…"}` (label at most 64 characters, no control characters). No app: the first. An unknown app is a 400 naming the deployment's apps. Records `E → app` in the registry, then mints the object; if the mint fails, the entry is removed. Returns `{id, app, relay_url, published}`. |
| `GET admin.<zone>/`, `/endpoints.json` | The endpoint list, newest first, at most 500, with each app's join hint. Public, as on the Rust host. |
| relay, status, destroy for `E` | The class is `E`'s app's, read from the registry over RPC and cached in the isolate. `E → app` never changes after mint. A valid id that is not registered is a 404. Destroy removes `E` from the registry after the object; if that fails, 502 and a retry removes it. |

A request for a valid id that the isolate has not cached wakes the registry.

## The object

`IrohDurableObject` owns `fetch`, the hibernatable WebSockets (`ctx.acceptWebSocket`, tags,
attachments), the alarm, and the tables below. Apps use `this.ctx`, `this.env` and
`ctx.storage.sql` directly. A subclass that overrides an event method must call the base
method.

The node is built on the first event after construction, not in the constructor, because
`protocols()` belongs to the subclass, whose fields do not exist while the base constructor
runs. Building it restores the ingress session of every socket from its attachment.

| Table | Contents |
|---|---|
| `iroh_identity` | Secret key, EndpointId, relay URL, pkarr relay, mint time. Written once, at mint. |
| `iroh_state` | `wakes`, `endpoint_builds`, `node_rebuilds`, `bound_at_ms`, `pkarr_failures`, `pkarr_status`, `pkarr_ok_ms`. |
| `iroh_schedule` | Alarm work: `sweep` (handshake deadlines), `republish` (pkarr), and the app's keys under `app:`. |

An object writes nothing until it is minted.

### What an app overrides

| Member | Rust host equivalent | |
|---|---|---|
| `protocols()` | `App::protocols` | Handlers by ALPN. Called once per node build, only in a minted object, so an app also creates its tables here (`App::schema`). |
| `onAlarm(key, payload)` | `App::on_alarm` | Runs work scheduled with `this.schedule(key, delayMs, payload)` (`Ctx::schedule`), from the alarm; `this.cancel(key)` removes it. If `onAlarm` throws, the work runs again after 30 s. |
| `appStatus()` | `App::status` | Fields added to the status. `this.endpointStatus()` returns the whole status as a Rust app's `Ctx::status` does, with keys sorted. |
| `static app` | `App::NAME` | Reported as `app` in the status. |
| `static namespace` | | The binding of the class's namespace, for the name check. Default `ENDPOINTS`. |
| `timings()` | `Config` | `t_idle`, `t_silent`, `t_linger`. |

`this.endpoint.peers()` lists the peers with a relay socket (`Ctx::peers`, `has_socket`), and
`this.endpoint.connect(peer, alpn)` dials one of them back (`Ctx::connect`). There is no
equivalent of `Ctx::spawn`: an app's promises keep the endpoint bound only while they hold a
connection, or until the linger (2 s) after the last one closes.

### pkarr

At mint the object publishes its record: `pkarrRecord` (Rust, `iroh_do_core::pkarr`)
builds and signs it, the object `PUT`s it to `<PKARR_RELAY>/<z32>`, reads it back with a
`GET`, and `pkarrConfirm` (Rust) checks that what came back is the record it signed. A stale
PUT also answers 204, so only the GET confirms (design note 004). The next attempt is
scheduled as the `republish` alarm: a day later after a success, otherwise after
60 s × consecutive failures (at most a day) plus up to 30 s of jitter.

### Connection lifetime

- **Accepted connections.** When the promise returned by a handler's `accept` settles,
  resolved or rejected, the node closes the connection with code 0 and reason `done`. This
  is iroh's `Router` behaviour, where returning from `accept` drops the connection. The
  garbage collector cannot provide it: it frees the JS `Connection` at an unspecified time.
  A handler that keeps using its connection, for example to send on it later, keeps its
  promise pending, for example until `conn.closed()`. As in iroh, a handler that writes a
  reply and returns at once may have the close overtake the reply; wait for the peer's close
  or `send.stopped()`.
- **Dialed connections** (`this.endpoint.connect(peer, alpn)`) have no handler scope. They
  stay open until the app calls `close`, the peer closes them, or the idle close does (after
  `t_idle`, or `t_silent` while a stream is open).
- **Streams.** Each stream half counts as open until the send half is finished or reset and
  the receive half has reached its end or was stopped. While a half is open the idle close
  waits `t_silent` instead of `t_idle`. When a connection closes its halves stop counting,
  even if JavaScript still references them.

### Panics

The crate builds with `panic = "abort"`, so a panic traps the wasm instance, which every
object in the isolate shares. Recovery:

1. A panic hook in Rust calls the host before the trap (`wasm_panic`). A trap that is not a
   panic is caught where the object called into wasm (`wasm_trap`).
2. At the start of the next event, or in a microtask once the trapping call has unwound,
   the host instantiates the module again with wasm-bindgen's `__wbg_reset_state`
   (`wasm_reset`). Objects and closures of the old instance are stale from then on: calling
   them throws instead of reaching the new instance.
3. Each object finds, on its next event, that its node belongs to an older instance and
   rebuilds it from storage and socket attachments, as a wake does (`node_rebuild`). The
   event that trapped is dropped.

Relay sockets survive: the WebSockets belong to the object, not to the instance. QUIC
connections do not: a peer with a connection open learns of it when its idle timeout fires,
then redials over the same relay socket.

## Logs

The object logs in the format of design note 010 (`target: "iroh_do::host"`). The node's
events (`state`, `endpoint_bind`, `endpoint_close`, `conn_open`, `conn_close`,
`ingress_auth`, ...) come from the wasm side unchanged. Each line belongs to the invocation
that wrote it, so Cloudflare's per-invocation CPU time can be joined to sessions by
`endpoint` and `peer`.

| `event` | Level | Fields | When |
|---|---|---|---|
| `wake` | info | `endpoint`, `wakes`, `cause`, `sockets`, `sockets_closed`, `state` | Node built: `cause` `construct` (first event after construction) or `wasm_reset`. |
| `evicted_active` | warn | `endpoint`, `since_bind_ms`, `wakes` | A construction found the bind marker: the previous instance was evicted, not hibernated, with its endpoint bound, `since_bind_ms` after the bind. |
| `node_rebuild` | warn | `endpoint`, `generation`, `rebuilds`, `was_active` | The node belonged to a trapped instance. `was_active`: its endpoint was bound. |
| `wasm_panic` | error | `generation`, `message` | The Rust panic hook ran. |
| `wasm_trap` | error | `generation`, `error` | A trap that the panic hook did not report. |
| `wasm_reset` | warn | `generation`, `reason` | The module was instantiated again. |
| `mint` | info | `endpoint`, `published`, `ms` | Identity stored. |
| `destroy` | info | `endpoint`, `sockets`, `ms` | Endpoint destroyed. |
| `pkarr_publish` | info, warn | `endpoint`, `ok`, `ms`, `status`, `failures`, `next_in_s` | Publish and confirm. |
| `alarm` | info | `endpoint`, `due`, `ran`, `ms` | One alarm run. `due` lists kinds, never a peer. |
| `socket_close` | info, warn | `endpoint`, `by`, `code`, `reason`, `clean`, `error` | A relay socket closed by the peer (`by` `peer`) or with an error. |
| `admin_auth` | info, warn | `ok`, `path` | A mint or destroy token was checked. |
| `alarm_failed` | warn | `endpoint`, `due`, `error`, `retry_in_s` | The app's `onAlarm` threw; the work runs again later. |
| `registry_register` | info | `endpoint`, `from`, `ok` | The router recorded a new endpoint (`from: 'mint'`). |
| `registry_entry`, `registry_remove` | info | `endpoint`, `app` | The registry stored or removed an entry. |
| `mint_failed` | error | `endpoint`, `app`, `error` | The object refused the mint; the registry entry is removed. |
| `route_unknown_app` | warn | `endpoint`, `app` | The registry names an app this deployment does not have; 404. |

The bind marker is `bound_at_ms` in `iroh_state`: set when the node binds an endpoint,
cleared when it closes the endpoint after `t_linger`. Hibernation happens only in Warm,
with no endpoint, so it never leaves the marker set.

## Differences from `@number0/iroh`

The Connection and stream API follows `@number0/iroh` 1.1.0, iroh's Node bindings, so that
stream code carries over.

| `@number0/iroh` | iroh-do | Why |
|---|---|---|
| `Array<number>` for bytes and ALPNs | `Uint8Array` | Workers APIs use `Uint8Array`; an array of numbers costs a copy and boxing per call |
| `Endpoint.acceptNext()` loop | `protocols()` returning `{ [alpn]: { accept } }` | An object cannot wait in a loop across hibernation. This is the shape of iroh's own `ProtocolHandler` |
| `Endpoint.bind`, builder, presets | none; `this.endpoint` has `id()`, `addr()`, `peers()`, `connect()` | The host binds on wake, from stored identity |
| relays, discovery, paths, datagrams, stats, network watches | not exposed | One relay path, no UDP |
| `EndpointId.toString()` only | adds `toZ32()`, `fromZ32()` | URLs and DNS labels use z-base-32 (004) |
| — | no stream guard API | The stream wrappers count themselves (above) |
| — | returning from `accept` closes the connection | iroh's `Router` semantics, without waiting for the garbage collector |

## Tests

```sh
npm ci                                            # repository root
cargo build -p iroh-do-test-peer                  # the stock test peer
cd packages/iroh-do && npm run check && npm test  # tsc; lifetime and panic tests
cd examples/echo/server && npm run check && npm run build && npm test   # admin and evict tests
```

- `test/lifetime-test.mjs`: the connection lifetime rules and dial-back, with
  `test/fixture.ts`.
- `test/panic-test.mjs`: panics in one object, recovery of both objects in the isolate.
- `examples/echo/server/test/admin-test.mjs`: mint with pkarr (a fake relay), destroy.
- `examples/echo/server/test/evict-test.mjs`: forced hibernation, idle and silent closes,
  `evicted_active`.
- `examples/ts-deployment/test/router-test.mjs`: mint by app, the endpoint list, class
  resolution through the registry and its cache, destroy.
- `examples/ts-deployment/test/room.test.mjs` (Vitest): the chat room with two present stock
  `iroh-do-chat` members across a forced hibernation, dial-back delivery and connection
  reuse.

The peers are `crates/iroh-do-test-peer` and `examples/chat/client` (`iroh-do-chat`): stock
iroh with default features, no iroh-do crate. The tests use ports 8818–8821, 8830 and 8832
and, for the fake pkarr relay, 8921.
