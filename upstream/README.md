# upstream/: restart-stable endpoint state for iroh

This directory is reference material. Nothing in this repository's build uses
it: iroh-do runs on stock iroh 1.4.0, which shows that an iroh endpoint can live
in a Durable Object without changes to iroh. What follows is a proposed patch to
iroh for the two things stock iroh cannot do when an endpoint restarts, with
the tests that justify it, kept here so it can be offered to the iroh
maintainers.

## The problem

A Durable Object endpoint restarts often. Each hibernation cycle rebuilds its
iroh `Endpoint` from the stored secret key, and Cloudflare can evict the object
at any time. iroh 1.4.0 creates three pieces of per-bind state randomly and
offers no way to supply them, so every restart looks like a new server:

| State created per bind | Effect on a restarted endpoint | Builder method added |
|---|---|---|
| Stateless-reset key and connection-id validation key | A peer's old connection hangs until its idle timeout instead of failing in about one round trip | `stateless_reset_key([u8; 32])`, from which both keys are derived |
| TLS server session store | 0-RTT is rejected after every restart, so each reconnect pays a full handshake | `tls_server_session_store(Arc<dyn StoresServerSessions>)` |

With the patch, an endpoint would derive the reset key from its secret key and
back the session store with durable storage. A Durable Object's SQLite database
is a single instance, so its single-use `take` gives correct 0-RTT anti-replay
across restarts.

The hooks are not specific to Cloudflare. Any iroh server that restarts with a
stable identity benefits.

## Why it is not wired in

A workspace-level `[patch.crates-io]` would also patch `iroh-do-demo`, whose
point is that the peer runs stock iroh. And the example should keep working on
published iroh. iroh-do reports the two effects above as known limitations
instead.

## Keeping it current

The patches target `v1.4.0`, the version iroh-do pins. When iroh-do moves to a
new iroh version, either delete this directory, if iroh has gained equivalent
hooks, or rebase the two commits onto the new tag, run `restart-tests/` and
`wasm-check/`, and update the results below.

noq and noq-proto 1.4.0 need no changes: everything the patch uses is already
public there.

| Base | Source | Commit |
|---|---|---|
| iroh 1.4.0 | tag `v1.4.0`, https://github.com/n0-computer/iroh | `55908a5809b61c72ec8deeff024038f4a9d68ab7` (= crates.io `.cargo_vcs_info.json`; `src/` byte-identical to the crates.io package) |
| noq / noq-proto 1.4.0 | tag `noq-v1.4.0`, https://github.com/n0-computer/noq | `34f75c49f93de5e2ff943483cabc5b6d0c5df918` (unchanged) |

## Layout

```
upstream/
  patches/iroh/0001-feat-iroh-add-Builder-stateless_reset_key.patch       git format-patch against v1.4.0
  patches/iroh/0002-feat-iroh-add-Builder-tls_server_session_store.patch
  iroh/            crates.io iroh-1.4.0 package + both patches (patch -p2). This is what [patch.crates-io] points at.
  restart-tests/   standalone test crate (own [workspace]) using [patch.crates-io] iroh = { path = "../iroh" }
  wasm-check/      standalone crate: patched iroh on wasm32-unknown-unknown, default-features = false + unstable-custom-transports
  git/             (gitignored) working clone of iroh at v1.4.0 with the two commits on branch `restart-state`
```

Recreate `git/` with:

```sh
git clone --branch v1.4.0 https://github.com/n0-computer/iroh.git upstream/git/iroh
git -C upstream/git/iroh switch -c restart-state
git -C upstream/git/iroh am ../../patches/iroh/*.patch
```

## The API

```rust
impl iroh::endpoint::Builder {
    /// Sets the stateless reset key, so a restarted endpoint can reset the connections of
    /// its previous instance. Default: a fresh random key per bind (unchanged).
    pub fn stateless_reset_key(self, key: [u8; 32]) -> Self;

    /// Sets where the TLS server stores session resumption state, so 0-RTT keeps working
    /// across restarts. Default: rustls' in-memory cache, created per bind (unchanged).
    pub fn tls_server_session_store(
        self,
        store: Arc<dyn rustls::server::StoresServerSessions>,
    ) -> Self;
}
```

Usage for an endpoint that restarts (e.g. a Durable Object between Active periods):

