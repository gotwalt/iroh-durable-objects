# Phase 0 verification: iroh 1.4.0 Endpoint inside a DO (V7, V8, V9, V10, V13, V14)

Scope: an iroh `Endpoint` built for `wasm32-unknown-unknown`, with **no relay transport, no IP
transports**, and a single custom transport ("ingress") whose paths are peer EndpointIds, fed
by WebSocket messages. Pinned sources: `iroh-1.4.0`, `iroh-base-1.4.0`, `noq-1.4.0`,
`noq-proto-1.4.0`, `noq-udp-1.4.0`, `n0-future-0.3.2`, `netwatch-0.19.3`, `rustls-0.23.46`
(cargo registry). Citations are `crate-version/path:line`.

Experiment crate: `wasmcheck` (not kept in the repository)
(`src/lib.rs` is the skeleton reproduced in the appendix; `tests/native.rs` holds the behavioural tests).

## Summary

| Item | Verdict | Severity |
|---|---|---|
| V7 compile for wasm32, relay off, custom transport | **Yes.** Builds, instantiates, binds and closes in node. Zero JS timers remain after `close().await` + drop | OK |
| V7 workerd run | **Not tested** (node only). A handshake inside wasm is also untested; the native tests exercise the same code | open |
| Keep-alive / idle | **QUIC idle timeout never fires**: iroh's per-path keep-alive (5 s) can't be turned off through the public API, and stock peers send keep-alives too | **BLOCKER for §5.5 as written** (design change, not a code blocker) |
| V8 stateless reset after restart | **No.** The reset key *and* the CID-validation key are random per bind and not injectable. A new instance drops old-CID packets **without** sending a reset. Measured: the peer waits for the full idle timeout | **BLOCKER for §5.6/§5.8 "fails fast"** (needs an upstream hook) |
| V9 custom `StoresServerSessions` / `max_early_data_size` | **No hook.** The server rustls config is built internally. Default is a stateful in-memory store (256 entries) with 0-RTT on. Measured: 0-RTT accepted on the same instance, **rejected after re-bind** | Upstream item. 0-RTT is lost on every Active→Warm cycle, not only on eviction |
| V10 address-validation token key | Random per bind, not settable for the endpoint-level config. With `default-features=false` the `bloom` feature is off, so NEW_TOKEN is not used at all | Perf only |
| V13 transport id / CustomAddr | id is **u64** (spec says u16). `0x00–0x1F` reserved, `0x20` test. Binary form: 8-byte LE id + data | Spec fix |
| V14 custom-only endpoint accepts | **Yes** (measured). The remote need not be known. The server side never opens paths or holepunches. Peer-advertised IP and relay addresses are never dialled | OK |

---

## V7: wasm build, relay off, timers, background tasks

### Answer

**Compiles and links.** Feature set `iroh = { version = "=1.4.0", default-features = false, features = ["unstable-custom-transports"] }`.
`default-features = false` drops `metrics`, `portmapper`, `fast-apple-datapath` and `tls-ring` (`iroh-1.4.0/Cargo.toml:67-99`).
Without `tls-ring`, a `CryptoProvider` **must** be passed to `Builder::crypto_provider`, or `bind()` fails with
`BindError::InvalidCryptoProvider` (`iroh-1.4.0/src/endpoint.rs:235-237`, `:756-773`). The spike depends on
`rustls = { default-features = false, features = ["ring","std"] }` and passes `rustls::crypto::ring::default_provider()`.

**getrandom:** no rustflags/cfg needed. iroh already enables `getrandom 0.4` `wasm_js` on wasm (`iroh-1.4.0/Cargo.toml:407-409`), and in 0.4 the feature alone selects the backend (`getrandom-0.4.3/src/backends.rs:172-174`). The lockfile has no getrandom 0.3. ring's own getrandom 0.2 needs `ring/wasm32_unknown_unknown_js` (`ring-0.17.14/Cargo.toml:180`).

**Gotcha (cost an hour):** on macOS, ring's C code must be archived with **llvm-ar**. Apple `ar` silently writes an empty archive for wasm objects: a 96-byte `.a`. The link still "succeeds", because wasm-ld turns undefined symbols into imports, and the module ends up with 44 `env.ring_core_*` imports that fail at instantiate. Check that the import list contains only `__wbindgen_*`.

#### Exact build commands

```sh
cd wasmcheck   # throwaway crate, not kept
export CC_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/clang
export AR_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/llvm-ar   # REQUIRED on macOS
cargo build --target wasm32-unknown-unknown --release                  # profile: opt z, lto, cgu=1, panic=abort, strip
WB=~/Library/Caches/worker-build/wasm-bindgen-aarch64-apple-darwin-0.2.129/wasm-bindgen
WO=~/Library/Caches/worker-build/wasm-opt-arm64-macos-132/bin/wasm-opt
$WB --target web --out-dir out target/wasm32-unknown-unknown/release/wasmcheck.wasm
$WO -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
    --enable-mutable-globals --enable-reference-types --enable-multivalue \
    out/wasmcheck_bg.wasm -o out/opt.wasm
node out/smoke.mjs                       # bind + close, counts live setTimeouts
cargo test --test native -- --nocapture  # native behavioural tests (V8/V9/V14/keep-alive)
```

The skeleton also builds natively with plain `cargo build`.

