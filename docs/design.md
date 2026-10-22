# Design

This document describes iroh-do as built, against iroh 1.4.0, workers-rs 0.8.7
and the Cloudflare runtime as of October 2026. The original plan is
[`spec.md`](spec.md); where the two differ, this document and the
[decision log](design-notes/) are current. Claims about iroh and Cloudflare
behaviour are backed by [`verification/`](verification/), which cites the
pinned sources line by line.

## Overview

```
 peer (stock iroh)                       Cloudflare
 ─────────────────                       ──────────────────────────────────────────────
 address lookup ──── pkarr: E → relay URL https://<E>.<zone>/
 relay client ── wss://<E>.<zone>/relay ──► Worker router (stateless)
                                              │ label <E> → idFromName(E) → stub.fetch
                                              ▼
                                           Durable Object "E" (class endpoint)
                                              ├─ ingress: relay protocol server, one session per socket
                                              ├─ hibernatable WebSockets, session state in attachments
                                              ├─ iroh Endpoint, only while Active
                                              │    ├─ custom transport: one path per connected peer
                                              │    └─ the app's protocol handlers, by ALPN
                                              └─ SQLite: identity (with the app's name), counters,
                                                 schedule, pkarr_state; the app's own tables

                                           Durable Object "registry" (class registry, one instance)
                                              └─ SQLite: the list of minted endpoints
```

`E` is the endpoint's EndpointId. The object named `E` is the only place that
holds `E`'s secret key, and the only server that accepts relay traffic for `E`.