```rust
let ep = Endpoint::builder(presets::Minimal)
    .secret_key(secret_key.clone())                      // identity: persisted
    .stateless_reset_key(blake3::derive_key(              // derived, no storage needed
        "my-app 2026 iroh stateless reset key", &secret_key.to_bytes()))
    .tls_server_session_store(sqlite_session_store)      // persisted, single-use take()
    .alpns(vec![ALPN.to_vec()])
    .bind()
    .await?;
```

### Why this shape (in iroh's terms)

iroh's model is that *identity is the key and connections are ephemeral*. An endpoint
that comes back with the same `SecretKey` is the same endpoint, so it should be able to do
two things for its peers: tell them quickly that their old connections are gone, and let
them resume cheaply. Both need exactly one piece of state that today dies with every
`bind()`:

* **Stateless reset.** The key already exists inside iroh (`socket.rs`, random
  `Blake3HmacKey` per bind). The hook only lets the caller choose it.
* **Session resumption.** The store already exists inside rustls (`ServerConfig::session_storage`,
  default `ServerSessionMemoryCache::new(256)` per bind). The hook only lets the caller supply it.

Both are small, independent builder methods next to `max_tls_tickets` and `crypto_provider`,
matching how iroh's `Builder` exposes other per-bind TLS/QUIC state. Neither adds a trait,
a new public type or a config struct. Rejected alternatives:

* **A single `PersistentState` trait.** The two pieces
  have different shapes (a 32-byte secret vs. a live store with single-use semantics) and
  different lifetimes. A trait would add an abstraction with one implementation per caller.
* **A general `tls_server_config_hook(FnOnce(&mut rustls::ServerConfig))`.** That would expose
  every rustls server knob, including ones that break iroh's invariants (cert resolver,
  verifier, `max_early_data_size` values other than 0/`u32::MAX`, which panic in QUIC).
* **Exposing `noq::EndpointConfig`.** That would leak the whole noq config (versions, grease
  bit, which iroh relies on being `false`, payload sizes) for the sake of one key.
* **Deriving the reset key from the `SecretKey` automatically.** That changes default
  behaviour. Callers can do it themselves, as above.
* **The address-validation token key.** Not included: with iroh's
  `default-features = false` on noq, NEW_TOKEN is off (`NoneTokenLog`), and iroh never sends
  Retry, so a persisted token key would change nothing today. The only cost is the 3x
  anti-amplification limit during the first handshake after a restart.

## Patch 0001: `Builder::stateless_reset_key`

**Problem.** iroh builds `noq::EndpointConfig::new(Arc::new(Blake3HmacKey::new(&mut rng)))`
on every bind (`iroh/src/socket.rs:1011-1012` at v1.4.0) and never sets a CID generator, so
noq uses `HashedConnectionIdGenerator::new()` with a **random** key
(`noq-proto/src/config/mod.rs:58-60`, `cid_generator.rs:108-111`). A key alone is not enough,
because of the order of checks in `noq_proto::Endpoint::handle`
(`noq-proto/src/endpoint.rs:243-266`) for a datagram that matches no known connection:

1. Initial → `handle_first_packet` (new connection);
2. other long header → drop;
3. short header whose DCID fails `local_cid_generator.validate()` → **drop** ("dropping packet with invalid CID");
4. empty DCID → drop;
5. otherwise → `stateless_reset()` (rate-limited by `min_reset_interval` = 20 ms; padded to
   less than the inciting datagram, and only if that datagram is > 22 bytes).

A restarted instance with a fresh CID key fails step 3 for every old CID and stays silent.
The token in the reset (`ResetToken::new(reset_key, dst_cid)`) must also match the token the
old instance issued for that CID, so the reset key must be stable too.

**Change.** One 32-byte secret, from which iroh derives both keys with `blake3::derive_key`
under two distinct contexts (`tls/misc.rs`, new `StatelessResetSecret`):