#### Sizes (whole Endpoint reachable through `#[wasm_bindgen]` exports, so LTO can't strip it)

| Artifact | Bytes | gzip -9 |
|---|---|---|
| rustc output (`wasmcheck.wasm`) | 3,369,059 | 1,105,447 |
| after wasm-bindgen (`wasmcheck_bg.wasm`) | 2,588,608 | n/a |
| after `wasm-opt -Oz` (v132) | 2,221,152 | **951,086** |

The final imports are only `__wbindgen_placeholder__` (128) and `__wbindgen_externref_xform__` (2). They include `setTimeout`/`clearTimeout`, `crypto.getRandomValues`, `addEventListener`, and two `fetch` imports that are linked but never called in this configuration (see below).

### Disabling relay on wasm

`Builder::empty()` creates **no** transports on wasm. The IPv4/IPv6 defaults are `cfg(not(wasm_browser))` (`iroh-1.4.0/src/endpoint.rs:193-199`), and `empty()` is documented as "no address lookup services, and RelayMode::Disabled" (`:192`). `relay_mode(RelayMode::Disabled)` removes any relay config (`:155-158`, `:580-583`), and `clear_relay_transports()` does the same (`:515-519`). Neither is cfg-gated. With no relay config, `relay_map` is empty (`iroh-1.4.0/src/socket.rs:899-911`) and no `RelayTransport` is created (`iroh-1.4.0/src/socket/transports.rs:225-229`). Do **not** use `presets::N0`: it adds the pkarr publisher and resolver plus default relays (`iroh-1.4.0/src/endpoint/presets.rs` N0 impl, `apply` lines 47-66 of the file excluding comments). On native, call `clear_ip_transports()` (`endpoint.rs:507-512`). On wasm that method doesn't exist and isn't needed.

### Timers on wasm

- The noq timer is `iroh::runtime::web::Timer(n0_future::time::Sleep)` (`iroh-1.4.0/src/runtime.rs:72-75`, `:113-135`). Spawn is `wasm_bindgen_futures::spawn_local` (`runtime.rs:98-101`).
- `n0_future::time::Sleep` on wasm is a raw `globalThis.setTimeout` via wasm-bindgen (`n0-future-0.3.2/src/time.rs:64-82`, `:464-487`). Drop and reset call `clearTimeout` (`time.rs:108-116`, `:128-157`). Time is `web_time::Instant` (`time.rs:32`).
- noq uses the clock, not the timer, to detect expiry (`noq-1.4.0/src/connection.rs:1845-1886`). This is consistent with the §5.4 frozen-clock argument. When `poll_timeout()` returns `None`, the existing timer is **not** cleared (`connection.rs:1846-1848`), so a stale `setTimeout` can linger until it fires. That's harmless, and it disappears when the connection driver ends.
- **Hazard:** `sleep_until`/`reset` cast `duration.as_millis() as i32` (`time.rs:76`, `:141`). Any deadline more than ~24.8 days away wraps negative and fires immediately. Never configure a timer interval ≥ 2^31 ms. The spike uses 24 h for "disabled" keep-alive.

**Timers that exist while the Endpoint is alive, even with zero connections** (measured in node, see `out/smoke.mjs`):
`[250 ms, ~25 s, ~15 s]`. These are:
1. the netwatch debounce `sleep(250ms)`, created eagerly (`netwatch-0.19.3/src/netmon/actor.rs:85-89`);
2. the socket actor's `periodic_re_stun_timer`, 20–26 s, always armed (`iroh-1.4.0/src/socket.rs:2017-2031`, polled at `socket.rs:1533-1537`); with an empty relay map net_report is skipped (`socket.rs:795-799`), so it dials nothing;
3. the netwatch wall-time poll, 15 s (`netwatch-0.19.3/src/netmon/actor.rs:32`, `:91`).

Per remote, `RemoteStateActor` adds a 60 s `interval` and a 60 s idle sleep (`iroh-1.4.0/src/socket/remote_map/remote_state.rs:66`, `:74`, `:248-251`). Per connection, noq adds its own timers (idle, PTO, keep-alive).

### Are all timers and tasks gone after `close().await` + drop?

**Yes. Measured:** after `ep.close().await; drop(ep)` plus a few microtask turns and one macrotask, the live `setTimeout` count is **0** (6 were created in total). The mechanism:
- `close()` cancels the tokens, `noq_endpoint().close()`, `wait_all_draining().await`, cancels the actors, and awaits the actor task with a 100 ms timeout (`iroh-1.4.0/src/socket.rs:1128-1188`).
- On wasm, `Runtime::shutdown()`/`abort()` are **no-ops** (`runtime.rs:57-63`). Task teardown relies on cancellation tokens and `AbortOnDropHandle`/`JoinSet` drop. On wasm, abort just sets a flag and wakes the task (`n0-future-0.3.2/src/task.rs:240-244`, `:282-287`, `:358-360`), so the future, and its `Sleep`s, are dropped on a **later microtask**, not synchronously.
- The noq `EndpointDriver` stops once connections are drained and either `close` was called or all handles were dropped (`noq-1.4.0/src/endpoint.rs:470-479`, `:882-893`).
- netwatch's JS `online`/`offline` listeners are removed on drop (`netwatch-0.19.3/src/netmon/wasm_browser.rs:73-83`).

