# Writing an app

For iroh developers who want to serve their own protocol from a Cloudflare
Durable Object with iroh-do: iroh's echo example from a new crate to a running
endpoint, then what differs from an ordinary, long-running iroh endpoint. The
API's design is design note 011; where it and the code differ, this guide
follows the code.

## What you get, and what it costs

- **One endpoint per object.** Each minted endpoint `E` is one Durable Object,
  named by `E`, with its own key and SQLite database. Stock iroh peers dial it
  by EndpointId with default discovery: the object publishes a pkarr record
  with the relay URL `https://<E>.<zone>/` and serves the relay protocol there.
- **Handlers are ordinary iroh `ProtocolHandler`s**, the trait iroh's `Router`
  takes. The echo handler below is iroh's own, unchanged.
- **Relay only.** Workers have no UDP: no direct paths, no hole punching.
- **The endpoint exists only while there is work.** The object binds an
  `Endpoint` on a peer's first datagram, drops it once no connection or app
  task has existed for 2 s (`Config::DEFAULT_T_LINGER`), and may then
  hibernate, losing everything in memory. Section 5 lists the consequences,
  including that the object can dial only peers connected to it.

## 1. The echo app, step by step

### The crate

An app is an ordinary library crate that builds natively and for
`wasm32-unknown-unknown`. `examples/echo/app/Cargo.toml`:

```toml
[dependencies]
iroh-do = { path = "../../crates/iroh-do" }
iroh = { workspace = true }
# `tokio::io::copy`, as iroh's example uses it. No runtime: the host provides one.
tokio = { version = "1", default-features = false, features = ["io-util"] }
```

The workspace pins `iroh = { version = "=1.4.0", default-features = false }`.
The iroh-do crates are not published, so a crate outside this repository uses
a path or git dependency on `iroh-do`, and must pin iroh to `=1.4.0` as well.

### The handler

`examples/echo/app/src/lib.rs` copies `Echo` from iroh 1.4.0 `examples/echo.rs` with
only `pub` added:

```rust
pub const ALPN: &[u8] = b"iroh-example/echo/0";

#[derive(Debug, Clone)]
pub struct Echo;

impl ProtocolHandler for Echo {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let endpoint_id = connection.remote_id();
        println!("accepted connection from {endpoint_id}");
        let (mut send, mut recv) = connection.accept_bi().await?;
        let bytes_sent = tokio::io::copy(&mut recv, &mut send).await?;
        println!("Copied over {bytes_sent} byte(s)");
        send.finish()?;
        connection.closed().await;
        Ok(())
    }
}
```

(Comments trimmed.) The `println!`s print nothing on wasm32; see section 5.

### `impl App`

The `App` is what the object builds; it registers the handlers by ALPN:

```rust
use iroh_do::{App, Ctx, Protocols};

#[derive(Debug)]
pub struct EchoApp;

impl App for EchoApp {
    const NAME: &'static str = "echo";

    fn new(_ctx: &Ctx) -> Self {
        EchoApp
    }

    fn protocols(&self, _ctx: &Ctx, p: &mut Protocols) {
        p.accept(ALPN, Echo);
    }

    fn join_hint(id: &str) -> String {
        format!("iroh-do-echo-client {id}")
    }
}
```

`NAME` is what you pass at mint; the object stores it with its identity and
runs that app on every wake. `join_hint` is the command the endpoint list at
`admin.<zone>/` shows next to the endpoint.

### Adding it to a deployment

A deployment is one Worker script built from a `cdylib` crate that invokes
`iroh_do::worker!` once. This repository's is `examples/deployment`:

```rust
iroh_do::worker! { apps: [iroh_do_chat_app::ChatApp, iroh_do_echo::EchoApp] }
```

To add an app, add its crate to `examples/deployment/Cargo.toml` and its
type to the list. All endpoints of a deployment share one script, one Durable
Object class (`endpoint`) and the namespace `https://<E>.<zone>/`; the app is
a property of each endpoint. Minting without an app name uses the first.

A single-app deployment can put the `worker!` line in its own crate. That
crate must be a `cdylib` and depend directly on `worker` and `wasm-bindgen`
at the pinned versions, as `examples/deployment/Cargo.toml` does:

```toml
[lib]
crate-type = ["cdylib", "rlib"]

# `worker!` expands to workers-rs macros, which name these crates by absolute path.
[target.'cfg(target_arch = "wasm32")'.dependencies]
worker = { workspace = true }    # "=0.8.7"
wasm-bindgen = "=0.2.129"
```

Off wasm32 the macro expands to nothing, so the crate still builds natively.
The expansion defines the classes `endpoint` and `registry` and the `fetch`
handler; the Worker configuration binds them as `ENDPOINTS` and `REGISTRY`
(`deploy/BUILD.md`, "Bindings"). `build-worker.sh` in the same
directory runs `worker-build` 0.8.7 in the crate's directory; copy it.

## 2. Testing natively against stock peers

`crates/iroh-do-harness` (`iroh-do-harness`) hosts the same node under tokio and
serves the relay protocol for each object it mints, so stock iroh 1.4.0 peers
connect over loopback. A `DoObject` keeps what a Durable Object keeps
(identity, scheduled work, SQLite, sockets) and drops the rest on
`hibernate()` or `evict()`.

From `crates/iroh-do-harness/tests/library.rs`, trimmed:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn echo_example_round_trips_across_hibernation() {
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<EchoApp>(Timings::default());
    let ep = peer(vec![]).await;               // stock Endpoint, no relay of its own
    let conn = ep.connect(addr(&h, &obj), ECHO).await.expect("connect");
    assert_eq!(echo(&conn, b"hello").await, b"hello");
    conn.close(0u32.into(), b"bye!");
    warm_and_quiet(&obj, Duration::from_secs(10)).await;

    obj.hibernate().expect("hibernatable");    // drops the node and the app
    let conn = ep.connect(addr(&h, &obj), ECHO).await.expect("connect");
    assert_eq!(echo(&conn, b"after").await, b"after");
    let st = obj.node().unwrap().status();
    assert_eq!((st["wakes"].as_u64(), st["endpoint_builds"].as_u64()), (Some(2), Some(2)));
}
```

`addr` is `EndpointAddr::new(obj.id()).with_relay_url(h.relay_url(&obj.id()))`:
the harness publishes nothing. `warm_and_quiet` waits until the object is Warm
with no node tasks, as a Durable Object must be to hibernate.

- `h.mint::<A>(Timings)`: an object running `A` with a fresh key. `Timings`
  sets `t_idle`, `t_silent`, `t_linger` and `max_idle_timeout`.
- `obj.hibernate()`: drops node, app and sessions, keeps storage and sockets.
  It fails if the node is Active or has tasks, which would keep a Durable
  Object resident. `obj.evict()` drops them whatever the state.
- `obj.with_sql(|sql| …)` and `obj.with_app::<A, _>(|app| …)`: the object's
  database and the live app value, for assertions.
- `obj.node()` (status, stats, `ctx()`), `obj.state()`, `obj.wakes()`.

The same file has a test app, `CallBack`, that uses `Ctx::schedule`,
`App::on_alarm` and `Ctx::connect` to dial a peer back after hibernation, and
`Flaky`, whose alarm work fails twice before it succeeds.
`cargo test -p iroh-do-harness --test library` ran its 7 tests in 6.24 s on
2026-10-22.

## 3. Running it locally on workerd

`wrangler dev` runs the real bundle in workerd, with no Cloudflare account
(tools: README, "Try it"). Port 8787 is wrangler's default; `LOCAL_PORT`
picks another.

```sh
LOCAL_PORT=8799 just local-up    # build the bundle, start wrangler dev, mint a chat room
LOCAL=1 just mint echo           # mint an endpoint running the echo app
LOCAL=1 just echo <E>            # dial it with iroh-do-echo-client
just local-down
```

Output of a run on 2026-10-22, trimmed:

```
$ LOCAL_PORT=8799 just local-up
index_bg.wasm:  3795846 B raw, 1461036 B gzip -9 (limit: 64 MiB uncompressed)
wrangler dev (pid 56191) starting on :8799; log: .../.demo/local-up.log
room:  trrffnn9e5k6mq5fkik6f36748zb44oqacbiue8fefhkxzohxroy

