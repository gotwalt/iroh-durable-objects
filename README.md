# iroh-do

iroh-do runs an [iroh](https://iroh.computer) 1.x endpoint inside a Cloudflare
Durable Object. Each endpoint is one object, named by its EndpointId. Stock
iroh peers reach it with default settings: they look up its EndpointId, find a
relay URL, and connect. The object serves that relay URL itself, runs QUIC
end to end with the peer, keeps its state in its own SQLite database, and
hibernates when nobody is talking to it.

## Install the chat client

macOS builds of `iroh-do-chat` (universal, signed and notarized) are on the
[releases page](https://github.com/gotwalt/iroh-durable-objects/releases):

```sh
curl -LO https://github.com/gotwalt/iroh-durable-objects/releases/latest/download/iroh-do-chat-<version>-macos-universal.zip
unzip iroh-do-chat-<version>-macos-universal.zip
mv iroh-do-chat-<version>-macos-universal/iroh-do-chat /usr/local/bin/
```

Then join a room with `iroh-do-chat <E> --nick <name>`. To build it from
source instead: `cargo build --release -p iroh-do-demo` produces
`target/release/iroh-do-chat`.

## The problem

An iroh endpoint that other people dial needs to be reachable at any time and
usually needs state that outlives a connection. Today that means a process on a
machine that is always on, paid for whether or not anyone connects. Serverless
platforms remove the idle cost but not the networking problem: Cloudflare
Workers have no UDP, so iroh's IP transports are unavailable, and a Worker
holding an outbound relay connection stays in memory and is billed for it.

## Why Durable Objects fit iroh

The two systems make the same assumptions.

- **Identity is the key.** iroh addresses endpoints by public key. A Durable
  Object is addressed by name. Naming the object after the EndpointId gives
  exactly one live instance per endpoint, with no coordination.
- **Connections are ephemeral, identity is durable.** iroh peers redial on
  demand. Durable Objects are evicted from memory when idle and recreated on
  the next event. iroh-do never keeps a QUIC connection across an eviction; it
  rebuilds the `Endpoint` from the stored key when the next packet arrives.
- **The relay protocol runs over inbound WebSockets.** A peer reaches an
  endpoint through the endpoint's home relay, over a WebSocket the peer opens.
  A Durable Object can hibernate while holding inbound WebSockets. So the
  object serves the relay protocol for its own EndpointId, and a peer's relay
  socket stays open while the object is hibernated.
- **State lives with the endpoint.** Each object has a private, strongly
  consistent SQLite database and alarms. An endpoint's data survives
  hibernation, eviction and redeploys, and an alarm can wake it to do work.
- **Nothing to operate.** An object is created when its endpoint is minted and
  placed by Cloudflare. There are no servers to size or keep running.

Peers need no changes. They run stock iroh with default features and default
discovery.

## What the example shows

`just demo` runs a scripted session against a deployed endpoint and prints a
table of checks. A production run, with a stock iroh 1.4.0 peer using default
discovery, passed all 26 checks:

- First contact, including object start, `Endpoint` build and QUIC handshake:
  708 ms.
- The object closed the idle connection after 10 s, dropped its `Endpoint` and
  hibernated. A wake counter in SQLite shows the object was recreated before
  the next session.
- Reconnect after hibernation: 230 ms.
- Chat lines written before hibernation were still in the object's SQLite log
  afterwards.
- The peer asked for a callback in 20 s and went idle. The object hibernated,
  its alarm woke it, and it dialed the peer over the peer's open relay socket.

The same run passes against local workerd, with no Cloudflare account.

Each endpoint is also a small chat room. `iroh-do-chat <E>` opens a
terminal client: scrollback from the object's SQLite, live messages from the
other members, `/me`, `/nick`, `/who`. Members do not hold QUIC connections
between messages, so a room hibernates as soon as a conversation pauses, and
the room delivers new messages by dialing each member over that member's relay
socket. In production, `just demo --room` showed a quiet member staying
present while the room hibernated, then messages delivered in both directions
by dial-back in about 100 ms. See [The chat room](docs/design.md#the-chat-room).

## Trust model

- **Cloudflare is in the endpoint's trusted computing base.** The endpoint's
  secret key is generated inside the Worker and stored in the object's SQLite
  database. Cloudflare can read it, as the owner of any machine running an
  iroh endpoint can.
- **Keys never leave Cloudflare.** OpenTofu, its state, the CLI and the logs
  never see a secret key. Minting returns only the EndpointId. There is no key
  export.
- **No third party in the data path.** The relay at `https://<E>.<zone>/` is
  the endpoint itself. It accepts datagrams addressed to its own EndpointId
  only and has no code path that forwards to anyone else. QUIC and TLS run end
  to end between the peer and the endpoint's key.
- **Destroying the deployment destroys the identities.** Deleting the Worker
  script deletes every object and every stored key. `just down` asks for typed
  confirmation.

## Limitations

- **Relay only.** Workers have no UDP, so there are no direct paths and no
  hole punching. All traffic passes through Cloudflare.
- **An open connection keeps the object in memory and billed.** iroh 1.4.0
  sends QUIC keep-alives every 5 s that its public API cannot turn off, so
  QUIC's idle timeout never fires. The object closes connections itself: after
  10 s with no open streams, or after 60 s with no application data.
- **The object can only dial peers that are connected to it.** A peer's relay
  socket stays open for 60 s after the peer last sent anything. Dialing a peer
  outside that window needs an outbound relay connection, which iroh-do does
  not implement. The chat client avoids this by making the room its home
  relay, which also means it can reach other endpoints only through the room.
- **The chat room is for a few people.** Because members cannot hold QUIC
  connections while the room hibernates, every delivery to a member is a
  dial-back, and every running client pings the room every 15 s. That is cheap
  for a handful of quiet members and wrong for a busy channel. The fix belongs
  in iroh: an idle connection that can be parked without timers.
- **Stock iroh 1.4.0 loses two things on restart.** 0-RTT is rejected after
  every hibernation, and after an abrupt eviction a peer's old connection hangs
  until its idle timeout instead of being reset. [`upstream/`](upstream/README.md)
  holds a proposed iroh patch that fixes both. iroh-do does not use it.

## Try it

You need `just`, OpenTofu 1.13.1, the Rust toolchain from
`rust-toolchain.toml`, `worker-build` 0.8.7
(`cargo install worker-build --version 0.8.7 --locked`), Node, `openssl` and
`curl`. On macOS, `brew install llvm`: the wasm32 build needs `llvm-ar`.

Locally, in workerd, with no account:

```sh
just local                       # build, start wrangler dev, mint, run the demo
LOCAL_PORT=8799 just local       # if port 8787 is taken
```

To chat locally, keep a server running and join its room from several
terminals. Each nick is a separate member with its own key:

```sh
LOCAL_PORT=8799 just local-up    # start wrangler dev in the background; prints E
LOCAL=1 just chat <E> ana        # one terminal per member
LOCAL=1 just chat <E> bo
just local-down                  # stop it; the room stays in .demo/wrangler-state
```

`just local` reuses a server that `just local-up` started and mints its own
endpoint, so the room is not touched.

On Cloudflare, with a zone that has Advanced Certificate Manager and an API
token with the permissions listed in [`infra/README.md`](infra/README.md):

```sh
cp infra/examples/terraform.tfvars.example infra/mine.auto.tfvars   # account and zone ids
export CLOUDFLARE_API_TOKEN=...
just up      # build and apply; OpenTofu owns every resource
just mint friends   # create an endpoint labelled "friends"; prints E and the join command
just demo           # the scripted session above
just plan    # expect no changes
just down    # delete everything, identities included
```

Join the room, and give `E` to whoever you want to chat with:

```sh
just chat <E>                               # nick = $USER
just chat <E> ana                           # as ana: a separate member, its own key
iroh-do-chat <E> --nick ana                 # anyone else, with only E
```

Minted endpoints are listed at `https://admin.<zone>/`. `just cli <command>`
runs the demo peer against `.demo/endpoint`. Other commands: `ping`, `ctl`
(diagnostics: `/status`, `/count`, `/log`, `/callback N`), `log`, `listen`,
`bench --mb N`, `lifecycle`.

## How it works

[`docs/design.md`](docs/design.md) describes the design as built: addressing,
the relay ingress, the custom transport, the object lifecycle, persistence,
dial-back, the chat room, the endpoint list, discovery and failure handling. [`docs/README.md`](docs/README.md)
indexes the rest of the documentation.

## Layout

```
crates/iroh-relay-ingress/      server side of the iroh relay protocol, sans-io
crates/iroh-ingress-transport/  iroh custom transport whose paths are peers' relay sockets
crates/iroh-do-core/            object lifecycle, Endpoint build and teardown, demo protocols
crates/iroh-do/                 the Worker (script `iroh-do-chat`): router, `room` and `registry` Durable Objects (wasm32)
crates/iroh-do-demo/            the iroh-do-chat client and diagnostics CLI (stock iroh)
harness/native/                 tokio host for the same code, with tests against stock peers
harness/workerd/                local workerd config and Miniflare eviction tests
infra/                          OpenTofu module for the Cloudflare deployment
docs/                           design, decision log, verification, measurements
upstream/                       proposed iroh patch, reference only
```

## License

MIT; see [`LICENSE`](LICENSE). `upstream/iroh/` is a copy of iroh and keeps
iroh's own license, MIT OR Apache-2.0.