**Design impact:** the §5.5 "assert zero timers" check must run after yielding to the event loop, not immediately after `close()` returns. `close()` itself can take up to about 3×PTO (≈3 s) while connections drain (`iroh-1.4.0/src/endpoint.rs:1713-1718`).

### Background tasks that could dial out, and how to disable each

| Component | Default with `Builder::empty()` + `default-features=false` | Evidence |
|---|---|---|
| Relay actor / relay WebSocket | none (no relay config) | `socket/transports.rs:225-229` |
| net_report (HTTPS/QAD probes, captive portal) | task runs on re-stun ticks but returns early with empty RelayMap; QAD only with IP transports | `socket.rs:795-799`, `socket.rs:992-1004` |
| Portmapper (UPnP/PCP/NAT-PMP) | not compiled on wasm (target-gated dep; feature off) | `iroh-1.4.0/Cargo.toml:432-435` |
| Address lookup / pkarr publish | none unless added; `publish_my_addr` is a no-op with no services and nothing to publish | `endpoint.rs:192`, `socket.rs:668-689` |
| DNS resolver | not on wasm | `endpoint.rs:263-269` |
| Metrics | `metrics` feature off → iroh-metrics counters compile to no-ops; no tasks | `Cargo.toml:67-77` |
| netwatch monitor | always on; JS `online`/`offline` listeners + 15 s timer; no network I/O; tolerates a missing `addEventListener` | `netwatch-0.19.3/src/netmon/wasm_browser.rs:43-59` |
| Holepunching | server-side connections never initiate it (see V14) | `remote_state.rs:523-533` |

---

## Custom transport API (iroh 1.4.0, feature `unstable-custom-transports`)

Public module: `iroh::endpoint::transports` re-exports `PathSelection, PathSelectionContext, PathSelectionData, PathSelector, Addr, AddrKind, FourTuple, RecvInfo, Transmit, CustomEndpoint, CustomSender, CustomTransport` (`iroh-1.4.0/src/endpoint.rs:29-48`). `CustomAddr` comes from `iroh_base`.

```rust
// iroh-1.4.0/src/socket/transports/custom.rs:24-105
pub trait CustomTransport: Debug + Send + Sync + 'static {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>>;           // called once per Builder::bind
}
pub trait CustomEndpoint: Debug + Send + Sync + 'static {
    fn watch_local_addrs(&self) -> n0_watcher::Direct<Vec<CustomAddr>>;
    fn create_sender(&self) -> Arc<dyn CustomSender>;
    fn poll_recv(&mut self, cx: &mut Context, bufs: &mut [io::IoSliceMut<'_>],
                 metas: &mut [noq_udp::RecvMeta], recv_infos: &mut [RecvInfo]) -> Poll<io::Result<usize>>;
    fn max_transmit_segments(&self) -> NonZeroUsize { NonZeroUsize::MIN }   // GSO; default 1
}
pub trait CustomSender: Debug + Send + Sync + 'static {
    fn is_valid_send_addr(&self, addr: &CustomAddr) -> bool;
    fn poll_send(&self, cx: &mut Context, dst: &CustomAddr, src: Option<&CustomAddr>,
                 transmit: &Transmit<'_>) -> Poll<io::Result<()>>;
}
// iroh-1.4.0/src/socket/remote_map/remote_state.rs:1466-1475
pub trait PathSelector: Send + Sync + Debug + 'static {
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection;   // PathSelection::none()/set(&p)
}
// Transmit: pub contents: &[u8], pub segment_size: Option<usize>; ecn is pub(crate) (transports.rs:553-559)
// RecvInfo::new(remote: CustomAddr, local: Option<CustomAddr>) (transports.rs:646-659)
```

Registration: `Builder::add_custom_transport(Arc<dyn CustomTransport>)` (`endpoint.rs:821-825`) and `Builder::path_selector(Arc<dyn PathSelector>)` (`endpoint.rs:848-852`). The default selector is `BiasedRttPathSelector` (`endpoint.rs:216`).