$ LOCAL=1 just mint echo
E          = cg39q74ipjkm6h5f1qcisw76o5oxj4t3udg3s6uib7f8gxitfk9y
app        = echo
relay URL  = http://cg39q74ipjkm6h5f1qcisw76o5oxj4t3udg3s6uib7f8gxitfk9y.localhost:8799/
published  = false (pkarr, note 004)
saved to .../.demo/endpoint.local.echo

$ LOCAL=1 just echo cg39q74ipjkm6h5f1qcisw76o5oxj4t3udg3s6uib7f8gxitfk9y
Hello, world!
connected in 40 ms, echoed 13 bytes in 54 ms
```

Nothing is published for `localhost`, so the client puts
`http://<E>.localhost:<port>/` in the address (`--local <port>`). The status
JSON is at `admin.localhost:<port>/status/<E>` (`"app":"echo"`, `"state"`,
`"wakes"`, `"endpoint_builds"`), and the object's events (`endpoint_bind`,
`conn_open`, `conn_close` with its reason, `endpoint_close`) are in
`.demo/local-up.log`.

`LOCAL=1 just mint APP` writes the new `E` to `.demo/endpoint.local.APP`;
`local-up` keeps its own room in `.demo/endpoint.local`.

## 4. Deploying

Deploying is the deployment owner's step: it changes live Cloudflare
resources. The commands, from the `justfile` and `deploy/tofu/README.md`:

```sh
just up                  # build the bundle; tofu init + apply; wait for the certificate
just mint echo [LABEL]   # POST https://admin.<zone>/mint {"app":"echo"}; prints E
iroh-do-echo-client <E>  # or: just echo <E>
```

`just up` needs `CLOUDFLARE_API_TOKEN` and an `examples/deployment/infra/<name>.auto.tfvars`;
`deploy/tofu/README.md` lists the token's permissions. Deploying any app redeploys
the whole script, which restarts every live object of every app. In
production the object publishes its pkarr record at mint, so a peer needs
only `E`: `examples/echo/client` binds `Endpoint::bind(presets::N0)` with iroh's
default features and connects to `EndpointAddr::new(id)`.

## 5. What differs from a long-running iroh endpoint

### The app value and the handlers are short-lived

The object builds the app once per construction (`App::schema`, then
`App::new`): on every wake from hibernation, after an eviction, and after a
wasm panic. `App::protocols` runs on every activation, before the endpoint
binds, so handlers are rebuilt with each endpoint, as iroh builds a `Router`
per `Endpoint`. Fields of the app value last across activations of one object
instance and no longer: use them for caches and rate-limit windows, never for
anything a peer must see again later.

### State that must survive goes in SQL

`Ctx::with_sql(|sql| …)` runs a closure on the object's SQLite database. It is
synchronous, as the Durable Object's `sql.exec` is: do the SQL, return the
result, then await. `Sql::exec(query, params)` takes positional parameters and
returns rows of `Value`; integers beyond ±2^53 lose precision on workerd
(`crates/iroh-do-core/src/sql.rs`). `with_sql` returns `None` if the object
was destroyed. The chat wraps it to log errors (`examples/chat/app/src/lib.rs`,
`Chat::sql`).

`App::schema` returns statements run on every construction of a minted object,
so they must be idempotent (`CREATE TABLE IF NOT EXISTS`). There is no
migration path. The library's own tables are `identity`, `counters`,
`schedule` and `pkarr_state` (`crates/iroh-do/src/schema.rs`). Do not use
those names: nothing checks for a collision, and `CREATE TABLE IF NOT EXISTS`
on an existing name succeeds silently against the library's table.

### No timers or background tasks of your own

An object is Warm, and may hibernate, only with no endpoint, no node task and
no timer (`docs/design.md`, "Lifecycle"). iroh-do accounts only for work it
knows about:

- **Later work:** `Ctx::schedule(key, delay, payload)` persists the work and
  arms the object's alarm. When due, the object (possibly a new construction,
  after hibernation) awaits `App::on_alarm(ctx, alarm)`, where `alarm` holds
  the key, the payload and `attempt`, the number of earlier runs that did not
  complete. A second `schedule` with the same key replaces the first;
  `Ctx::cancel(key)` deletes it. Keys are prefixed with `app:`
  (`APP_KEY_PREFIX`) before storage, so they cannot collide with the
  library's own (`republish`, `sweep`); `on_alarm` receives the key without
  the prefix.
- **Alarm work runs at least once.** The object keeps the work until
  `on_alarm`'s future returns `Ok`. If it returns an error, or the object ends
  while it runs (eviction, a panic), the work runs again 30 s later, then after
  1 min, 2 min and so on up to 1 h (`alarm_retry_delay`). Do the work inside
  the future rather than spawning it: spawned work is lost if the object ends,
  and its failures are not retried. Return an error only for failures worth
  repeating the work for.
- **Async work:** `Ctx::spawn(fut)` starts a task for work that is not tied
  to an alarm. The node counts the task: the endpoint stays bound while
  it runs, the object reports Active (so it is not hibernated under the task,
  even with no endpoint bound), and a task left over when the endpoint closes is reported in
  `Stats::teardown_leftover_tasks`. A task started with `n0_future::task::spawn`
  is not counted. Handler futures need no `spawn`: the node runs each accepted
  connection's `accept` as its own counted task, and `on_alarm` keeps the
  endpoint bound while it runs.
- **No timers that outlive a connection.** A `sleep` inside a handler while its
  connection is open is fine; a loop that ticks while no peer is connected
  keeps the object resident.

`CallBack` in `crates/iroh-do-harness/tests/library.rs` shows the pattern: the handler
schedules, and `on_alarm` dials.

### Idle connections are closed by the object

iroh 1.4.0 sends a QUIC keep-alive every 5 s that its public API cannot turn
off, so QUIC's idle timeout never fires and an open connection would keep the
object billed indefinitely. The node closes a connection itself, with
application code 0 and reason `idle`, after its idle limit without
application data: `Config::t_idle`, 10 s by default, or the limit given with
`Protocols::accept_with(alpn, handler, Idle::after(d))`, which also applies to
connections the app dials with that ALPN.

By default the node observes application data by sampling the connection's
STREAM frame counts every quarter of the limit, so the close comes between
one and one and a quarter limits after the last data (`crates/iroh-do-core/src/conn.rs`).
Keep-alives are PING and ACK frames and do not count. iroh's echo handler
needs nothing more: it waits in `connection.closed()`, and a peer that never
closes is closed after `t_idle` (test `idle_close_without_handler_reports`).

A protocol that holds a stream open with long gaps between messages (a
subscription, an interactive session) is closed as idle under this default.
Either raise its limit with `accept_with`, or report activity exactly:

```rust
let activity = ctx.activity(&conn);                  // Option<Arc<Activity>>
let _open = activity.as_ref().map(|a| a.stream());   // StreamGuard: one per open stream
// ... on each message: if let Some(a) = &activity { a.touch(); }
```

Once a handler has reported through `Activity`, the node stops sampling frames
for that connection and closes it exactly one limit after the last report.
While any `StreamGuard` is alive the limit is `Config::t_silent` (60 s by
default) and the reason `silent`. The chat's handlers work this way
(`examples/chat/app/src/ctl.rs`, `examples/chat/app/src/room.rs`). A handler that reports must
report all of its activity, because frame counts are then ignored.

Under iroh's `Router`, a connection is closed when its handler returns and
drops it. The same holds here: the node keeps only a weak handle.

### Dialing out reaches only connected peers

The object has no outbound relay connection. Its only paths are the relay
sockets peers opened to it, so `Ctx::connect(peer, alpn)` succeeds only for a
peer in `Ctx::peers()` (`Ctx::has_socket(&peer)`); otherwise it returns
`ConnectError::NoSocket` at once and binds nothing. A stock client closes its
socket to a relay that is not its home relay 60 s after it last sent anything
(`docs/design.md`, "Lifecycle"), so the window for a dial-back is short unless
the peer makes the object its home relay, as the chat client does.