Each endpoint runs one app, chosen when it is minted: a set of iroh protocol
handlers, its schema and its scheduled work (see [Apps](#apps)). This
deployment has two, the chat room and iroh's echo example; all endpoints,
whatever their app, share the namespace `<E>.<zone>`.

The Worker script is `iroh-do`, built from `examples/deployment`
([`design-notes/012`](design-notes/012-plumbing-and-examples.md)), which lists
its apps in `iroh_do::worker!`. Its classes are exported as `endpoint` and
`registry`, so Cloudflare names the namespaces `iroh-do_endpoint` and
`iroh-do_registry`; the router reaches them through the bindings `ENDPOINTS`
and `REGISTRY`.

## Addressing

An endpoint's relay URL is `https://<E>.<zone>/`, where `<E>` is the
EndpointId in z-base-32. The URL is a function of the key, so it cannot drift
from the identity.

The destination has to be in the hostname. The iroh relay client builds its
WebSocket URL by replacing the relay URL's path with `/relay`
(`iroh-relay` 1.4.0, `src/client.rs`, `set_path(RELAY_PATH)`), so a path such
as `https://<zone>/e/<E>/` is lost before the request is sent. The query string
survives `set_path`, but iroh's TXT record parser splits attributes on every
`=`, so a query-based URL would rest on two unspecified behaviours. See
[`design-notes/001`](design-notes/001-path-form-relay-url.md).

The label is z-base-32 (52 characters) because the default text form of an
EndpointId is 64 hex characters, longer than the 63-octet limit for a DNS
label. Ids in labels are parsed only with `PublicKey::from_z32`: `FromStr`
reads 52-character input as standard base32 and silently returns a different
key for some inputs ([`design-notes/004`](design-notes/004-discovery-and-id-encoding.md)).

## The router

One Worker script hosts both the router and the Durable Object class
(`crates/iroh-do/src/glue/router.rs`, `crates/iroh-do/src/route.rs`). The
router reads the leftmost label of the request host:

- `admin`: `POST /mint` creates an endpoint (see [Minting](#minting));
  `GET /` and `GET /endpoints.json` list minted endpoints, and
  `GET /status/<E>` returns one endpoint's status (see
  [Endpoint list](#endpoint-list)).
- a valid z-base-32 EndpointId: `GET /relay` with a WebSocket upgrade is
  forwarded to the object `idFromName(<E>)`. `/ping` and `/generate_204` are
  answered by the router.
- anything else: 404, without touching an object.

Paths under `/_internal/` are used for router-to-object calls and are never
forwarded from outside. These calls use `stub.fetch`: worker-build 0.8.7
wraps Durable Object classes in a JavaScript proxy that does not extend
`DurableObject`, so Workers RPC to the object fails at runtime.

## The ingress

`crates/iroh-relay-ingress` is the server side of the iroh relay protocol as a
sans-io state machine: the host owns the socket and the clock and feeds it
messages. It accepts only peers that prove their key, and only datagrams
addressed to `E`. It has no forwarding code path.

The wire format is implemented from
[`verification/relay-protocol.md`](verification/relay-protocol.md) §8 rather
than reused from `iroh-relay`, whose codec is `pub(crate)` and whose crate
pulls in tokio and hyper. Golden tests compare every frame against the
upstream crate's own encoder and decoder
(`crates/iroh-relay-ingress/tests/conformance.rs`, `golden.rs`).

**Version.** The client offers `Sec-WebSocket-Protocol: iroh-relay-v2,
iroh-relay-v1`. The ingress picks the highest it supports and the 101 response
echoes exactly that token; the client fails otherwise. Upgrades with no common
version are rejected.

**Authentication.** iroh clients can authenticate in the upgrade request with
the `x-iroh-relay-client-auth-v1` header, whose signature covers TLS exporter
keying material. TLS terminates at Cloudflare's edge, so the object cannot
compute that material and ignores the header. It always sends a 16-byte
`ServerChallenge` as the first message, and the client answers with a
signature over it. A stock client completes this path even after sending the
header (verification V3). The challenge must be answered within 30 s; the
deadline is enforced from the object's alarm, not from a timer, because a
pending timer prevents hibernation.

**Destination filter.** After authentication the peer may send datagrams only
to `E`. Datagrams for any other destination are dropped and counted; after 32
the session is closed. The relay protocol has no "not here" signal for unknown
destinations, so dropping is what an upstream relay does too.

**Keep-alive.** Clients ping every 15 s. The session answers with a Pong
without involving the `Endpoint`, so a ping wakes a hibernated object briefly
but does not activate it.

**One socket per peer.** A second authenticated socket from the same peer
replaces the first. The old socket receives `SameEndpointIdConnected` and is
closed with code 4000.

**Hibernation.** A session serializes to about 35 bytes. The object stores it
in the WebSocket's attachment when the peer authenticates and whenever the
destination-violation count changes, and restores it when the object is
recreated.

## The transport

The object's `Endpoint` has no IP transport, no relay transport and no
address lookup (`crates/iroh-do-core/src/endpoint.rs`). Its only transport is
`crates/iroh-ingress-transport`, an iroh custom transport
(`unstable-custom-transports`) in which every path is a peer's relay socket:

- The path address is a `CustomAddr` holding the peer's 32-byte EndpointId,
  under the provisional transport id `0x494E4752`. It is local and never
  published.
- Received relay datagrams are pushed into the transport with the sender's id
  as the source address.
- Transmits to `CustomAddr(peer)` go to the host, which encodes them as relay
  frames and writes them to that peer's socket. If the peer has no socket, the
  send fails with a no-route error. The transport never dials out.

To the peer, this is an ordinary relay path to `E` through `E`'s home relay.
Only the object's side is a custom transport. iroh's relay transport is a
client that connects to a relay; here the endpoint is the relay, and a custom
transport expresses that without changing iroh. The object's endpoint never
initiates hole punching and never dials the IP or relay addresses a peer
advertises; it accepts connections from peers it has not seen before
(verification V14).

## Apps

The library hosts an app; it does not know what the app does
([`design-notes/011`](design-notes/011-app-api.md)). An app implements
`iroh_do_core::App` (`crates/iroh-do-core/src/app.rs`):

- `protocols` registers iroh `ProtocolHandler`s by ALPN, the trait iroh's
  `Router` takes. It runs on every activation, before the endpoint binds, and
  the endpoint accepts exactly those ALPNs. The node dispatches accepted
  connections as `Router` does: `on_accepting`, then `accept`.
- `schema` is SQL the object runs on every construction of a minted object.
- `on_alarm` receives work the app scheduled.
- `status` adds fields to the endpoint's status.

One app value exists per object construction. It reaches the node through
`Ctx` (`crates/iroh-do-core/src/ctx.rs`): synchronous SQL, scheduling, the
peers that hold a relay socket, dialing them, spawning tasks, and activity
reports for the idle close.

The deployment is a crate that invokes `iroh_do::worker! { apps: [...] }`,
which declares the Durable Object classes and the Worker's `fetch` handler
there (`crates/iroh-do/src/lib.rs`). An endpoint's app is stored in its
identity at mint; an object whose app is missing from the running build
refuses all traffic.

The apps in this repository are `examples/chat/app` (the chat room and the diagnostic
protocols, design notes 006–008) and `examples/echo/app` (iroh's `examples/echo.rs`
handler, unchanged). `docs/writing-an-app.md` describes writing one.

## The TypeScript host

[`design-notes/013`](design-notes/013-typescript-host.md) moves the Durable
Object class to TypeScript. The move is in progress. The Rust host described in
the rest of this document still serves the deployment `iroh-do`. The TypeScript
host serves a second deployment, `iroh-ts` (`*.iroh-ts.<zone>`), which runs the
echo app.

- `crates/iroh-do-wasm` builds the ingress, the node, the endpoint and the
  transport into one wasm module with `wasm-bindgen --target web`, without
  workers-rs. It holds no Cloudflare types. TypeScript passes it socket ids,
  bytes and session snapshots, and receives frames through one callback.
- `packages/iroh-do` exports `IrohDurableObject` and `irohRouter`.
  - The class owns the platform: the relay upgrade, hibernatable WebSockets and
    their attachments, the alarm and the schedule table, the identity and the
    counters in `iroh_*` tables, and Workers RPC for mint, status and destroy.
  - Apps register handlers by ALPN in the shape of iroh's `ProtocolHandler`,
    and use `this.ctx.storage.sql` directly.
  - The connection and stream API follows iroh's Node bindings,
    `@number0/iroh`. The package README lists the differences.
- An accepted connection is closed with reason `done` when its handler's
  promise settles. Stream halves count as open until they are finished, reset,
  read to the end or stopped, which drives the idle and silent close without a
  guard API.
- A panic traps the isolate's wasm instance. The class reinstantiates the
  module, and every object rebuilds its node on its next event from storage and
  socket attachments, as on a wake.
- An object constructed while its endpoint was bound logs `evicted_active`.
- The build writes a `manifest.json` naming the bundle's modules. `deploy/tofu`
  uploads a deployment from it.

Evidence, from Miniflare and from production, is in
[`verification/ts-host.md`](verification/ts-host.md).

## Lifecycle

```
           peer's relay socket                         first datagram,
           authenticated                               or a dial by the app
  ┌──────┐ ────────────────────────► ┌──────┐ ──────────────────────────► ┌────────┐
  │ Cold │                           │ Warm │                             │ Active │
  └──────┘ ◄──────────────────────── └──────┘ ◄────────────────────────── └────────┘
           last socket closed                  no connections or app tasks
           by the peer                         for t_linger; Endpoint closed
                                               and dropped
```

The state machine is `crates/iroh-do-core/src/lib.rs`; the object drives it
from `crates/iroh-do/src/glue/endpoint.rs`.

**Cold.** No authenticated socket. The object is not in memory, or has just
been created.

**Warm.** At least one authenticated socket, no `Endpoint`, no tasks, no
timers. The object is eligible for hibernation, and Cloudflare evicts it from
memory while its WebSockets stay open. With Miniflare, a Warm object that has
already run an `Endpoint` has no pending timers or fetches and is evicted on
demand in 5 ms (`tests/workerd/evict-test.mjs`).

**Active.** An `Endpoint` is running, or an app task spawned with `Ctx::spawn` is. The first datagram from a Warm state
starts the build; datagrams that arrive during the build wait in a FIFO queue
of 128 frames, and later ones are dropped. A dial by the app (`Ctx::connect`)
also activates the object.

Transitions out of Active:

| Timer | Default | Effect |
|---|---|---|
| `t_idle` | 10 s | A connection with no application data for this long is closed with reason `idle`. An app may set another limit per ALPN. |
| `t_silent` | 60 s | Replaces `t_idle` while a handler reports an open stream; the close reason is `silent`. |
| `t_linger` | 2 s | Once no connection is open and no app task runs, the `Endpoint` is closed and dropped after this long with no new work. The node then checks that none of its tasks remain. |

The object cannot rely on QUIC's idle timeout to end connections. iroh sends
a keep-alive on every path every 5 s, which its public API cannot disable,
and stock peers do the same, so a QUIC connection never goes idle
([`design-notes/005`](design-notes/005-idle-close-and-restart-state.md)).
`t_idle` and `t_silent` are the object's own policy. Peers see a normal
application close and redial when they next have something to send.

The node sees application data in one of two ways
(`crates/iroh-do-core/src/conn.rs`). By default it samples a connection's
STREAM frame counts every quarter of the shorter limit; keep-alives are PING
and ACK frames and leave them alone (design note 011, phase 0), so the close
comes between one and one and a quarter limits after the last data. A handler
that reports activity through `Ctx::activity` gets exact timing instead, and
the `idle`/`silent` distinction, which needs to know whether a stream is open.
The chat's handlers report activity.

Transitions out of Warm are the peer's decision. The object never closes an
idle relay socket. The iroh client closes its connection to a relay that is
not its own home relay 60 s after it last sent anything, and reconnects
immediately if the server closes it instead (verification V5). When the last
socket closes, the object is Cold.

## Time

On Cloudflare, `Date.now()` and `performance.now()` advance only between
events; within one event, time stands still. iroh's QUIC implementation (noq)
takes `now` as an explicit input sampled at event boundaries, which matches
this clock. Its timers measure time between packet arrivals and timer
firings, and those are the moments at which the clock advances.

The clock never has to account for a suspension, because no QUIC connection
survives one. An object with a connection has pending timers and cannot
hibernate; a Warm object has no `Endpoint`. Local workerd does not freeze the
clock during execution; the design works with either behaviour.

One interaction with the frozen clock needed a fix. n0-future converts timer
durations to whole milliseconds by truncation before calling `setTimeout`, so
a noq timer can fire up to 1 ms before its deadline. Inside a timer callback
the production clock reads exactly the scheduled time, so noq finds its
deadline not yet reached and re-arms with a zero delay, and the clock does not
move until some other event arrives. Measured in production, this spun for
1.5–3.6 s of CPU per session. The Worker adds 1 ms to every `setTimeout` delay
(`crates/iroh-do/src/glue/timers.rs`); `tests/workerd/clock-test.mjs`
emulates the production clock and fails without the fix. The root cause is
the truncation in n0-future, which rounding up would remove.

## Persistence

Each object uses its own SQLite database through the synchronous `sql.exec`
API (`crates/iroh-do/src/glue/store.rs`). Nothing is written per packet.

The library's tables (`crates/iroh-do/src/schema.rs`):

| Table | Contents |
|---|---|
| `identity` | The endpoint's app, secret key and EndpointId. Written once, at mint. |
| `counters` | `wakes` (object constructions), `endpoint_builds`, `registered`. |
| `schedule` | Alarm work: pkarr republish, the handshake-deadline sweep, and the app's work under keys prefixed `app:`. One alarm is armed for the earliest row. |
| `pkarr_state` | Result of the last publish. |

The app's tables follow, from its `App::schema`. The chat's
(`examples/chat/app/src/store.rs`):

| Table | Contents |
|---|---|
| `log` | The message log: room events and diagnostic requests (newest 10,000 rows, none older than 14 days). |
| `log_meta` | `log_seq` and `room_seq`, which keep `seq` and `prev` monotonic across pruning. |
| `members` | Chat room members: peer id, nick, whether joined. |

The registry object holds one table, `endpoints(id, app, label, minted_at)`.

Session state lives in WebSocket attachments, not in SQLite. The schema is
created as-is on first use; there is no upgrade path, so changing it means
resetting the deployment's state.

On every construction the object checks that the stored key's public key
equals the object's name. On a mismatch it refuses all traffic. An object
with no stored identity writes nothing, so requests for random names create
no storage.

What is deliberately not stored: QUIC connection state, connection ids,
congestion and RTT state. A restored connection would be inconsistent with
the peer's timers, which kept running during the suspension.

Two pieces of state that would make restarts cheaper cannot be stored with
stock iroh 1.4.0, because iroh creates them per bind and offers no way to
supply them: the TLS session store, which 0-RTT resumption needs, and the
stateless-reset and connection-id keys, which let a new instance reset a
previous instance's connections. See [Restart state](#restart-state).

## Dial-back

The object can connect to a peer that holds a relay socket to it, through
`Ctx::connect`. In the chat app's diagnostic protocol (`iroh-do/ctl/1`), a peer
sends `/callback <secs>`. The app schedules work through `Ctx::schedule`, which
stores a schedule row and arms an alarm. When the alarm fires, the app calls
`Ctx::connect`, which starts the `Endpoint` if needed and calls
`Endpoint::connect` with an `EndpointAddr` whose only address is the peer's
`CustomAddr`. The QUIC
handshake goes out over the peer's existing relay socket. A stock iroh client
accepts an incoming connection that arrives through a relay other than its
home relay (verification V20).

The window is limited by the client, which closes the socket 60 s after its
own last send; traffic from the object does not extend it. The demo therefore
caps `/callback` at 50 s. A dial-back that finds no socket is recorded in the
log as `failed: no socket for peer`. Dialing a peer that is not connected
would require an outbound relay connection, which keeps the object in memory
while open. iroh-do does not do this. The chat client avoids the window by
making the room its home relay; see [The chat room](#the-chat-room).

## The chat room

An endpoint minted with the chat app is a small chat room (`iroh-do/room/1`,
`examples/chat/app/src/room.rs`; protocol in
[`design-notes/008`](design-notes/008-chat-room.md)). Members run
`iroh-do-chat <E>`. Messages, actions, joins, parts and nick changes are
stored in the `log` table, so scrollback survives hibernation, eviction and
redeploys.

**Members do not hold connections.** A chat built on Durable Object
WebSockets can hibernate between messages with every member connected,
because a WebSocket has no timers. A QUIC connection has timers (loss
recovery, keep-alive, idle), and an object with pending timers cannot
hibernate, so a member holding a connection would keep the room Active. In
iroh-do a connection exists only while there is something to say. A member
dials the room to send a request. To deliver an event, the room dials each
present member over the member's relay socket (dial-back), batching events
produced within 50 ms on one stream. Either side reuses an open connection
before dialing. After the last connection closes, the normal `t_idle` and
`t_linger` path returns the room to Warm.

**Presence through the home relay.** The room can dial only members whose
relay socket to it is open, and a stock client closes a socket to a relay that
is not its home relay 60 s after its last send. The chat client therefore sets
the room's relay URL as its home relay (`RelayMode::Custom`, stock iroh). A
client keeps its home relay connection open and pings it every 15 s, so a
member is present while its client runs. The cost is that the chat client can
reach other endpoints only through the room, which forwards nothing.

**Catch-up.** Every stored room event carries its `seq` and the `seq` of the
previous room event. A client that sees a gap, or that restarts, asks for the
events it missed with `/since`.

**Cost.** The room is Active while messages flow and returns to Warm about
12 s after the last one. Each present member adds one short wake per 15 s
ping: the object must echo the ping's random payload, which Cloudflare's
WebSocket auto-response (fixed text pairs) cannot do. Each delivery to a
member without an open connection costs a QUIC handshake from the object.
This suits a quiet room with a few members. A busy room or many members would
be better served by members holding connections, which requires the object to
hibernate with idle QUIC connections open; see [Open questions](#open-questions).

## Discovery

When an endpoint is minted, the object publishes a signed pkarr record for `E`
containing only its relay URL, with a TTL of 30 s, to n0's pkarr relay
(`https://dns.iroh.link/pkarr`). It builds the record with iroh's own
`EndpointInfo::to_pkarr_signed_packet` (`crates/iroh-do/src/pkarr.rs`). The
publish is confirmed by reading the record back, because the server answers
204 to a stale record without storing it. The object republishes every 24 h
from its alarm. The iroh DNS server expires records seven days after their
timestamp by default; n0's production setting is not published.

Publishing at mint, before the EndpointId is handed out, keeps resolvers from
caching a negative answer for the name. Peers with default discovery resolve
`E` to the relay URL with no configuration (verification V16).

## Minting

```
POST https://admin.<zone>/mint
Authorization: Bearer <ADMIN_TOKEN>
```

The router compares the token in constant time, generates a secret key in the
isolate, and sends it with the app's name to the object named after the new
EndpointId on the internal mint path. The body may name the app,
`{"app": "echo"}`; with none, the deployment's first app is used, and an
unknown name is a 400. The object stores the key and the app's name if it has
no identity yet, verifies the key against its name, creates the app's tables,
publishes the pkarr record, and returns `{id, app, relay_url, published}`. The secret is never returned and there is no
export path. The admin token is an OpenTofu variable; the keys never pass
through OpenTofu. The request body may carry a `label`, which the router
records with the new id in the registry.

`DELETE https://admin.<zone>/endpoints/<E>`, with the same token, destroys an
endpoint: the object closes its sockets, deletes its alarm and all of its
storage, key included, and the router removes `E` from the registry. The
object is then indistinguishable from one never minted
([`design-notes/009`](design-notes/009-endpoint-list.md)). There is no undo.

## Endpoint list

Cloudflare lists Durable Objects by internal id, not by the name they were
created from, so the set of EndpointIds cannot be read back from the
namespace. The router records each minted id, with its label and mint time, in
a registry object (class `registry`, one instance). An endpoint minted
before the registry existed registers itself once, on its next wake.

`GET https://admin.<zone>/` renders the list as plain HTML, newest first, with
each endpoint's app and the app's command for it (`App::join_hint`, for example
`iroh-do-chat <E>`); `GET /endpoints.json` returns the same data. Rendering
wakes only the registry. A row's live status (`GET /status/<E>`) is fetched
only on request, because it wakes that endpoint. The list is public: in this
deployment, endpoint ids are not secret, and anyone who knows one can connect
to it. Minting stays behind the admin token
([`design-notes/009`](design-notes/009-endpoint-list.md)).

## Failure handling

| Event | What the peer sees | Recovery |
|---|---|---|
| Idle or silent close | Application close, code 0, reason `idle` or `silent` | Redials when it next sends, with a full handshake |
| Active → Warm | Nothing; its connections were already closed | None needed |
| Abrupt eviction: deploy, crash, runtime decision | Packets on the old connection go unanswered until the negotiated QUIC idle timeout expires (10 s in `abrupt_restart_documented`, `examples/chat/app/tests/node.rs`) | Redials; the object rebuilds from storage |
| Peer closes its relay socket | n/a | The object drops the session; if it was the last, the object is Cold |
| Second socket from the same peer | Old socket closed with code 4000 | The new socket replaces the path |
| Alarm work fails (for example a storage error) | Dial-back or republish delayed | The row is retried after 30 s; other rows still run |
| Panic | Same as abrupt eviction | The wasm instance aborts and the object is reconstructed |

## Logs

Every `tracing` event, from iroh-do and from iroh, is written to Workers Logs
as one JSON object, through `console.log`, `console.warn` or `console.error`
by level. Span fields are flattened into the object, so iroh's own events
inside a node task carry `endpoint`. Events meant for operators have a stable
`event` field: `wake`, `state`, `endpoint_bind`, `endpoint_close`,
`ingress_auth`, `socket_close`, `conn_open`, `conn_close`, `room_request`,
`room_event`, `room_delivery`, `dial_back`, `alarm`, `pkarr_publish`, `mint`
and `registry_register`, among others. The `LOG_LEVEL` binding filters by
target and level; the default keeps iroh and noq at `warn`. No event carries a
key, the admin token or message text. Field names and queries are in
[`design-notes/010`](design-notes/010-structured-logs.md).

## Security

- **Peer authentication.** A peer proves its key by signing the server's
  challenge, with iroh's domain separation string. QUIC and TLS then
  authenticate both ends by raw public key, end to end.
- **No open relay.** The ingress delivers datagrams only to `E` and has no
  forwarding path. A peer that keeps sending to other destinations is
  disconnected.
- **Wake amplification.** Every upgrade wakes an object. The router rejects
  malformed EndpointIds without touching an object, objects without an
  identity write nothing, unauthenticated sockets are closed after 30 s, and
  queues are bounded (128 frames before activation, 1024 datagrams in the
  transport).
- **Residency.** `t_idle` and `t_silent` bound how long a peer can keep an
  object in memory without sending application data.
- **Key custody.** Keys are stored unencrypted, as hex, in the object's
  database.
  Cloudflare is in the endpoint's trusted computing base.
- **0-RTT.** 0-RTT data is replayable at the application level and must be
  treated that way. With stock iroh 1.4.0 it is accepted only within one
  Active period.

## Restart state

An iroh endpoint that restarts with the same key is the same endpoint, but on
stock iroh 1.4.0 it cannot act like it. Two effects are visible in iroh-do:

- 0-RTT is rejected after every Active → Warm cycle, because the TLS session
  store is created per bind.
- After an abrupt eviction, the new instance drops packets for the old
  instance's connections without sending a stateless reset, because the reset
  key and the connection-id validation key are random per bind. The peer
  waits for its idle timeout.

[`upstream/`](../upstream/README.md) contains a patch to iroh 1.4.0 (4 files,
+149/−5, no noq changes) that adds two builder methods,
`stateless_reset_key` and `tls_server_session_store`, with tests showing a
reset in about one round trip and 0-RTT accepted after a restart. iroh-do does
not use it; it is a proposal for iroh.

## Open questions

- **Pong latency from hibernation.** After the first Pong, the client's
  deadline for later Pongs can be as short as 500 ms (verification V6). How
  long a hibernated object takes to answer has not been measured in
  production.
- **pkarr publish rate.** n0's pkarr relay limits publishes per source IP, and
  Workers send from shared addresses. Republishing many endpoints has not been
  measured.
- **Transport id.** `0x494E4752` is provisional; custom transport ids are
  allocated in iroh's `TRANSPORTS.md`.
- **Hibernating with idle connections.** A room would cost the same as a
  WebSocket chat if members could keep QUIC connections open while the object
  hibernates. That needs noq to park a connection with nothing in flight, so
  that no timers remain, and peers whose keep-alive interval does not wake the
  object every 5 s.
- **Reaching idle peers.** Dial-back to a peer that is not using the room as
  its home relay works only within the client's 60 s window. Reaching such a
  peer later requires an outbound relay connection. An iroh relay mode in which the
  endpoint advertises a relay URL it serves itself, and connects to other
  relays only when dialing, would let iroh's relay logic see the real
  topology ([`spec.md`](spec.md) §8, item 3).