- **Send + Sync bound:** JS handles (`WebSocket`) can't live in the transport. Route sends through a thread-local JS callback or a queue that the glue drains. The skeleton uses an `IngressSink` trait.
- **Receive is poll-based.** iroh polls each transport's `poll_recv`, alternating order for fairness (`transports.rs:278-367`). Use your own queue and store `cx.waker()` on `Pending`. The skeleton uses `Mutex<VecDeque>` plus a `Waker`. On wasm `noq_udp::BATCH_SIZE == 1` (`noq-udp-1.4.0/src/lib.rs:95-97`), so one datagram is delivered per poll, and `max_receive_segments` is 1 (`transports.rs:450-453`). The slot buffer is `max_udp_payload_size` = 1472 bytes (`noq-proto-1.4.0/src/config/mod.rs:63`, `noq-1.4.0/src/endpoint.rs:920-923`). Fill `meta.len`, `meta.stride` (= len) and optionally `meta.ecn`. iroh overwrites `meta.addr` with the mapped address (`socket.rs:648-660`). If every transport errors repeatedly, the endpoint shuts down (`transports.rs:342-357`), so return `Pending`, not `Err`, when idle.
- **"No route"** is any `Poll::Ready(Err(io::Error))`. iroh logs and swallows it as `Ok` ("dropped transmit"). `Pending` is also treated as a dropped datagram (`transports.rs:1277-1299`). If no sender accepts the address, the datagram is blackholed with `Ok` (`transports.rs:1039-1042`). Loss recovery handles all three cases. Never block.
- **MTU:** custom transports have no MTU knob in the trait. QUIC's minimum is 1200. The DO advertises `max_udp_payload_size` 1472 (default `EndpointConfig`, `noq-proto-1.4.0/src/config/mod.rs:63`). iroh doesn't change it (`socket.rs:1011-1018`). `may_fragment` is false on wasm (`transports.rs:460-463`).
- **segment_size / GSO:** with `max_transmit_segments` = 1 (the default), every transmit is one datagram. Still, split `contents` by `segment_size` if it's `Some` (as the test transport does: `iroh-1.4.0/src/test_utils/test_transport.rs:206-215`).
- **ECN:** `Transmit.ecn` is `pub(crate)`, so a custom sender **cannot read ECN** (`transports.rs:554`). On receive you may set `meta.ecn`.
- **CustomAddr ↔ EndpointId for incoming:** iroh maps every received `CustomAddr` to a `CustomMappedAddr`, created on demand (`custom_addrs.get(remote)`, `socket.rs:648-655`). The peer's EndpointId is learned from TLS. After the handshake, `register_connection(remote_id, conn)` attaches the connection and its path to that remote's state actor (`endpoint/connection.rs:317-347`, `remote_state.rs:387-452`). **No prior knowledge of the peer is needed.** The CustomAddr payload is just our path label.

---

## V8: stateless reset key; do resets happen?

### Answer: **No, on both counts in practice.**

1. **Reset key:** iroh builds `noq::EndpointConfig::new(Arc::new(Blake3HmacKey::new(&mut rand::rng())))` internally on every bind (`iroh-1.4.0/src/socket.rs:1011-1018`). No Builder method touches it. noq itself supports `EndpointConfig::reset_key` (`noq-proto-1.4.0/src/config/mod.rs:88-93`), but iroh doesn't expose `EndpointConfig`.
2. **The CID generator also blocks resets.** Before resetting, noq checks `local_cid_generator.validate(dst_cid)` and silently drops on failure (`noq-proto-1.4.0/src/endpoint.rs:253-258`). The default is `HashedConnectionIdGenerator` (`config/mod.rs:58-60`), whose key is **random per instance** (`cid_generator.rs:108-111`). A restarted DO therefore classifies every old CID as invalid and drops it ("dropping packet with invalid CID"). It never reaches `stateless_reset()` (`endpoint.rs:262-266`, `:271-319`). Persisting the reset key alone would **not** be enough. The CID-generator key (`HashedConnectionIdGenerator::from_key`, `cid_generator.rs:117-122`) or the generator itself must also be injectable.
3. On a *live* endpoint, resets for unknown-but-valid CIDs on custom-transport paths are transport-agnostic and rate-limited by `min_reset_interval` (20 ms) (`endpoint.rs:278-284`, `config/mod.rs:66`). They're returned as a `Transmit` to the same `network_path`, i.e. back out through the custom sender.

**Measured** (`tests/native.rs::v8_restart_stateless_reset`): the old instance went silent and a new instance with the same secret key took the route. The client's connection ended **after 9.99 s with `TimedOut`**, which equals the 10 s idle timeout. No reset was observed.

**Design impact:** §5.6/§5.8 "abrupt eviction fails in ~1 RTT" is false on iroh 1.4.0. The peer waits for its idle timeout, and with stock peers' 5 s keep-alives the idle timeout is ≈ the negotiated `max_idle_timeout`. Upstream ask: a `Builder` hook for `EndpointConfig` (reset key + CID generator factory), or a single "stateless endpoint secret" that derives both. Until then, keep `max_idle_timeout` modest (it bounds the stall), and possibly use a *relay-level* signal: when a peer's socket reconnects to a cold DO, the DO knows no connection exists. But no relay frame can make the client drop a QUIC connection, so this stays an upstream item.

---

## V9: TLS session store, `max_early_data_size`, 0-RTT

### Answer

- **No hook.** The server `rustls::ServerConfig` is built privately in `TlsConfig::make_server_config` (`iroh-1.4.0/src/tls.rs:110-128`) and wrapped into `noq::ServerConfig` in `StaticConfig::create_server_config` (`socket.rs:249-257`). The public `ServerConfigBuilder` (from `Endpoint::create_server_config_builder`, `endpoint.rs:1772-1775`) only exposes transport config, validation-token config, token key, retry lifetime, incoming limits and time source (`endpoint/quic.rs:614-717`). It has nothing for crypto, `session_storage`, `ticketer` or `max_early_data_size`. `Builder::max_tls_tickets` only sizes the **client** cache (`endpoint.rs:745-754`, `tls.rs:64-66`).
- **Server default:** `max_early_data_size = u32::MAX`, so 0-RTT is enabled (`tls.rs:123-125`). `session_storage` is rustls' default `ServerSessionMemoryCache::new(256)`, and `ticketer` is `NeverProducesTickets`, i.e. **stateful** resumption (`rustls-0.23.46/src/server/builder.rs:111-123`). rustls only allows early data with a stateful store: `max_early_data_size > 0 && !ticketer.enabled()` (`rustls-0.23.46/src/server/tls13.rs:671`). Lookups use single-use `take()` (`tls13.rs:125-133`). `send_tls13_tickets = 2` (`builder.rs:123`). The cache is created fresh per `bind()`, so **all tickets die with the Endpoint**.
- **Client:** the `ClientSessionMemoryCache` holds 256 tickets by default, with `enable_early_data = true` (`tls.rs:64-66`, `:87-89`).
- **Client API (1.4.0):** `Endpoint::connect_with_opts(addr, alpn, opts).await? -> Connecting`. Then `Connecting::into_0rtt() -> Result<OutgoingZeroRttConnection, Connecting>` (`endpoint/connection.rs:534-560`). Use the connection immediately, then call `handshake_completed().await -> ZeroRttStatus::{Accepted(Connection), Rejected(Connection)}` (`connection.rs:1299-1301`, `:704-711`). On the server, `Accepting::into_0rtt() -> IncomingZeroRttConnection`, with `handshake_completed().await -> Connection` (`connection.rs:626-646`, `:1257-1259`).