`Ctx::connect` binds the endpoint if needed, and the dialed connection counts
like an accepted one for the linger and the idle close. The ALPN dialed need
not be registered in `Protocols`.

Protocols that dial arbitrary peers therefore do not work: gossip, which dials
its neighbors; blob transfer that fetches from other providers; anything that
discovers a peer and connects to it. A protocol where peers connect to the
object, and the object answers or calls back while they are connected, does.

### wasm32-unknown-unknown

- **Every dependency must build for `wasm32-unknown-unknown`.** Check with
  `cargo build -p <app> --target wasm32-unknown-unknown`; on macOS set the
  Homebrew LLVM `clang` and `llvm-ar` as `build-worker.sh` does if anything
  pulls in ring.
- **No std clock.** `std::time::Instant::now()` and `SystemTime::now()` panic.
  Use `n0_future::time::{Instant, SystemTime, sleep}`, as the chat does.
- **No tokio runtime.** The host drives futures (`spawn_local` on wasm32).
  Use tokio only for features that need no runtime, such as `io-util`;
  `tokio::spawn` and `tokio::time` need a tokio runtime, which is not there.
- **`println!` goes nowhere.** Use `tracing`. The object writes every event as
  one JSON line; the default filter, `LOG_LEVEL`, logs `info` and above from
  app crates.
- **Handler futures must be `Send`**, as iroh's `ProtocolHandler` requires,
  even on single-threaded wasm32.

## 6. API reference

`iroh_do` re-exports `App`, `AppEntry`, `Ctx` and `Protocols`, and the rest of
`iroh_do_core` as `iroh_do::core` (`Idle`, `Activity`, `StreamGuard`,
`ConnectError`, `Sql`, `Value`, `State`).

| `App` (`Send + Sync + 'static`) | |
|---|---|
| `const NAME` | Name given at mint and stored with the identity; unique within a `worker!` list. |
| `fn new(ctx) -> Self` | Once per object construction, after `schema`. |
| `fn protocols(&self, ctx, p)` | Every activation, before the endpoint binds; the endpoint accepts exactly these ALPNs. |
| `fn schema() -> &[&str]` | Idempotent SQL run on every construction of a minted object. Default: none. |
| `async fn on_alarm(&self, ctx, alarm) -> Result<(), AlarmError>` | Work scheduled with `Ctx::schedule` is due; kept and run again until it returns `Ok`. |
| `fn status(&self, ctx) -> Map` | Extra fields for the status JSON (`admin.<zone>/status/<E>`). |
| `fn join_hint(id) -> String` | The command shown on the endpoint list; `id` is z-base-32. |

| `Ctx` (cheap to clone; does not keep the node alive) | |
|---|---|
| `id()` | This endpoint's EndpointId. |
| `with_sql(\|sql\| …) -> Option<T>` | Synchronous SQL; `None` if the object was destroyed. |
| `transaction(\|sql\| …) -> Option<Result<T, SqlError>>` | Synchronous SQL in one storage transaction: committed if the closure returns `Ok`, rolled back otherwise. |
| `schedule(key, delay, payload)` / `cancel(key)` | Durable work for `App::on_alarm`, replacing pending work with the same key; kept until a run succeeds. |
| `spawn(fut)` | A counted node task; keeps the endpoint bound while it runs. |
| `peers()` / `has_socket(&peer)` | Peers with an authenticated relay socket: the only peers the object can dial. |
| `connect(peer, alpn).await` | Dials over the peer's socket, binding the endpoint if needed. Errors: `NoSocket`, `Gone`, `Bind`, `Connect`. |
| `activity(&conn)` | The connection's `Activity`, for exact idle tracking (`touch`, `stream`). |
| `wakes()`, `state()`, `status()` | Constructor runs, Cold/Warm/Active, the status JSON. |

| `Protocols` | |
|---|---|
| `accept(alpn, handler)` | Registers any iroh `ProtocolHandler`, as `RouterBuilder::accept`. A later call for the same ALPN replaces the earlier one. |
| `accept_with(alpn, handler, Idle::after(d))` | The same, with this ALPN's idle limit instead of `t_idle`, for accepted and dialed connections. |
