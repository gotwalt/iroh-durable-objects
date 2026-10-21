# 005: App-level idle close; restart state needs upstream hooks

**Status:** accepted (2026-10-21). Amends §5.5 (Active idle policy), §5.6,
§5.8, Phase 2/4 acceptance. No §1 principle changes.
**Evidence:** `docs/verification/iroh-endpoint.md` (keep-alive, V8, V9),
`docs/verification/relay-protocol.md` (V5, V6).

## 1. Active → Warm needs an application idle close

QUIC's idle timeout never fires: iroh sends per-path keep-alives every 5 s
that the public API cannot disable, and stock peers do the same. As written,
§5.5 would keep the DO Active for as long as a peer holds a connection.

Change: the DO closes a connection itself (`CONNECTION_CLOSE`, app code
`0`, reason `idle`) when it has had **no open streams and no application
bytes for `T_idle`** (default 10 s). This is the "optional operator cap" of
§5.5, now on by default. Peers see a graceful close and redial on demand.

## 2. Warm is client-bounded

The client closes a non-home relay socket 60 s after its last send and
reconnects eagerly if the server closes it. So: the DO never closes idle
sockets (`T_warm` is removed); Warm lasts ≤ 60 s after the peer's last send;
client pings (every 15 s) wake the DO for a Pong. Measure Pong latency from
hibernation against the ~500 ms deadline (V6).

## 3. Restart-stable state requires upstream hooks (Phase 4)

In 1.4.0 the stateless-reset key, the CID-validation key and the TLS session
store are all created per bind and are not injectable. Consequences without
a patch: abrupt eviction costs the peer one idle timeout (not ≤ 2 RTT), and
0-RTT is rejected after every Active → Warm cycle.

Plan: Phases 1–3 and the example ship on stock 1.4.0 and report these as
known gaps. In parallel, a minimal patch set (`upstream/`, against iroh
1.4.0 and noq 1.4.0 tags) adds builder hooks for (a) reset key, (b)
CID-generator key, (c) server session store + `max_early_data_size`. It is
wired in through `[patch.crates-io]` only once its own tests pass, and the
example's `lifecycle` output then shows 0-RTT after Warm and fail-fast resets.

## Update (2026-10-21): patch landed, wiring deferred

The patch set is in `upstream/` (iroh-only, +149/−5; noq needed nothing):
`Builder::stateless_reset_key` and `Builder::tls_server_session_store`.
Decision (revised after the stock production demo passed 26/26): the patch is
**not** wired into this repository. The example runs entirely on stock iroh
1.4.0; the two gaps (0-RTT rejected after Warm, abrupt eviction costs the peer
one idle timeout) are reported as `info` rows by `lifecycle`, not failures.
The patch set remains an upstream proposal (§8 item 2) with its own tests.

## 4. Silent open streams are capped

`t_idle` only applies when a connection has no open streams, and keep-alives
keep the connection itself alive, so a peer holding a stream open without
sending anything kept the DO Active (and billed) indefinitely; observed with
`iroh-do-demo chat`, see `docs/verification/hibernation-references.md`.

Change: `t_silent` (default 60 s). A connection with open streams but no
application bytes for `t_silent` is closed with app code `0`, reason
`silent`. Test: `harness/native` `silent_open_stream_is_closed`.
