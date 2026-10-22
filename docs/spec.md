# iroh endpoints on Durable Objects: the relay-ingress design

**Status:** implementation plan, pre-spike
**Target:** iroh 1.x (wire-stable since 1.0, June 2026), Cloudflare Durable Objects (production) and workerd (self-hosted)
**Audience:** an expert implementing agent, and later the n0/iroh maintainers as reviewers
**Author's intent:** every load-bearing decision is argued here so the implementer executes and verifies instead of re-deriving. Items marked **[verified]** were confirmed against primary sources on 2026-10-21. Items marked **[verify: Vn]** are assumptions that block specific phases until checked (see §10).

---

## 0. Thesis

A Durable Object can be a first-class iroh endpoint, reachable by **unmodified** iroh 1.x peers, with near-zero idle cost. Two moves make this possible:

1. **Invert the relay direction.** The DO never dials a relay. It *serves* the iroh relay protocol at a URL derived from its own EndpointId and advertises that URL as its home relay. Peers reach it with their stock relay client, over an **inbound** WebSocket, which Durable Objects can hibernate. The QUIC session stays end-to-end between the peer and the DO's key. The "relay" and the endpoint are the same principal, so no new party enters the trust model.
2. **Treat eviction as an endpoint restart, never as a pause.** QUIC connection state is never serialized, frozen or thawed. When the DO has no live QUIC connections it drops its `Endpoint` entirely and may hibernate. On the next inbound packet it rebuilds the endpoint from persisted identity and resumption state, and the peer re-handshakes, normally with 0-RTT. Persisted stateless-reset and session-store keys make abrupt evictions fail fast and safely.

This removes the timing problem at its root. QUIC's clock sensitivity only bites if one tries to suspend a live connection; this design has no live connection across any suspension point.

---

## 1. Design principles (the review rubric)

These are the standards every change is judged against. They are drawn from iroh's own stated model; deviations must be argued in the PR.

- **P1. Identity is the key.** Everything addressable is derived from the EndpointId. The relay URL is a function of the key. No IPs, no out-of-band names that can drift from the key.
- **P2. Connections are ephemeral; identity is durable.** Persist only what lets a *new* connection start fast and safely (identity key, resumption state, reset key). Never persist connection state.
- **P3. No wire changes.** Peers run stock iroh 1.x with default features. If interop requires a peer-side change, the design is wrong.
- **P4. Relays stay dumb and untrusted.** The ingress forwards nothing to third parties. It accepts datagrams addressed to its own EndpointId only. It is not a general relay, and must not become an open relay by accident.
- **P5. Sans-io first.** The relay ingress (handshake, framing, session) is a pure state machine with explicit `now` and explicit I/O, testable natively with no Cloudflare dependency. The iroh endpoint and the node's lifecycle read the platform clock; `design.md` § Time states why that clock is sufficient. Runtime glue is thin. *(Narrowed by design note 013.)*
- **P6. No gratuitous abstraction.** Do not add a clock abstraction, transport trait or config knob unless a measurement or a second consumer demands it. Reviewers will reject speculative layers.
- **P7. Minimal, principled upstream asks.** Phase 1 ships with zero iroh changes, using public (if unstable) APIs. Upstream proposals come after, each justified by a concept iroh already has, never by "Cloudflare needs it".
- **P8. Honest trust model.** The host runtime (Cloudflare, or whoever runs workerd) is inside the endpoint's TCB, exactly as any machine running an iroh endpoint is. State this; do not obscure it.

---

## 2. Environment facts

### 2.1 Cloudflare / workerd [verified]