**Measured** (`tests/native.rs::v9_zero_rtt`): #1 fresh: no ticket → 1-RTT. #2 against the same DO instance: **0-RTT ACCEPTED**. #3 after `close()` and re-bind with the same key: **0-RTT REJECTED**. Full handshake fallback works.

**Design impact:** this is worse than §5.6 assumes. Because §5.5 drops the Endpoint at every Active→Warm transition, every reconnect after `T_linger` loses resumption, not only those after eviction. A SQLite `StoresServerSessions` needs an upstream hook (proposal §8 item 2): e.g. `Builder::server_session_storage(Arc<dyn StoresServerSessions>)`, or a general `Builder::tls_server_config_hook(FnOnce(&mut rustls::ServerConfig))`. Without it, 0-RTT only works within one Active period.

---

## V10: address-validation token key/store

- The **server token key** is `RustlsTokenKey::new(&mut rand::rng(), …)`, random per bind (`iroh-1.4.0/src/endpoint.rs:238-241`, `tls/misc.rs:27-41`). It's baked into the endpoint-level server config (`socket.rs:256`). `ServerConfigBuilder::set_token_key(Arc<dyn HandshakeTokenKey>)` exists (`endpoint/quic.rs:637-641`, `HandshakeTokenKey` re-exported at `endpoint.rs:109`). However, it only applies via `Incoming::accept_with`, *after* noq has already decoded the Initial's token with the **endpoint-level** server config (`noq-proto-1.4.0/src/endpoint.rs:506-508`). `Endpoint::set_alpns` rebuilds the config with the same random key (`endpoint.rs:982-991`). Net: not persistable through the public API.
- The **client token store** (#4317) is one `TokenMemoryCache` per endpoint (`endpoint.rs:253`), in memory only.
- With `default-features = false`, noq-proto's `bloom` feature is **off**. iroh depends on noq/noq-proto with `default-features = false` (`iroh-1.4.0/Cargo.toml:289-297`), and `bloom` is only in their defaults (`noq-1.4.0/Cargo.toml` `default`, `noq-proto-1.4.0/Cargo.toml` `default`). Then `ValidationTokenConfig` defaults to `NoneTokenLog` with `sent = 0`: the server neither issues NEW_TOKEN nor accepts such tokens (`noq-proto-1.4.0/src/config/mod.rs:519-530`). iroh never calls `retry()` on its own, so there is no Retry RTT. The only cost of unvalidated addresses is the 3× anti-amplification limit during the handshake.

**Design impact:** perf only, as the spec predicted. Accept it. Optionally add `noq-proto/bloom` to get NEW_TOKEN within one Active period.

---

## V13: transport id allocation; CustomAddr serialization

- The id is **u64**, not u16 (`iroh-base-1.4.0/src/endpoint_addr.rs:186-191`, `:342-351`). "Freely chosen"; the registry is `TRANSPORTS.md` in the iroh repo (`endpoint_addr.rs:166-167`). The file isn't shipped in the crate. On GitHub `main` (fetched 2026-10-21) it reads: `0x00-0x1F` reserved, `0x20` Test (iroh test-utils, Ed25519 key), `0x544F52` Tor (Ed25519 key, experimental), `0x424C45` BLE (MAC, reserved). The process is "choose an id and do a PR". iroh's own test transport uses `TEST_TRANSPORT_ID = 0x20` (`iroh-1.4.0/src/test_utils/test_transport.rs:28-30`).
- **Serialization:** binary is `u64` little-endian id followed by raw data (min 8 bytes) (`endpoint_addr.rs:177-181`, `:366-383`). String form is `<hex id>_<hex data>` (`:170-175`, `:193-214`). serde is a struct `{id, data: bytes}` (`:185-191`, `:243-282`). Data is stored inline up to 30 bytes, otherwise on the heap (`:235-241`, `:319-331`). The binary layout is identical in iroh-base 1.0.3 and 1.2.0 (`iroh-base-1.0.3/src/endpoint_addr.rs:323-333`).
- **Design impact:** the spike uses provisional `0x494E4752` ("INGR"). Request a registry entry; the address format is the Ed25519 public key (32 bytes). Because the DO never publishes this address, a collision only matters inside the DO's own process. Fix spec §2.2/§5.3: "u16" should be "u64".

---

## V14: a custom-only endpoint accepting connections; NAT traversal sanity

### Answer: **works.** Measured in `tests/native.rs::v14_accept_with_only_custom_transport`.

The DO endpoint has only ingress. The client was never introduced to the DO beforehand. The DO accepted, echoed, saw `remote_id` = the client's key, and its path list showed `Custom(CustomAddr { id: 0x494E4752, data: <client id> })`.

The accept path, traced:
1. `CustomEndpoint::poll_recv` → `Transports::poll_recv` → `Socket::process_datagrams`, which maps `Addr::Custom(remote)` to a `CustomMappedAddr` (an IPv6 ULA) and writes it into `meta.addr` (`transports.rs:254-275`, `socket.rs:648-659`).
2. noq sees an Initial from an unknown address and calls `handle_first_packet` → `Incoming` (`noq-proto-1.4.0/src/endpoint.rs:244-247`, `:486-540`).
3. `Endpoint::accept()` → `Incoming::accept()` → `Accepting`. The handshake proves the peer's key. `conn_from_noq_conn` → `register_connection(remote_id, conn)` (`endpoint/connection.rs:148-152`, `:317-347`).
4. The `RemoteStateActor` for that id registers PathId 0's 4-tuple (`remote_state.rs:387-452`, `:996-1024`).

Replies: noq transmits to the mapped address → `Sender::poll_send` → `custom_addrs.lookup` → `FourTuple::Custom` → our `CustomSender` (`transports.rs:1236-1258`, `:1027-1036`).

**NAT traversal / holepunch / foreign addresses:**
- Only **client-side** connections holepunch (`remote_state.rs:523-533`). The DO is always the server, so it never initiates rounds.
- The server never opens paths: `open_path_on_conn` returns early for `Side::Server` (`remote_state.rs:1058-1062`). So the peer's advertised relay URL and IPs, whether from QNT ADD_ADDRESS or anything else, are **never dialled** by the DO. The DO has no address lookup configured, so it never resolves the peer either.
- The DO advertises no local candidates: `local_candidates()` comes from direct addrs (`remote_state.rs:1110-1116`), which are empty without IP transports. The peer's rounds fail locally with `NotEnoughAddresses` *if the peer has no local addrs* (`noq-proto-1.4.0/src/n0_nat_traversal.rs:460-462`). Otherwise the peer sends REACH_OUT frames.
- When a peer with IP addresses sends REACH_OUT, the DO (server) handles them (`noq-proto-1.4.0/src/connection/mod.rs:5539-5544`) and noq emits probe datagrams to **raw IP addresses**. In iroh these become `FourTuple::Ip`. On wasm the send returns `Err("IP is unsupported in browser")` (`transports.rs:972-975`), which is swallowed as a dropped datagram (`:1287-1289`). On native with no IP sender it is blackholed (`:1039-1042`). Each round costs at most 9 probes per address (`n0_nat_traversal.rs:32`). The cost is CPU only. **Nothing breaks.**
- From the peer's side the path is a relay path, which it treats as backup/"not good enough". It re-runs holepunch checks every 60 s (`remote_state.rs:66`, `:756-788`). If it has IP addresses, each check sends REACH_OUT frames to the DO, i.e. a few packets per minute per connection, on top of keep-alives.

---

## Keep-alive and idle (affects §5.5): **design blocker as written**

- iroh's default `QuicTransportConfig` sets `keep_alive_interval = 5 s` **and** `default_path_keep_alive_interval = 5 s`, plus path idle 15 s and multipath/QNT on (`iroh-1.4.0/src/endpoint/quic.rs:154-163`, `socket.rs:109`, `:117`).
- `QuicTransportConfigBuilder::keep_alive_interval(Duration)` can only set a value; you can't pass `None` (`quic.rs:366-369`). `default_path_keep_alive_interval` ignores values above 5 s (`quic.rs:514-524`). There's no API to disable either. `QuicTransportConfig` is an opaque newtype (`quic.rs:129-130`), and iroh's `Connection` doesn't expose noq's `set_path_keep_alive_interval` (`noq-proto-1.4.0/src/connection/mod.rs:918-932`).
- noq arms `PathKeepAlive` for every path, including the server's PathId 0 (`noq-proto-1.4.0/src/connection/paths.rs:344`, `connection/mod.rs:3911-3930`), and pings on expiry (`:2567-2569`).
- **Measured** (`tests/native.rs::keepalive_behaviour`, DO `max_idle_timeout` = 8 s, app idle for 25 s):
  - client defaults: connection **still open**; DO sent 10 datagrams, client 10.
  - client *and* DO with connection keep-alive pushed to 24 h: **still open**; DO sent 14, client 13. The per-path keep-alive alone keeps it alive.

**Design impact:** "the DO advertises `max_idle_timeout` and disables its own keepalive" is not achievable with iroh 1.4.0. Even if it were, every stock iroh peer pings every ~5 s. While any connection is open, the DO is woken about every 2.5 s (pings from both sides plus ACKs). QUIC idle timeout effectively **never** closes an idle-but-held connection. To reach Warm, the DO must enforce an application-level idle policy: close connections with no open streams or datagrams for N seconds, using `Connection::close`. The "optional operator cap" in §5.5 becomes **mandatory, default on**. Upstream asks: allow `None` for both keep-alive knobs on iroh's builder, and/or a per-path keep-alive setter on `Connection`.

---

## Builder knobs we need (all exist unless noted)

| Need | API | Evidence |
|---|---|---|
| empty base (no relay, no lookup; no IP on wasm) | `Builder::empty()` / `Endpoint::builder(presets::Empty)` | `endpoint.rs:192-222`, `presets.rs:36-43` |
| crypto provider (mandatory without `tls-ring`) | `.crypto_provider(Arc<CryptoProvider>)` | `endpoint.rs:770-773`, `:235-237` |
| secret key | `.secret_key(SecretKey)` | `endpoint.rs:529-532` |
| ALPNs | `.alpns(Vec<Vec<u8>>)`; runtime `Endpoint::set_alpns` | `endpoint.rs:544-547`, `:982-991` |
| max_idle_timeout | `QuicTransportConfig::builder().max_idle_timeout(Some(IdleTimeout))` → `.transport_config(..)` | `quic.rs:212-215`, `endpoint.rs:678-681` |
| disable keep-alive | **not possible**; workaround: `keep_alive_interval(24h)`; path keep-alive stuck at ≤5 s | `quic.rs:366-369`, `:514-524` |
| no relay | `.relay_mode(RelayMode::Disabled)` / `.clear_relay_transports()` | `endpoint.rs:566-586`, `:515-519` |
| no IP (native only) | `.clear_ip_transports()` | `endpoint.rs:507-512` |
| no address lookup / publishing | don't add any (`empty()`); `.clear_address_lookup()` | `endpoint.rs:594-597` |
| metrics off | crate feature `metrics` off (`default-features = false`) | `Cargo.toml:67-77` |
| portmapper off | not compiled on wasm; `.portmapper_config(PortmapperConfig::Disabled)` natively | `Cargo.toml:432-435`, `endpoint.rs:795-798` |
| net_report off | implicitly inert with empty relay map; `.net_report_config(..)` for features | `socket.rs:795-799`, `endpoint.rs:807-810` |
| custom transport / selector | `.add_custom_transport(..)`, `.path_selector(..)` | `endpoint.rs:821-852` |
| per-connection server config (transport cfg, limits) | `Endpoint::create_server_config_builder` + `Incoming::accept_with` | `endpoint.rs:1772-1775`, `connection.rs:165-172` |
| reset key, CID generator, TLS session store, server token key | **no API** (upstream) | see V8/V9/V10 |

---

## Appendix A: skeleton (`src/lib.rs`, builds native + wasm32)

```rust
use std::{collections::VecDeque, fmt, io, sync::{Arc, Mutex}, task::{Context, Poll, Waker}};
use bytes::Bytes;
use iroh::{Endpoint, EndpointId, SecretKey, endpoint::{Builder, QuicTransportConfig, transports::{
    Addr, CustomEndpoint, CustomSender, CustomTransport, PathSelection, PathSelectionContext,
    PathSelector, RecvInfo, Transmit}}};
use iroh_base::CustomAddr;

pub const INGRESS_TRANSPORT_ID: u64 = 0x494E_4752; // "INGR", provisional (register in TRANSPORTS.md)
const INBOX_CAP: usize = 1024;

pub fn peer_addr(peer: &EndpointId) -> CustomAddr { CustomAddr::from_parts(INGRESS_TRANSPORT_ID, peer.as_bytes()) }
pub fn parse_peer(addr: &CustomAddr) -> io::Result<EndpointId> {
    if addr.id() != INGRESS_TRANSPORT_ID { return Err(io::Error::other("not an ingress addr")); }
    let b: &[u8; 32] = addr.data().try_into().map_err(|_| io::Error::other("bad len"))?;
    EndpointId::from_bytes(b).map_err(|_| io::Error::other("bad endpoint id"))
}

/// Outgoing datagrams → peer's hibernatable WebSocket. Must not block, must not dial.
pub trait IngressSink: fmt::Debug + Send + Sync + 'static {
    fn send(&self, peer: &EndpointId, datagrams: &[u8], segment_size: Option<usize>) -> io::Result<()>;
}

#[derive(Debug, Default)]
struct Inbox { queue: VecDeque<(EndpointId, Bytes)>, waker: Option<Waker>, dropped: u64 }

#[derive(Debug, Clone)]
pub struct Ingress { inbox: Arc<Mutex<Inbox>>, sink: Arc<dyn IngressSink>, local: n0_watcher::Watchable<Vec<CustomAddr>> }

impl Ingress {
    pub fn new(local_id: EndpointId, sink: Arc<dyn IngressSink>) -> Self {
        Self { inbox: Default::default(), sink, local: n0_watcher::Watchable::new(vec![peer_addr(&local_id)]) }
    }
    /// From webSocketMessage: one call per datagram of a relay datagram batch.
    pub fn push(&self, from: EndpointId, datagram: Bytes) {
        let mut i = self.inbox.lock().expect("poisoned");
        if i.queue.len() >= INBOX_CAP { i.dropped += 1; return; }
        i.queue.push_back((from, datagram));
        if let Some(w) = i.waker.take() { w.wake(); }
    }
}

impl CustomTransport for Ingress {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> { Ok(Box::new(IngressEndpoint(self.clone()))) }
}

#[derive(Debug)] struct IngressEndpoint(Ingress);
impl CustomEndpoint for IngressEndpoint {
    fn watch_local_addrs(&self) -> n0_watcher::Direct<Vec<CustomAddr>> { self.0.local.watch() }
    fn create_sender(&self) -> Arc<dyn CustomSender> { Arc::new(IngressSender(self.0.sink.clone())) }
    fn poll_recv(&mut self, cx: &mut Context, bufs: &mut [io::IoSliceMut<'_>],
                 metas: &mut [noq_udp::RecvMeta], recv_infos: &mut [RecvInfo]) -> Poll<io::Result<usize>> {
        let mut inbox = self.0.inbox.lock().expect("poisoned");
        let mut n = 0;
        while n < bufs.len() {
            let Some((from, d)) = inbox.queue.pop_front() else { break };
            if d.len() > bufs[n].len() { inbox.dropped += 1; continue; } // > 1472: drop
            bufs[n][..d.len()].copy_from_slice(&d);
            metas[n].len = d.len(); metas[n].stride = d.len(); metas[n].ecn = None;
            recv_infos[n] = RecvInfo::new(peer_addr(&from), None);
            n += 1;
        }
        if n > 0 { Poll::Ready(Ok(n)) } else { inbox.waker = Some(cx.waker().clone()); Poll::Pending }
    }
}

#[derive(Debug)] struct IngressSender(Arc<dyn IngressSink>);
impl CustomSender for IngressSender {
    fn is_valid_send_addr(&self, a: &CustomAddr) -> bool { a.id() == INGRESS_TRANSPORT_ID }
    fn poll_send(&self, _cx: &mut Context, dst: &CustomAddr, _src: Option<&CustomAddr>, t: &Transmit<'_>)
        -> Poll<io::Result<()>> {
        // never Pending; Err == "no route" (iroh logs + treats as loss)
        Poll::Ready(parse_peer(dst).and_then(|p| self.0.send(&p, t.contents, t.segment_size)))
    }
}

#[derive(Debug)] pub struct OnlyIngress;
impl PathSelector for OnlyIngress {
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        let mut sel = PathSelection::none();
        if let Some(p) = ctx.paths().find(|p| matches!(p.network_path().remote(),
            Addr::Custom(c) if c.id() == INGRESS_TRANSPORT_ID)) { sel.set(&p); }
        sel
    }
}

pub fn do_transport_config(idle: std::time::Duration) -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .max_idle_timeout(Some(idle.try_into().expect("fits")))
        .keep_alive_interval(std::time::Duration::from_secs(24 * 3600)) // "off"; must stay < 2^31 ms
        .build()
}

pub fn do_builder(secret: SecretKey, ingress: Ingress, alpns: Vec<Vec<u8>>) -> Builder {
    let b = Builder::empty()
        .secret_key(secret)
        .alpns(alpns)
        .crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .transport_config(do_transport_config(std::time::Duration::from_secs(30)))
        .add_custom_transport(Arc::new(ingress))
        .path_selector(Arc::new(OnlyIngress))
        .clear_relay_transports();
    #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
    let b = b.clear_ip_transports();
    b
}
```

`Cargo.toml` essentials:

```toml
iroh = { version = "=1.4.0", default-features = false, features = ["unstable-custom-transports"] }
iroh-base = { version = "=1.4.0", default-features = false, features = ["key"] }
noq-udp = { version = "=1.4.0", default-features = false }
n0-watcher = "1"
rustls = { version = "0.23.46", default-features = false, features = ["ring", "std"] }
bytes = "1"
[target.'cfg(all(target_family = "wasm", target_os = "unknown"))'.dependencies]
ring = { version = "0.17", features = ["wasm32_unknown_unknown_js"] }
wasm-bindgen = "0.2"
wasm-bindgen-futures = "0.4"
```

The wasm glue in the spike (`mod wasm` in `src/lib.rs`) exports `DoEndpoint::{bind, push, drain_outbox, accept_echo, close}`. It uses a thread-local outbox, because JS handles aren't `Send`. In the real DO, the glue should flush the outbox (or invoke a JS callback) after every turn, *including* turns triggered by noq timers, not just after `webSocketMessage`.

## Appendix B: test results (native, `cargo test --test native -- --nocapture`)

```
DO saw remote_id=fd1724385a paths=["Custom(CustomAddr { id: 1229866834, data: [fd17…f618] })"]
V9 #1 fresh: no ticket -> 1-RTT
V9 #2 same DO instance: 0-RTT ACCEPTED
V9 #3 after DO endpoint re-bind: 0-RTT REJECTED
V8: client conn ended after 9.990046416s: TimedOut            (idle timeout 10 s → no stateless reset)
[client defaults] after 25s idle (idle_timeout=8s): closed=None, DO sent 10 dgrams, client sent 10
[client keepalive=24h] after 25s idle (idle_timeout=8s): closed=None, DO sent 14 dgrams, client sent 13
```

wasm in node (`out/smoke.mjs`, setTimeout/clearTimeout wrapped):

```
bound; live timers: [ 250, 24984, 14998 ]
after close+drop; live timers: [] total created 6
```

## Open items

- Run the wasm build inside workerd/miniflare: bind, a handshake driven by a real iroh client through the ingress, then close, and check that the DO is hibernatable afterwards. Node lacks `globalThis.addEventListener`; workerd has it, so the netwatch listeners will actually register there.
- Native tests didn't exercise a peer with real IP candidates sending REACH_OUT (analysed from code only, see V14).