* reset key = `derive_key("iroh 2026-10-21 stateless reset key", secret)` → `Blake3HmacKey`
  (same type as today's random one);
* CID key = first 8 bytes of `derive_key("iroh 2026-10-21 connection id key", secret)` →
  `EndpointConfig::cid_generator(|| HashedConnectionIdGenerator::from_key(k))`.

One input means the two keys cannot drift apart. Supplying a reset key without the CID key
would silently do nothing. With no call, `socket.rs` takes the old branch verbatim:
random `Blake3HmacKey`, noq's default generator. The secret is redacted in `Debug`.

**noq-proto APIs used (all public in 1.4.0):** `EndpointConfig::new(reset_key)`,
`EndpointConfig::cid_generator(factory)` (`config/mod.rs:81-87`),
`HashedConnectionIdGenerator::from_key(u64)` (`cid_generator.rs:117-122`; its doc already says
"recognize a consistent set of connection IDs across restarts").

## Patch 0002: `Builder::tls_server_session_store`

**Problem.** `TlsConfig::make_server_config` (`iroh/src/tls.rs:110-128`) leaves
`session_storage` at rustls' default, a fresh `ServerSessionMemoryCache::new(256)` per bind.
`ticketer` is `NeverProducesTickets`, i.e. stateful resumption, which rustls requires for
early data (`max_early_data_size > 0 && !ticketer.enabled()`, `rustls/src/server/tls13.rs:671`).
So every restart forgets every session and 0-RTT is rejected.

**Change.** If a store was supplied, `make_server_config` sets `crypto.session_storage = store`.
Nothing else changes:

* `max_early_data_size` stays `u32::MAX`. iroh already sets it, and QUIC allows only 0 or
  `u32::MAX` (RFC 9001 §4.6.1), so there is nothing to configure.
* `ticketer` stays disabled, so resumption stays stateful and **anti-replay stays with the
  store**. rustls looks TLS 1.3 PSKs up with `StoresServerSessions::take`
  (`tls13.rs:121-134`), and only a hit can lead to early data. The doc comment states the
  contract: `take` must be atomic and single-use across every instance sharing the store.
  rustls additionally enforces a 60 s ticket-age skew (`msgs/persist.rs:340`).
* `Endpoint::set_alpns` / `create_server_config_builder` clone the same `QuicServerConfig`,
  so they keep the store.

## Wiring it in

In a consuming workspace's root `Cargo.toml`:

```toml
[patch.crates-io]
iroh = { path = "upstream/iroh" }
```

Only `iroh` is replaced. `iroh-base`, `iroh-relay`, `noq*` and everything else still come
from crates.io, so there are no duplicate types. The patched tree is the crates.io package
(normalized `Cargo.toml`, registry dependencies), not the git workspace member, whose path
dependencies would pull in a second `iroh-base`/`iroh-relay`.

## Tests

### restart-tests (`cd upstream/restart-tests && cargo test`)

Native UDP on loopback. Topology: `client [::1] ⇄ proxy ⇄ server 127.0.0.1:p`. The client is
IPv6-only and the server IPv4-only, so no direct path can bypass the proxy even though the
server advertises its address via QNT. The proxy adds 25 ms one way (RTT ≈ 54 ms measured),
can blackhole server→client traffic (so a "crash" cannot leak a CONNECTION_CLOSE), and can
capture client datagrams for replay. A restart is: drop `A` (abort tasks, drop `Endpoint`
without `close()`, proxy downstream cut) and bind `A'` with the same `SecretKey` on the same
`127.0.0.1:p`.

| # | Test | Result |
|---|---|---|
| 1 | `abrupt_restart_with_reset_key_fails_fast`: same secret + reset key; client writes on the old connection | `ConnectionError::Reset` after **54.3 ms = 1.01 RTT** (assert < 2 RTT); redial to A' works |
| 1c | `abrupt_restart_default_waits_for_idle_timeout`: today's behaviour | `TimedOut` after **6.00 s** (= idle timeout) |
| 1n | `abrupt_restart_with_wrong_reset_key_is_ignored`: A and A' use different keys | `TimedOut` after 5.9 s: a reset under the wrong key is ignored by the peer |
| 2 | `zero_rtt_accepted_after_restart_with_persistent_store`: shared in-memory store across A and A' | `ZeroRttStatus::Accepted`; server reads the stream; store: 4 puts, 1 take hit |
| 2c | `zero_rtt_rejected_after_restart_by_default`: today's behaviour | `ZeroRttStatus::Rejected` (falls back to 1-RTT) |
| 3 | `replayed_0rtt_flight_rejected_after_restart`: every client datagram of the accepted 0-RTT handshake (Initial+0-RTT+Handshake) replayed from another address to A'' (a further restart, same store) | nothing delivered to the app; store `take` **miss** (hits unchanged) |
| 3c | `replayed_0rtt_flight_accepted_if_take_is_not_single_use`: control, same replay but the store's `take` doesn't remove | replayed 0-RTT stream **delivered** (`is_0rtt = true`). Shows that test 3 passes because of single-use `take`, not a malformed replay |

7/7 pass. The full suite was run 12× in a row with parallel test threads, green every time
(~7 s per run). Two details keep the tests deterministic:

* The test server's per-connection tasks live in a `JoinSet`, so aborting the server drops every
  `Connection` (and with it the `Endpoint`). Otherwise the port isn't released.
* The client sometimes sends its 0-RTT stream bytes after the handshake completes (task timing),
  even though early data was accepted. Test 2 asserts on `ZeroRttStatus::Accepted`, the server's
  verdict. Tests 3/3c need a flight that physically carries 0-RTT data, so they retry setup until
  the server saw `RecvStream::is_0rtt()` (bounded at 10 attempts).

### Upstream iroh tests (defaults unchanged)

Run in `git/iroh` (the workspace at the tag, with the patches committed):
`cargo test -p iroh --lib --features test-utils -- endpoint:: socket:: tls::`
(87 tests in the touched modules, incl. the new `tls::misc::tests::stateless_reset_secret_is_deterministic_and_domain_separated`).

| Run (same clone, same target dir, alternating `git checkout`) | v1.4.0 (unpatched) | restart-state (patched) |
|---|---|---|
| round 1 | 82 pass / 3 fail | 81 pass / 5 fail |
| round 2 | 79 pass / 6 fail | 82 pass / 4 fail |
| earlier, separate trees | 84/1, 85/0 | 83/3, 84/2 |

The failures are the same pool of timing-sensitive network tests on both sides
(`test_two_devices_roundtrip_*`, `endpoint_unreachable_relay_direct_connect_succeeds`,
`test_endpoint_online_add_relay`, `test_paths_watcher`, `connect_multi_time`) and vary run to run.
Every one of them passes when run alone on the patched branch, except `test_paths_watcher`,
which is flaky on both (3/3 fail on unpatched, 3/3 pass on patched in back-to-back
isolated runs). The default path through both patches is the old code verbatim
(`None` → random `Blake3HmacKey` + noq default CID generator; `None` → rustls default store),
so no behavioural difference is expected, and none is observed.
`cargo clippy -p iroh --all-targets --features test-utils`: no warnings. `cargo fmt`: clean.

### wasm32

`cd upstream/wasm-check && CC_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/clang AR_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/llvm-ar cargo build --target wasm32-unknown-unknown`
builds the patched iroh with `default-features = false, features = ["unstable-custom-transports"]`,
calling both new builder methods (ring with `wasm32_unknown_unknown_js`, as in
`crates/iroh-do-core`). Result: **builds**; the native build of the same crate also passes.

## Diffstat (vs v1.4.0)

```
0001  iroh/src/endpoint.rs | 34 +++++++++++++-   (builder field + method + docs)
      iroh/src/socket.rs   | 23 +++++++--     (Options field; EndpointConfig branch; 2 test Options literals)
      iroh/src/tls/misc.rs | 60 +++++++++++++++  (StatelessResetSecret + unit test)
0002  iroh/src/endpoint.rs | 31 +++++++++++++-  (builder field + method + docs)
      iroh/src/tls.rs      |  6 ++++            (field + 3 lines in make_server_config)
total 4 files changed, 149 insertions(+), 5 deletions(-)   noq / noq-proto: 0
```

## Risks and open points

* **Key handling is the caller's job.** Anyone who knows the reset key can reset the endpoint's
  connections (given a CID, which is on the wire). Deriving it from the `SecretKey` adds no new
  secret to protect. Use a distinct key per endpoint.
* **CID key lifetime.** `HashedConnectionIdGenerator` uses FxHash, which is not cryptographic. A stable key
  means an observer who could recover it from many CIDs could link CIDs across restarts, not
  only within one instance. It can also forge "valid" CIDs to solicit resets, which are
  rate-limited (20 ms) and smaller than the inciting packet, so there is no amplification. This is
  noq's existing design for "across restarts".
* **Store correctness = replay safety.** A replicated or eventually consistent store whose
  `take` is not atomic makes 0-RTT replayable (test 3c demonstrates this). A Durable Object's
  single-threaded SQLite satisfies the contract.
* **Same address required for resets.** The peer only accepts a reset arriving on the 4-tuple
  it registered the token for. For a DO behind its relay URL this is the same relay path. Not
  tested here (native UDP only).
* **Upstreaming.** The two commits are stacked but logically independent, and each builds on its own. For the
  upstream PR, tests 1 and 2 would move into `iroh/src/endpoint.rs`'s test module (they use only
  public API).

## License

`iroh/` and the patches are derived from iroh and keep its license, MIT OR
Apache-2.0 (`iroh/LICENSE-MIT`, `iroh/LICENSE-APACHE`). The rest of this
repository is MIT (`../LICENSE`).