- **No UDP.** Workers have outbound TCP (`connect()`) and, since August 2026, a `connect()` *handler* for TCP forwarded over service bindings. Direct inbound TCP is unsupported; Cloudflare says UDP-based protocols are next. Treat UDP as unavailable.
- **Outbound connections pin the DO.** Hibernation requires no active outbound TCP socket or outbound WebSocket. Each outbound connection keeps the DO resident for at most 15 minutes, billed as duration.
- **Inbound WebSockets survive hibernation** when accepted via `ctx.acceptWebSocket()`. Evicting an idle DO into hibernation does not close them. A message on such a socket re-instantiates the DO and invokes `webSocketMessage`.
- **Hibernation also requires no pending timers** (`setTimeout`/`setInterval`), no in-flight `fetch`, and no use of the standard (non-hibernatable) WebSocket API.
- **Frozen clock.** On Cloudflare, `Date.now()` and `performance.now()` advance only after I/O; `performance.timeOrigin` is 0, so `performance.now() == Date.now()`. Time is monotonic and advances at every event boundary.
- **Auto-response is text-only.** `setWebSocketAutoResponse` takes strings only, so binary keepalives wake the DO (workerd#6062).
- **DOs can shut down at any time** (deploys, placement, runtime decisions). Design for abrupt eviction as a normal event.
- **workerd** self-hosts DOs; cluster mode with filesystem service discovery and OFD-lock ownership is landing (workerd#6780). workerd also has a "pin in memory forever" option for DO namespaces, unavailable in production.

### 2.2 iroh 1.x [verified unless marked]

- **Browser/wasm build is relay-only** (no UDP from the sandbox). It compiles for `wasm32-unknown-unknown` via wasm-bindgen with default features off, and is checked in node.js regularly. workerd is a V8 isolate with the same constraints, so this is the starting point.
- **Custom transports** (since 0.97, `unstable-custom-transports`): any unreliable datagram transport with a ≥1200-byte MTU can carry iroh's QUIC datagrams. Traits: `CustomTransport`, `CustomEndpoint`, `CustomSender`, plus `PathSelector` for preference. Transports are identified by a `u16` id registered in `TRANSPORTS.md`. The API is explicitly unstable and will stay so past 1.0, so pin exact versions.
- **noq** is iroh's QUIC (a Quinn fork, sans-io core in `noq-proto`). Like Quinn, the proto layer is driven by explicit `now` arguments and `poll_timeout()`, and supports 0-RTT/0.5-RTT.
- **0-RTT** is opt-in at the application layer (`into_0rtt`), with fallback to 1-RTT when there's no ticket or the server rejects.
- **Relay handshake (since 0.91):** split into a handshake protocol and a messaging protocol. The client proves key ownership either by signing a `ServerChallenge` sent after the WebSocket opens, or immediately via an HTTP header (`x-iroh-relay-client-auth-v1`) whose signature covers **TLS exporter keying material**. Domain separation strings are `"iroh-relay handshake v1 challenge signature"` and `"iroh-relay handshake v1"`. Frames are binary WebSocket messages beginning with a QUIC-varint frame type. Protocol versions are negotiated, and some frames (Health vs Status) are version-gated.
- **Relay admission tokens (2026):** `Authorization: Bearer <token>` on the upgrade, a signed capability bound to the presenting endpoint's public key.
- **Embeddable relay server:** `RelayService` is public since 0.97. It is hyper/tokio-based and almost certainly unusable inside workerd **[verify: V7]**, but its protocol modules are the reference implementation to conform to.

### 2.3 Prior art

HeddleCo's `weft/crates/heddle-iroh-object-provider` is a Cloudflare DO iroh endpoint. Its portable half is a WebSocket transport written against `CustomTransport`/`CustomSender`/`PathSelector`, plus a host-driven `QuicTimeSource` clock (`observe_host_time`/`next_timer`/`fire_timer`). Extraction is tracked as HeddleCo/weft#1274. As far as the public record shows, it uses a bespoke relay (weft) rather than stock-peer interop via the iroh relay protocol. **Read it before Phase 2; check its license before reusing any code; cite it in upstream discussions.** The present design differs in three ways: stock-peer interop (P3), hibernation as a designed state rather than a side effect, and the persistence model in §5.6.

---

## 3. Rejected approaches (and why, for reviewers)

| Approach | Why rejected |
|---|---|
| **Serialize/restore QUIC connection state across hibernation** | QUIC state is bound to wall time: loss-detection and PTO timers, idle timeout, congestion state, key-update phase. The peer's timers keep running while ours are frozen. A restored connection is a lie both sides would detect, as spurious loss, PTO storms, or idle-timeout races. noq exposes no serialization, rightly. Violates P2. |
| **Manipulating noq's `Instant` to "skip" suspended time** | The issue this addresses only exists if live connections span a suspension. With no live connections across suspensions, there's nothing to correct. Any such hook would be a footgun in the public API. |
| **Stock iroh endpoint in the DO dialing n0 relays** | Works, but the outbound WebSocket pins the DO (≤15 min per connection, continuously re-established). Idle cost equals resident cost. Defeats the purpose. |
| **iroh native inside a Cloudflare Container per DO** | Containers get no inbound UDP either, cost far more than a DO, and their lifecycle is coarser. They relocate the problem without solving it. |
| **General-purpose iroh relay on Workers** | A different product (relay hosting), with abuse surface and P4 implications. Out of scope. The ingress here serves exactly one endpoint. |
| **Path-routed shared relay URL** (one hostname, route to DO by inspecting traffic) | Routing must happen at the WebSocket upgrade, before any datagram reveals its destination. The client's handshake reveals only the *client's* id. Routing therefore has to be by URL, so the URL must encode the destination. See §5.1. |

---

## 4. Architecture overview

```
 stock iroh 1.x peer                        Cloudflare
 ───────────────────                        ───────────────────────────────────────────────
 Endpoint                                   Worker (router, stateless)
  ├─ address lookup (pkarr/DNS) ──────────►  pkarr record for E: relay = https://<E>.<zone>/
  └─ relay client ── wss://<E>.<zone>/relay ─►  host label → idFromName(E) → DO stub.fetch()
                                                     │
                                            Durable Object "E" (Rust → wasm, workers-rs)
                                             ├─ Ingress (sans-io): handshake, frames, dst filter
                                             ├─ hibernatable WebSockets, one per authed peer
                                             │    attachment = {peer_id, proto_version, ...}
                                             ├─ Lifecycle: Cold | Warm | Active
                                             ├─ Endpoint (only in Active)
                                             │    └─ custom transport "ingress": paths = peer ids
                                             ├─ Router + ALPN handlers (application)
                                             └─ SQLite: identity, TLS session store, keys, config
```

`<E>` is the DO's EndpointId in iroh's canonical z-base-32 text form (52 characters, fits a 63-octet DNS label). `<zone>` is an operator-owned zone with wildcard routing to the router Worker.

---

## 5. Detailed design

### 5.1 Addressing and routing

- **Relay URL:** `https://<E>.<zone>/`. The relay client appends its standard path (`/relay`). Using a hostname rather than a path segment avoids depending on how `RelayUrl` joins paths **[verify: V1]**. If V1 shows paths are preserved, a path form (`https://<zone>/e/<E>/`) is an acceptable alternative for zones without wildcard certificates.
- **Router Worker:** parse the leftmost label, validate it as an EndpointId (reject otherwise, cheaply, before touching any DO), then `env.ENDPOINTS.idFromName(label)` and forward the upgrade. The router holds no state and performs no auth beyond syntax and optional IP rate limiting.
- **Certificates:** a wildcard edge certificate for `*.<zone>` **[verify: V15]**. Prefer a dedicated zone or subdomain so that a single wildcard level suffices.
- **Consequence:** the relay URL is a pure function of the key (P1). Peers can resolve the address via pkarr (zero peer changes) or, optionally, via a trivial static address lookup `E ↦ https://<E>.<zone>/` (one line of peer config, no network round trip).

### 5.2 Ingress: server side of the relay protocol (sans-io core)

New crate, provisional name **`iroh-relay-ingress`**: runtime-agnostic, `no tokio`, `no hyper`. Its API is a state machine:

```rust
pub struct IngressSession { /* per WebSocket */ }

impl IngressSession {
    /// Called with the HTTP upgrade request headers. Returns the response
    /// headers to send (subprotocol selection) and an optional initial frame
    /// (ServerChallenge) to write once the socket is open.
    pub fn on_upgrade(req: &UpgradeRequest, cfg: &IngressConfig, rng: &mut impl CryptoRngCore)
        -> Result<(UpgradeResponse, Self), Reject>;

    /// Feed one binary WebSocket message. Emits zero or more outputs.
    pub fn on_message(&mut self, msg: &[u8], now: Instant) -> Result<Outputs, Close>;

    /// Emit frames addressed to the authenticated peer.
    pub fn encode_datagrams(&self, batch: DatagramBatch<'_>) -> Frame;

    /// Persistable summary for hibernation (small; < 2 KiB WebSocket attachment limit).
    pub fn snapshot(&self) -> SessionSnapshot;
    pub fn restore(s: SessionSnapshot) -> Self;
}

pub enum Output<'a> {
    Send(Frame),                                  // write to this socket
    Authenticated { peer: EndpointId },
    Datagrams { from: EndpointId, batch: DatagramBatch<'a> }, // dst already checked == self
    Ping(Frame),                                  // reply already encoded as Pong
}
```

Behavior requirements:

1. **Version negotiation:** select the highest mutually supported version from the client's offer, using whatever header carries it **[verify: V2]**. Unknown or no overlap means reject at upgrade. Never "best effort" an unknown version.
2. **Authentication:** the DO cannot verify the TLS-exporter header form, because TLS terminates at Cloudflare's edge and exporter material is unavailable to the DO. The ingress therefore **always uses the challenge path** and ignores the exporter header **[verify: V3: the client must accept a ServerChallenge after having sent the header]**. The challenge is 16+ bytes from a CSPRNG (`crypto.getRandomValues` via `getrandom`'s js backend). Verify with iroh-base's ed25519 using the exact domain separation. Conformance is non-negotiable here: reuse `iroh-relay`'s codec types if their visibility permits; otherwise mirror them and add byte-level golden tests generated from the upstream crate.
3. **Admission (optional):** if configured, validate an `Authorization: Bearer` token using iroh's relay-token semantics (issuer, audience = presenting endpoint key, expiry, grant). Same header, same meaning (P1, P3).
4. **Destination filter (P4):** after auth, accept only datagram frames whose destination equals the DO's own EndpointId. For any other destination, respond with the protocol's "peer gone / not here" signal if one exists **[verify: V4]**; otherwise drop silently. Count and rate-limit violations per peer, and close after a threshold.
5. **Batching and ECN:** preserve whatever batching (segment size) and ECN fields the 1.x datagram frame carries, end to end into the transport **[verify: V4]**.
6. **Keepalive:** answer client Ping with Pong (echoing payload). Do **not** originate server pings while Warm (each would require waking). Determine whether the client times out a silent server **[verify: V6]**; if it does, the Warm-state duration bound in §5.5 must sit below that timeout, or the ingress must close the socket deliberately before it.
7. **Restarting / Status frames:** use the version-appropriate frame when the DO intends to close the socket (Warm→Cold), so the client treats it as expected rather than as a fault **[verify: V4, V5]**.
8. **One socket per peer:** a second authenticated socket from the same EndpointId supersedes the first. Close the old one with a clear code. This keeps the path mapping a function.

Native test harness (required before any Cloudflare work): a tokio WebSocket server wrapping `IngressSession`, with a stock iroh 1.x client configured with the harness URL as the target's home relay. Conformance is proven natively; workerd becomes a deployment detail.

### 5.3 Transport binding

The DO-side `Endpoint` runs with **no relay transport and no IP transports** (`RelayMode::Disabled` on wasm, **[verify: V7]**) and one custom transport, **`ingress`**:

- **Transport id:** request an id in `TRANSPORTS.md` from n0, using an experimental-range id until assigned **[verify: V13]**.
- **CustomAddr payload:** the peer's 32-byte EndpointId. One path per peer. The DO never publishes this address. It is purely local path identity, which is why it needs no global meaning.
- **recv:** `Output::Datagrams { from, batch }` → transport receive queue with source `CustomAddr(from)`.
- **send(dst = CustomAddr(peer)):** look up the live socket for `peer` and write `encode_datagrams`. If none exists, return the transport's "no route" error. **Never dial out from the ingress transport.**
- **PathSelector:** trivial. Only one transport exists in Phase 1.
- **Semantic note for reviewers:** on the wire and from the peer's perspective, this is an ordinary relay path `(RelayUrl(<E>), E)`. Only the DO-side representation is "custom". §8 proposes making the DO side a relay path too, so iroh's relay-specific logic (holepunch initiation, path policy) sees the truth. Phase 1 uses a custom transport because it requires no upstream change (P7).
- **Must verify:** an endpoint whose *only* transport is custom accepts incoming connections, and its NAT-traversal and holepunch logic behaves sanely, sending candidates harmlessly or not at all, with no relay transport present **[verify: V14]**.

### 5.4 Time model

Decision: **use the stock wasm time source (`web_time::Instant` → `performance.now()`) unmodified in Phase 1.**

Argument:
- noq's proto layer is sans-io and already expects `now` sampled at event boundaries. Workers' frozen clock is exactly "`now` sampled at the last I/O". Within a single event (one WebSocket message, one timer callback), every `Instant::now()` returns the same value. This is how a correct sans-io driver behaves anyway.
- Time advances at every packet arrival and every timer firing, which are the only moments QUIC's recovery logic consults the clock. RTT samples therefore measure arrival-to-arrival correctly, at I/O granularity.
- The only error is CPU time spent within one event, which is invisible to `now`. Handshake crypto (ed25519, x25519, AEAD on ≤ a few KB) is sub-millisecond; deadlines computed slightly early are harmless.
- Timers: noq's wasm runtime schedules via `setTimeout` (through n0-future / wasm-bindgen-futures) **[verify: V7]**. Pending timers block hibernation, which is **correct** while Active and is handled by dropping the Endpoint before Warm (§5.5).

A host-driven clock (Heddle's approach) is admitted **only if** measurement in Phase 6 shows a defect attributable to the time source: e.g. RTT estimates or PTO behavior diverging materially from a native baseline under identical network conditions. If added, it must be a noq/iroh-level injection point with a second consumer (deterministic simulation testing is the obvious one), never a DO-specific hack (P6).

workerd self-hosted: confirm whether the clock is frozen there too **[verify: V12]**. Either behavior is fine under this argument. Test both.

### 5.5 Lifecycle state machine

```
             inbound upgrade (fetch)                     first datagram
   ┌──────┐ ───────────────────────► ┌──────┐ ─────────────────────────► ┌────────┐
   │ Cold │                          │ Warm │                            │ Active │
   └──────┘ ◄─────────────────────── └──────┘ ◄───────────────────────── └────────┘
        all sockets closed / evicted      no QUIC connections for T_linger,
        (T_warm elapsed, budget hit,      Endpoint closed, zero timers/tasks
         or platform eviction)
```

**Cold:** no isolate, no sockets. Entered by platform eviction or by closing the last socket. Exits on an inbound upgrade (a peer's relay client connecting) or an alarm (pkarr republish, key rotation).

**Warm:** ≥1 accepted hibernatable socket with an authenticated peer (restored from attachments via `ctx.getWebSockets()`), **no `Endpoint` instance, no timers, no tasks**. The isolate may be hibernated. Invariant, tested: *the DO is hibernatable in Warm.*

**Active:** `Endpoint` instantiated; ≥1 QUIC connection or handshake in progress, or within `T_linger` of the last close. Timers and tasks permitted.

Transitions:

- **Warm → Active** on the first datagram. Instantiate the Endpoint (load identity, install persisted stores, §5.6), then inject the datagram. Instantiation is async, and the DO input gate opens on non-storage awaits, so further messages can arrive meanwhile. They go into a **bounded FIFO pre-activation queue**, drained in order once the Endpoint is ready, with drops beyond the bound (QUIC tolerates loss; unbounded buffering is a DoS vector).
- **Active → Warm** when the connection count hits 0 and stays 0 for `T_linger` (default ~2 s, to absorb immediate redials). Call `endpoint.close().await`, drop it, then **assert** no outstanding timers or tasks (a debug-build check via a task/timer counter in the glue; a test in CI).
- **Active idle policy** uses QUIC's own mechanism: the DO advertises `max_idle_timeout` (default 30 s, configurable) and **disables its own keepalive**. If a peer enables keepalive, it pays to keep the session open; that's the peer's choice and the operator's cost. An optional operator cap (max connection age without open streams) exists as a policy knob, off by default. Policy, not mechanism.
- **Warm → Cold** after `T_warm` without traffic (default 10 min) or when a per-socket wake budget is exhausted (client pings cost one wake each; see V6). Close with the version-appropriate Restarting/Status semantics so the client reconnects **lazily**, only when it next has data **[verify: V5: if the client reconnects eagerly to a non-home relay it learned from discovery, this design causes wake storms, and the default must change to never close, or the client behavior must be fixed upstream]**.

Why keep Warm at all, given that Cold→Active is possible directly (an inbound upgrade wakes the DO)? Warm saves the relay handshake round trip on reconnect and keeps the peer's relay path "up" from its perspective. If V5/V6 make Warm expensive, `T_warm = 0` is a valid configuration, and the design degrades gracefully to Cold↔Active.

### 5.6 Persistent state (DO SQLite)

| Item | Why | Lifetime | Notes |
|---|---|---|---|
| Endpoint secret key | Identity (P1) | Forever | Random per object by default. Alternative: HKDF from a namespace master secret + object name. Stateless, but master compromise compromises all. Default is per-object random, matching iroh's model where each endpoint owns its key. |
| TLS server session store | 0-RTT/resumption after restart | Ticket lifetime (≤ 7 days per TLS 1.3; configure shorter, e.g. 24 h) | Implement rustls `StoresServerSessions` over SQLite. Its single-use `take()` is what rustls uses for 0-RTT anti-replay. Backed by a DO, which is a single global instance, this gives **correct replay protection across restarts**, which most servers can't offer. Requires a hook in iroh's TLS config **[verify: V9; likely an upstream ask, §8]**. |
| Stateless reset key | Abrupt eviction fails fast | Forever (rotate with identity) | Derive with HKDF from the secret key under a dedicated domain separation string; no storage needed. Acceptable because compromise of the identity key already compromises everything the reset key protects. Must be injectable into noq through iroh **[verify: V8]**. |
| Address-validation token key | Avoid Retry round trip after restart | Rotating | iroh 1.0 has a global token store (#4317) **[verify: V10]**. If not configurable, accept the extra RTT on first contact after restart; it is a perf cost, not a correctness one. |
| WebSocket attachments | Restore sessions after hibernation | Socket lifetime | `{peer_id, proto_version, authed_at, wake_budget}`; must be ≤ 2 KiB. |
| Config | Policy knobs | Forever | `max_idle_timeout`, `T_linger`, `T_warm`, admission keys, rate limits. |

**Deliberately not persisted:** QUIC connection state, CIDs, congestion/RTT state, application stream state. On restart, new CIDs are random. A peer that still holds an old connection sends short-header packets with unknown CIDs, and the new endpoint answers with a stateless reset under the persisted key. The peer fails the old connection in ~1 RTT instead of waiting out its idle timeout, then redials (0-RTT if it has a ticket).

**Storage discipline on the hot path:** use the synchronous SQLite API (`sql.exec`) only; never `await` storage in packet paths, which would close the input gate and add latency. Writes delay outgoing messages until durable (output gate). Session-ticket writes happen once per handshake, which is acceptable. Never write per packet.

### 5.7 Concurrency inside the DO

- Single-threaded event delivery. All ingress sessions and the Endpoint share one executor (`wasm-bindgen-futures::spawn_local`).
- Event handlers (`webSocketMessage`, `webSocketClose`, `alarm`, `fetch`) do minimal synchronous work and push into the sans-io core. The Endpoint's own tasks run between events.
- Panics: build with `panic = "abort"`. A panic resets the DO. Correctness then reduces to the abrupt-eviction path (stateless reset + redial), which is already designed for. Add a panic counter to observability.

### 5.8 Failure semantics

| Event | Peer observes | Recovery | Cost |
|---|---|---|---|
| Graceful idle (QUIC idle timeout) | Connection closed | Redial on demand; 0-RTT | none |
| Active→Warm drain | Already closed | none needed | none |
| Platform eviction mid-connection (deploy, migration, panic) | Stateless reset on next packet (≤1 RTT) | Redial; 0-RTT if ticket valid, else 1-RTT | one handshake |
| Socket closed by DO (Warm→Cold) | Relay connection to `<E>` closed with restart semantics | Lazy reconnect on next send **[V5]** | relay handshake + DO wake |
| Client relay ping while Warm | Pong | n/a | one wake per ping **[V6]** |
| Storage unavailable during activation | Upgrade rejected / packets dropped | Client retries per its backoff | n/a |
| Second socket from same peer | Old socket closed | Path re-binds to new socket | none |

### 5.9 Discovery

- The DO publishes a signed pkarr record for E with `relay = https://<E>.<zone>/` and no direct addresses. It republishes from a DO **alarm** at the TTL cadence the chosen publisher requires. Alarms wake the DO briefly and are compatible with hibernation.
- Publisher: n0's DNS/pkarr infrastructure via outbound `fetch` (short-lived, does not meaningfully pin), or a self-hosted pkarr relay Worker (n0 publishes one, `iroh-discovery-cloudflare-worker`). Signing must use iroh's own pkarr record construction so the format matches exactly **[verify: V16]**.
- Optional static lookup for peers that prefer it: `E ↦ https://<E>.<zone>/`, implementable as a tiny address-lookup in the peer. Document it; don't require it (P3).

### 5.10 Security analysis

- **Authentication:** the relay handshake proves the peer holds its key (challenge signature with domain separation). QUIC/TLS then authenticates both ends by raw public key, end to end with the DO's identity.
- **Open-relay risk:** eliminated by the destination filter (§5.2.4). The ingress has no forwarding code path at all, which is stronger than a policy check.
- **Amplification:** the ingress only writes to sockets whose peer has authenticated; QUIC's anti-amplification limits still apply above it.
- **Wake-amplification DoS:** each unauthenticated upgrade wakes the DO. Mitigate at the router (syntax check, IP rate limits, optional admission token checked *before* forwarding when the token format allows stateless verification) and in the DO (per-peer budgets, handshake timeout, bounded queues).
- **0-RTT replay:** single-use tickets via the SQLite session store. Applications must still treat 0-RTT data as replayable at the semantic level, as iroh documents; the ALPN handler API surfaces whether a stream arrived in 0-RTT.
- **Key custody:** identity keys at rest in DO storage. Cloudflare (or the workerd operator) is in the TCB (P8). Document this in the README's first screen.
- **Resumption forward secrecy:** session ticket keys are random and rotated via alarm, never derived from the identity key, so identity-key compromise does not retroactively expose resumption secrets.

### 5.11 Cost model (to be measured, not asserted)

Measure in Phase 6 and publish: WebSocket messages per MB transferred (as a function of relay datagram batching); wakes per idle hour per peer under default client ping behavior; Cold→first-byte latency (wasm instantiation + relay handshake + QUIC handshake, broken out); resident duration per session. Cloudflare bills inbound WebSocket messages at a ratio to requests; take the exact ratio from current pricing docs at measurement time.

---

## 6. Phased plan with acceptance criteria

**Phase 0: verification spike (blocking; no production code).**
Answer V1–V19 (§10; V17–V19 may be deferred until Phase R starts) with evidence: source links pinned to a commit, or a minimal repro. Output: `docs/verification.md`. Any "no" answer triggers a design note before proceeding. Expected design pivots are listed with each item.

**Phase 1: `iroh-relay-ingress` sans-io core.**
Handshake, framing, destination filter, ping/pong, snapshot/restore.
*Accept:* byte-level golden tests against upstream `iroh-relay` encodings; proptests for frame parsing that mirror upstream's (#4389); a native tokio harness in which a **stock iroh 1.x client** authenticates and exchanges datagrams; malicious-client tests (wrong dst, bad signature, oversize, unknown version, flood).

**Phase 2: `ingress` custom transport + lifecycle, natively.**
Endpoint with ingress transport only; lifecycle state machine with injected events; Endpoint drop/rebuild between sessions; stateless reset after a simulated abrupt restart.
*Accept:* echo ALPN round trip from a stock client; three sequential sessions with Endpoint rebuild in between; 0-RTT accepted on sessions 2+ (using an in-memory session store); an abrupt-restart test showing the client fails the old connection in < 2 RTT and redials; zero tasks/timers after Active→Warm.

**Phase 3: Cloudflare/workerd glue.**
workers-rs DO, hibernatable sockets, attachments, router Worker, wildcard routing, SQLite-backed stores.
*Accept:* the Phase 2 tests pass against `wrangler dev` (workerd) and against a deployed Cloudflare DO; a hibernation test proving Warm is hibernatable (forced eviction in the test harness **[V12]**, plus production duration metrics showing no billing in Warm); wasm bundle size within Workers limits, with the number reported.

**Phase 4: persistence and replay safety.**
SQLite `StoresServerSessions`, ticket-key rotation by alarm, reset-key derivation, token key (if configurable).
*Accept:* 0-RTT accepted after a **real** Cold restart at ≥ 95% when the ticket is valid; a captured 0-RTT flight replayed after restart is rejected; expired and rotated tickets fall back to 1-RTT cleanly. If iroh lacks the hooks (V8/V9), implement them as a **minimal, separately reviewable patch** on a fork pinned to a 1.x tag, opened upstream in parallel (§8).

**Phase 5: discovery.**
Alarm-driven pkarr publish; optional static lookup crate.
*Accept:* a stock client given only `E` (default discovery) reaches the DO; record TTL and republish cadence documented.

**Phase 6: measurement.**
RTT/PTO behavior vs native baseline under netem loss and delay on the client side; the cost metrics in §5.11; a wake-storm test for V5/V6 behaviors.
*Accept:* a published report with methodology and raw data; the time-source decision (§5.4) confirmed or reversed with evidence.

**Phase 7: upstream proposals** (§8), each with a design note, a minimal patch and tests.

**Phase 8 (optional): workerd fork track** (§9).

**Phase R: replicable example (runs alongside Phases 3–6; first usable at the end of Phase 3).**
An OpenTofu-provisioned, one-command deployment of an echo endpoint, plus a native peer CLI, specified in §13.
*Accept:* a reviewer with a Cloudflare account and a zone runs `just up`, then `just demo`, and sees a stock iroh 1.x peer round-trip through a DO that demonstrably hibernates between sessions. A second `tofu apply` shows zero diff. `just down` leaves no Cloudflare resources behind. The same demo runs against local workerd with no Cloudflare account.

---

## 7. Test strategy (cross-cutting)

- **Conformance first:** all protocol behavior is tested natively against stock iroh, before and independently of workerd.
- **Deterministic lifecycle tests:** drive the state machine with an explicit event list and an explicit clock (test-only; this does not contradict §5.4, because the production time source is unchanged).
- **Adversarial suite:** handshake forgery, cross-destination datagrams, slowloris upgrades, socket churn, oversized frames, version downgrade attempts.
- **Interop matrix:** client iroh versions 1.0.x through latest 1.x; client relay protocol versions; Linux/macOS clients; `wrangler dev` vs production.
- **Eviction tests:** forced hibernation, forced restart mid-transfer, deploy during an active session.
- **CI gates:** `cargo deny`, `clippy -D warnings`, `wasm32-unknown-unknown` build, bundle-size budget, no `unsafe` outside reviewed modules, every public item documented.

---

## 8. Upstream proposals (draft RFC shapes)

Each is framed in iroh's existing concepts. None is Cloudflare-specific.

1. **Sans-io relay server core in `iroh-relay`.**
   Expose the server-side handshake and frame codec as a runtime-agnostic state machine. `RelayService` becomes one driver of it. *Justification:* the same sans-io split noq already has; enables embedding a relay in any runtime (workerd, embedded, tests). *Evidence:* `iroh-relay-ingress` from Phase 1 as a working prototype.

2. **Restart-stable endpoint state.**
   A builder hook for server TLS session storage (`StoresServerSessions`), stateless-reset key and address-validation token key. Possibly unified as one `PersistentState` trait with defaults that match today's behavior. *Justification:* "identity is the key; connections are ephemeral" implies that an endpoint restarting with the same key should resume as cheaply and safely as possible. This benefits every server that restarts, not only DOs. *Evidence:* Phase 4 replay and resumption results.

3. **Ingress relay mode.**
   `RelayMode::Ingress(url)`: advertise `url` as home relay, never connect to it (the endpoint *is* it), and connect to remote relays lazily and only when dialing. The DO-side path becomes a true relay path `(url, peer)` instead of a custom-transport path, so iroh's relay-specific logic sees the truth. *Justification:* removes the custom-transport impedance mismatch noted in §5.3 and unlocks DO-initiated dials without keeping a home relay connection pinned. *Evidence:* Phases 2–3, plus V14 findings.

4. **(If V5 shows eager reconnects) lazy reconnect to non-home relays** on server-initiated close with restart semantics. Probably already the behavior; propose only on evidence.

5. **Transport id registration** for `ingress` in `TRANSPORTS.md`. Moot if proposal 3 lands.

Explicitly **not** proposed: any time-source injection (unless Phase 6 forces it), any connection serialization, any wire change.

---

## 9. workerd fork track (self-hosted only)

Ranked by value per unit of fork burden:

1. **Pattern auto-response for binary frames** (generalizes workerd#6062): a responder that recognizes a frame-type prefix and echoes the payload into a templated response. A static request/response pair cannot answer relay Pings, because the ping payload is random and must be echoed. This eliminates Warm-state wakes for client keepalives. Upstreamable to Cloudflare as a general feature (MQTT has the same need).
2. **Hibernatable datagram sockets:** the runtime owns a UDP socket and delivers datagrams as events, like hibernatable WebSockets, with the DO evictable between events. With an iroh IP transport bound to it, this gives **direct paths and holepunching**, while lifecycle semantics stay identical to §5.5 (QUIC state still never survives hibernation). This is the principled primitive and the one to propose to Cloudflare. It would also make this design work on production Cloudflare unchanged once they ship inbound UDP.
3. **Endpoint-relocation sidecar** (only if long-lived sessions across eviction are a hard requirement): a native iroh process holds the UDP socket *and* the QUIC state; the DO talks to it over a local hibernatable socket as an application, consuming streams. Be explicit in docs: the sidecar **is** the endpoint and the TCB; the DO is an application of it. It introduces node affinity, interacting with cluster mode (workerd#6780), so the sidecar must follow DO ownership. It is a different architecture, not an extension of this one.

---

## 10. Verification checklist (Phase 0)

Each item lists the question, where to look, and the design impact of a negative answer.

- **V1.** Does `RelayUrl` preserve path segments when the client appends the relay path? *(iroh-relay client URL construction)* → if not, hostname routing is mandatory (already the default).
- **V2.** How is the relay protocol version negotiated on upgrade (header name, ordering), and what must the response echo? → shapes `on_upgrade`.
- **V3.** If the server cannot verify the TLS-exporter auth header, does a 1.x client correctly complete the `ServerChallenge`/`ClientAuth` path? → if not, this is a **blocker**: propose a client fallback upstream, or find a header form that doesn't bind to TLS exporter material.
- **V4.** Exact 1.x frame set: datagram batch layout (ECN, segment size), Ping/Pong, Health vs Status, Restarting, peer-gone semantics for unknown destinations. → codec and §5.2 items 4–7.
- **V5.** Client behavior for a relay that is the *remote's* home relay (not its own): lazy connect? inactivity cleanup interval? behavior on server close (eager reconnect vs on-demand)? → `T_warm` default; possible upstream item 4.
- **V6.** Client relay ping interval, and whether the client times out a server that never pings. → Warm wake cost; `T_warm` bound.
- **V7.** iroh wasm build with `default-features = false`, `unstable-custom-transports`, and relay disabled: does it compile, does it run in workerd, and are noq timers `setTimeout`-backed and dropped on `Endpoint` close? → if relay cannot be disabled on wasm, an upstream fix is a prerequisite.
- **V8.** Can the stateless reset key be supplied through iroh's builder, and does noq emit resets for unknown CIDs on custom-transport paths? → Phase 4 hook or patch.
- **V9.** Can a custom rustls `StoresServerSessions` be installed, and `max_early_data_size` set, through iroh's TLS configuration? → Phase 4 hook or patch (upstream item 2).
- **V10.** Is the 1.0 address-validation token store (#4317) key configurable or persistable? → perf only.
- **V11.** workers-rs support for `acceptWebSocket` with tags, `serializeAttachment`/`deserializeAttachment`, `getWebSockets`, and setting `Sec-WebSocket-Protocol` on the 101 response. → if missing, the DO shell is a thin JS class calling into wasm.
- **V12.** Is workerd's local clock frozen like production? Can tests force hibernation/eviction (vitest-pool-workers or workerd test hooks)? → test harness design.
- **V13.** `TRANSPORTS.md` id allocation policy and CustomAddr serialization (changed in 1.1, #4465). → transport id choice.
- **V14.** Does an endpoint with only a custom transport accept inbound connections and behave sanely in path selection and NAT-traversal logic? → if not, upstream item 3 moves from "nice" to "required".
- **V15.** Cloudflare wildcard routing and edge certificates for 52-character labels under `<zone>`. → zone layout.
- **V16.** Can iroh's pkarr record construction and signing be used standalone on wasm, publishing over `fetch`? → discovery implementation.
- **V17.** Does the pinned Cloudflare provider (v5.x) for OpenTofu support, in `cloudflare_workers_script`: a multi-module upload (JS shim + `.wasm` module), Durable Object namespace bindings, SQLite-class migrations (`new_sqlite_classes`), compatibility date/flags and secret bindings? Are wildcard Workers routes plus a proxied wildcard DNS record sufficient, and which certificate resource covers `*.<sub>.<zone>`? → if script upload is unsupported, use the fallback in §13.3; never split ownership of one resource between tools.
- **V18.** Will a stock iroh client accept an `http://` relay URL for local mode (e.g. `http://<E>.localhost:8787`), or does local mode need a locally trusted TLS certificate? → shapes `just local`.
- **V19.** Does destroying the Worker script delete its DO namespace and stored data, and does re-creating it under the same name reuse or orphan object storage? → shapes `just down` semantics and the documented key-loss warning.

---

## 11. Repository layout and standards

```
crates/
  iroh-relay-ingress/     # sans-io server-side relay protocol (no runtime deps)
  iroh-ingress-transport/ # CustomTransport impl over IngressSession outputs
  iroh-do/                # workers-rs Durable Object shell, lifecycle, SQLite stores
  iroh-do-router/         # stateless router Worker
  iroh-static-lookup/     # optional peer-side E ↦ URL address lookup
  iroh-do-demo/           # native peer CLI for the replicable example (§13)
infra/                    # OpenTofu root module for the replicable example (§13)
justfile                  # build / up / mint / demo / local / down
harness/
  native/                 # tokio WS server + stock iroh client conformance tests
  workerd/                # wrangler dev integration + eviction tests
docs/
  verification.md         # Phase 0 output
  design-notes/           # one per pivot
  measurements/           # Phase 6 report + raw data
```

Standards: Rust 2024 edition; MSRV matching iroh 1.x; `#![deny(missing_docs)]` on public crates; errors via the same error-handling conventions iroh uses (match upstream; don't introduce a different error crate); `tracing` with iroh's span naming conventions; no `unwrap`/`expect` outside tests; every protocol constant cited to its upstream definition by path and commit.

---

## 12. Instructions to the implementing agent

1. Pin iroh, iroh-relay, noq and workers-rs to exact versions at the start. Record them in `docs/verification.md`.
2. Do Phase 0 completely before writing production code. Where a verification contradicts this plan, write a design note proposing the change, and stop for review if it touches §1 principles.
3. Build and prove everything natively (Phases 1–2) before touching Cloudflare.
4. Prefer reading upstream source over docs; cite file and commit for every protocol claim.
5. Keep upstream-candidate code (Phase 1 core, Phase 4 hooks) in separate crates/patches with their own tests, written in upstream's style, so they can be offered to n0 without disentangling.
6. Never add a mechanism this plan rejects in §3 without a design note and evidence.
7. Report results as: what was verified, what changed in the design, measurements, open questions. No narrative of effort.
8. Keep the replicable example (§13) green from the end of Phase 3 onward. It is the artifact reviewers will actually run, so a regression there counts as a regression in the design.

---

## 13. Replicable example (OpenTofu)

### 13.1 Goal and principles

A reviewer clones the repository, supplies a Cloudflare account, a zone and an API token, and gets a working, hibernating iroh endpoint on Durable Objects plus a native peer that talks to it, using only OpenTofu, `just`, a Rust toolchain and Node (for `worker-build`/`wrangler` as build and local-dev tools). The example exists to make every claim in this plan checkable by someone who distrusts it.

- **R1. One owner per resource.** OpenTofu owns every Cloudflare resource in production mode, the Worker script included. Wrangler is a build and local-dev tool only; it never deploys to the account in production mode. Split ownership produces drift that a reviewer cannot reproduce.
- **R2. Infrastructure is not identity.** Endpoint secret keys never pass through OpenTofu and never appear in state. Keys are minted inside the system (§13.4). Tearing down and re-applying infrastructure is a supported operation; destroying stored identity is an explicit, separately named one.
- **R3. Determinism.** Exact pins for OpenTofu, the Cloudflare provider, Rust toolchain, `worker-build`, workers-rs and iroh. The Worker bundle is content-addressed (its SHA-256 is an input to the script resource), so an unchanged build yields a zero-diff plan.
- **R4. Least privilege.** The API token is scoped to: Workers Scripts edit, Workers Routes edit, DNS edit on the one zone, SSL/certificates edit (only when the certificate pack is enabled). Document the exact permission list in `infra/README.md`.
- **R5. Local parity.** `just local` runs the same Worker bundle in workerd via `wrangler dev`, with the same demo CLI, no account required. Production and local differ only in routing and TLS.

### 13.2 Topology

One Worker script hosts both the router `fetch` handler and the DO class. Cross-script DO bindings add a deployment ordering problem and buy nothing here.

```
infra/ (OpenTofu)
  ├─ cloudflare_dns_record        "*.e.<zone>"  proxied   (placeholder target, Worker serves it)
  ├─ certificate for *.e.<zone>                            (only if Universal SSL does not cover it [V15/V17])
  ├─ cloudflare_workers_script    "iroh-do"
  │     modules: shim.mjs (main) + index_bg.wasm           (built by worker-build, hashed)
  │     bindings: durable_object_namespace ENDPOINTS → class IrohEndpoint
  │               secret ADMIN_TOKEN (from variable, see §13.4)
  │               plain_text ZONE_SUFFIX = "e.<zone>"
  │     migrations: tag v1, new_sqlite_classes = ["IrohEndpoint"]
  │     compatibility_date pinned
  └─ cloudflare_workers_route     "*.e.<zone>/*"   → iroh-do
```

`e.<zone>` is a dedicated subdomain, so every endpoint URL is `https://<E>.e.<zone>/`. A second-level wildcard is not covered by Cloudflare's free Universal SSL, which only covers one level under the zone apex; the module therefore exposes `wildcard_cert = "universal" | "advanced"`, defaulting to "advanced" with a certificate pack, and documents that setting `subdomain = ""` (endpoints directly under the apex) makes Universal SSL sufficient **[verify: V15, V17]**.

### 13.3 Module layout

```
infra/
  versions.tf      # required_version for OpenTofu; cloudflare provider pinned exactly; no other providers
  variables.tf     # account_id, zone_id, zone_name, subdomain = "e", wildcard_cert,
                   # worker_bundle_dir, admin_token (sensitive), compatibility_date
  main.tf          # resources in §13.2
  outputs.tf       # endpoint_url_template = "https://{E}.e.<zone>/", admin_url, script_name
  encryption.tf    # OpenTofu state encryption (see below)
  README.md        # token permissions, costs, teardown semantics, the key-custody warning
  examples/
    local-backend.tfvars.example
```

- **Bundle input:** `worker_bundle_dir` points at the `worker-build` output. The script resource reads the shim and the wasm module from it and receives their SHA-256 values, so a rebuild with identical inputs produces no diff (R3).
- **Fallback if V17 fails** (the provider cannot upload a multi-module wasm script, or cannot express the SQLite migration): OpenTofu still owns DNS, certificate and route; the script is deployed by a `terraform_data` resource whose `triggers_replace` is the bundle hash, running a pinned `wrangler deploy` with a generated config. This bends R1, so it must be documented as a known limitation with a link to the provider issue, and removed when the provider catches up. Do not adopt this fallback preemptively.
- **State encryption:** the admin token is sensitive and lands in state. Use OpenTofu's native client-side state encryption (a `pbkdf2` key provider with a passphrase from `TF_ENCRYPTION`-style environment configuration, AES-GCM method) so local and remote state are encrypted at rest. The default backend is local; `examples/` shows an S3-compatible backend pointed at R2 for teams. Mark the exact block syntax for the pinned OpenTofu version as checked during Phase R.
- **No provisioners in the happy path,** no `local-exec` for anything except the documented fallback.

### 13.4 Identity minting (keeps keys out of OpenTofu)

The DO name must be the EndpointId, so the key must exist before the object is addressed. Resolution:

1. `POST https://admin.e.<zone>/mint` with `Authorization: Bearer <ADMIN_TOKEN>` reaches the router (the `admin` label is reserved and is not a valid EndpointId, so routing is unambiguous).
2. The router generates a fresh secret key in the isolate, computes E, and calls the DO named E over RPC with the secret. The DO writes it to SQLite **only if it has no identity yet**, and verifies `public(secret) == E`. It then publishes its pkarr record.
3. The router returns E and the endpoint URL. The secret never leaves Cloudflare and never touches OpenTofu state, the CLI or logs.
4. **Invariant, checked on every activation:** the stored secret's public key equals the DO's name. A mismatch is a fatal configuration error, so the DO refuses all traffic and logs one structured error.
5. An upgrade or mint-less RPC to an E with no stored identity is rejected **without writing storage**, so probing random names creates no objects.

The admin token is generated by the reviewer (`just up` creates one with `openssl rand` if none is supplied) and passed as a sensitive variable. It authorizes minting only, never reading keys. A `GET /export` is deliberately absent; key export, if ever needed, is a separate, explicitly reviewed feature.

### 13.5 The demo peer (`crates/iroh-do-demo`)

A native CLI built on stock iroh 1.x with **default features and default discovery**, which is the point (P3):

```
iroh-do-demo ping  <E>            # echo round trip; prints connection type, RTT, 0-RTT status
iroh-do-demo bench <E> --mb 16    # throughput over the relayed path
iroh-do-demo lifecycle <E>        # scripted: session, wait past T_linger, session (expect 0-RTT),
                                  # wait past T_warm, session (expect cold wake); prints a timing table
iroh-do-demo replay <E>           # captures a 0-RTT flight and replays it; expects rejection
```

The ALPN is a namespaced echo protocol (`iroh-do/echo/1`). `lifecycle` output is the reviewer-facing evidence: cold wake latency broken into relay handshake, Endpoint instantiation (reported by the DO in a debug header or trailer) and QUIC handshake; whether 0-RTT was accepted; and the DO's own lifecycle state transitions as reported by a debug RPC exposed only when `DEMO_DEBUG=1` is bound.

### 13.6 `justfile` targets

```
just build      # cargo build demo CLI; worker-build the DO bundle; print bundle hash and size
just up         # build; tofu init; tofu apply -var-file=...; print endpoint URL template
just mint       # call /mint; print E; store E (not the key) in .demo/endpoint
just demo       # iroh-do-demo lifecycle $(cat .demo/endpoint)
just local      # wrangler dev with the same bundle; mint locally; run demo against http(s)://<E>.localhost [V18]
just plan       # tofu plan; CI asserts zero diff after up
just down       # tofu destroy; warns that endpoint identities are destroyed with the namespace [V19]
```

### 13.7 CI for the example

- **Every PR:** `tofu fmt -check`, `tofu validate`, `tflint` with the provider ruleset if available for the pinned provider, `just build`, `just local` + `iroh-do-demo lifecycle` against workerd.
- **Nightly (opt-in, credentials in a dedicated sandbox account and zone):** `just up`, `just mint`, `just demo`, `just plan` (assert empty), `just down`, then a script asserting the account has no leftover scripts, routes or DNS records with the example's tag. Publish the `lifecycle` table as a CI artifact so measurements accumulate over time and feed Phase 6.

### 13.8 What the example deliberately omits

Admission tokens, multi-endpoint fleets, custom application protocols, DO-initiated dials and the workerd fork track. Each would make the example less reproducible without making the core claims more checkable. They belong in separate examples once the corresponding phases land.

---

## Sources

- Cloudflare: [DO lifecycle](https://developers.cloudflare.com/durable-objects/concepts/durable-object-lifecycle/) · [outbound connections keep DOs alive](https://docs-durable-objects-instance-replaced-errors.previews.developers.cloudflare.com/durable-objects/concepts/durable-object-lifecycle/index.md) · [WebSocket hibernation (server only)](https://github.com/cloudflare/cloudflare-docs/blob/production/content/durable-objects/examples/websocket-hibernation-server.md) · [performance/timers](https://developers.cloudflare.com/workers/runtime-apis/performance/) · [connect handler](https://developers.cloudflare.com/workers/runtime-apis/handlers/connect/) · [connect handler changelog](https://developers.cloudflare.com/changelog/post/2026-08-18-connect-handler/) · [InfoQ: inbound TCP, UDP next](https://infoq.com/news/2026/08/workers-inbound-tcp-grpc)
- workerd: [#6062 binary auto-response](https://github.com/cloudflare/workerd/issues/6062) · [#4864 outbound WS hibernation](https://github.com/cloudflare/workerd/issues/4864) · [#6780 cluster mode](https://github.com/cloudflare/workerd/pull/6780)
- iroh: [1.0 announcement](https://iroh.computer/blog/v1) · [v1.0.0](https://github.com/n0-computer/iroh/releases/tag/v1.0.0) · [v1.1.0](https://github.com/n0-computer/iroh/releases/tag/v1.1.0) · [0.97 custom transports, noq, embeddable relay](https://www.iroh.computer/blog/iroh-0-97-0-custom-transports-and-noq) · [0.91 relay handshake](https://www.iroh.computer/blog/iroh-0-91-0-the-last-relay-break) · [relay handshake.rs](https://docs.rs/iroh-relay/0.98.0/src/iroh_relay/protos/handshake.rs.html) · [authenticated relays](https://www.iroh.computer/blog/authenticated-relays) · [0-RTT API](https://iroh.computer/blog/0rtt-api) · [wasm/browser](https://docs.iroh.computer/languages/wasm-browser) · [noq](https://github.com/zakura-core/iroh-quinn) · [pkarr Cloudflare worker](https://github.com/n0-computer/iroh-discovery-cloudflare-worker)
- Prior art: [HeddleCo/heddle#1333](https://github.com/HeddleCo/heddle/issues/1333) · [go-iroh relayproto (independent port of the relay wire protocol)](https://pkg.go.dev/github.com/tmc/go-iroh/internal/relayproto)