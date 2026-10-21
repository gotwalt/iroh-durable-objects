# Phase 0: discovery (V16) and prior art

Status: **V16 verified (positive)**, with one hard rule (z-base-32 everywhere) and two risks (shared-egress rate limiting, negative DNS caching).
Date: 2026-10-21. Pinned sources: iroh 1.4.0, iroh-dns 1.4.0, iroh-base 1.4.0 (local cargo registry, cited as `crate-version/path:line`).
Experiment crate and logs, not kept in the repository: `pkarrcheck`, `roundtrip-run1.txt`, `update-run.txt`.

## Summary

| Question | Answer |
|---|---|
| How does a stock client resolve an EndpointId? | `presets::N0` installs three address lookups: `PkarrPublisher` (publishes), `PkarrResolver` (HTTPS GET `https://dns.iroh.link/pkarr/<z32>`), and, outside `wasm_browser`, `DnsAddressLookup` (TXT `_iroh.<z32>.dns.iroh.link.` through the system resolver). Resolution queries **both** and merges results. |
| Record format | pkarr signed packet: `<32B pubkey><64B ed25519 sig><8B BE µs timestamp><DNS wire packet ≤1000B>`. The DNS packet holds TXT RRs named `_iroh.<z32>` with values `relay=<url>`, `addr=<ip:port or custom>`, `user-data=<s>`. The default filter publishes **relay only**. |
| Publisher | `PUT https://dns.iroh.link/pkarr/<z32>`, body = signed packet minus the 32-byte pubkey (`to_relay_payload`). No required headers. Success is `204`. TTL = 30 s; republish every 5 min; on failure, retry after `n` seconds for the n-th consecutive failure. |
| Standalone on wasm32? | **Yes.** `iroh-dns` + `iroh-base` (no `iroh`) give `EndpointInfo::to_pkarr_signed_packet` / `SignedPacket::to_relay_payload`, the same code path the `PkarrPublisher` uses. `cargo check` and `cargo build --release --target wasm32-unknown-unknown` succeed with **no RUSTFLAGS**. |
| Real round trip | PUT → 204 in 0.74 s. Readable via the pkarr GET, DNS TXT (1.1.1.1, 8.8.8.8, ns1) and a stock `presets::N0` iroh Endpoint, effectively immediately (< 1 s). Caveat: see negative caching below. |
| Canonical text form of EndpointId | `Display`/`to_string()` = **64-char lowercase hex** (too long for a DNS label). `to_z32()` = **52-char z-base-32**, used by pkarr and DNS. **`FromStr` does NOT accept z-base-32**: it accepts hex, or RFC 4648 base32 (also 52 chars). Use `PublicKey::from_z32` for hostname labels. |
| Direct dial without discovery | `EndpointAddr::new(id).with_relay_url(url)` passed to `Endpoint::connect`. Supported. Discovery still runs concurrently but isn't awaited. |
| Prior art | `HeddleCo/weft` is **private / not resolvable** with our credentials. `HeddleCo/heddle` (Apache-2.0), the `HeddleCo/iroh` fork (MIT OR Apache-2.0) and `tmc/go-iroh` (MIT) are public and useful; details below. |

---

## 1. V16: discovery

### 1.1 What a stock client does (default features, `presets::N0`)

- `presets::N0` applies `Minimal`, then adds `PkarrPublisher::n0_dns()` (`iroh-1.4.0/src/endpoint/presets.rs:125`), `PkarrResolver::n0_dns()` (`:128`) and, under `#[cfg(not(wasm_browser))]`, `DnsAddressLookup::n0_dns()` (`:133`). `wasm_browser` = `all(target_family="wasm", target_os="unknown")` (`iroh-1.4.0/build.rs:7`), so **inside a Worker iroh would use only the HTTP pkarr resolver**. That doesn't affect us, because we publish standalone.
- Endpoints:
  - `N0_DNS_PKARR_RELAY_PROD = "https://dns.iroh.link/pkarr"` (`iroh-1.4.0/src/address_lookup/pkarr.rs:127`).
  - Staging is `https://staging-dns.iroh.link/pkarr` (`:134`), selected when `IROH_FORCE_STAGING_RELAYS` is non-empty (`iroh-1.4.0/src/endpoint.rs:2047-2049`).
  - The DNS origin is `N0_DNS_ENDPOINT_ORIGIN_PROD = "dns.iroh.link."` (`iroh-dns-1.4.0/src/dns.rs:36`).
- **DNS lookup:**
  - Name: `format!("_iroh.{}.{}", endpoint_id.to_z32(), origin)` (`iroh-dns-1.4.0/src/dns.rs:1062`).
  - Resolver: the endpoint's resolver, which defaults to system nameservers (`iroh-dns-1.4.0/src/dns.rs:748-749`).
  - Retries are staggered at 200/300/600/1000/2000/3000 ms, with a 3 s per-query timeout (`iroh-1.4.0/src/address_lookup/dns.rs:22`).
- **Pkarr lookup:** `GET <relay>/<z32>`, then `SignedPacket::from_relay_payload` (signature verified) and `EndpointInfo::from_pkarr_signed_packet` (`iroh-1.4.0/src/address_lookup/pkarr.rs:660-694`).
- Both lookups run concurrently. `AddressLookupServices::resolve` merges the streams and yields every item (`iroh-1.4.0/src/address_lookup.rs:565-579`). The observed run yielded `[dns]` first, then `[pkarr]`.

### 1.2 Record format (what the publisher produces)

- Name and attributes:
  - TXT name constant `IROH_TXT_NAME = "_iroh"` (`iroh-dns-1.4.0/src/attrs.rs:20`).
  - Attributes `relay`, `addr`, `user-data` (kebab-case enum, `attrs.rs:78-89`).
  - Encoding: `Relay(url) → relay=<url.to_string()>`, `Ip → addr=<sockaddr>`, `Custom → addr=<CustomAddr Display>` (`iroh-dns-1.4.0/src/endpoint_info.rs:486-499`). Values are one TXT RR per `key=value` (`attrs.rs:172-191`). Module docs: `endpoint_info.rs:13-33`.
- `RelayUrl` is stored verbatim; there is no trailing-dot or path canonicalization (`iroh-base-1.4.0/src/relay_url.rs:23-45`). Publish the exact string a client should dial.
- **Parser gotcha:** `TxtAttrs::from_strings` uses `s.split('=')` and keeps only the first two parts (`attrs.rs:122-125`). **A relay URL containing `=` (e.g. a query string) is silently truncated.** Keep relay URLs free of `=`; the planned `https://<z32>.<zone>/` is fine.
- Signed packet (`iroh-dns-1.4.0/src/pkarr.rs`):
  - Wire format: `<32 pubkey><64 sig><8 BE timestamp µs><DNS packet>` (`:3`, `:26-30`).
  - The DNS packet is `Packet::new_reply(0)`. Its answers are TXT RRs with name `_iroh.<z32>` (normalized against the z32 origin, `:298-317`), class IN and the given TTL. It is built with name compression and must be ≤ 1000 bytes (`:44-88`).
  - Signature: Ed25519 over the bencode-style `"3:seqi{ts}e1:v{len}:" ‖ packet` (`:291-295`).
  - Timestamp: `Timestamp::now()` = `n0_future::time::SystemTime` µs, forced strictly monotonic per process (`:341-362`). On wasm this is `Date.now()` via web-time. In a DO, the frozen clock is fine, since it advances at I/O and the per-isolate atomic guarantees strict increase.
- Our record (`relay=https://<z32>.e.example.invalid/`, 52-char label) produced a **240-byte relay payload**.

### 1.3 Publisher endpoint, method, cadence

- `PkarrRelayClient::publish`: `PUT {pkarr_relay}/{public_key.to_z32()}` with body `signed_packet.to_relay_payload()` (= bytes `[32..]`, i.e. sig ‖ ts ‖ dns). There are **no custom headers** and no Content-Type (`iroh-1.4.0/src/address_lookup/pkarr.rs:697-722`). The HTTP client is plain reqwest with only TLS/DNS customised (`iroh-1.4.0/src/util.rs:10-24`).
- Server side (n0 `iroh-dns-server`, `n0-computer/iroh@efcb1c3f`, `iroh-dns-server/src/http/pkarr.rs:14-35`):
  - Parses the key with `PublicKey::from_z32`, verifies with `SignedPacket::from_relay_payload`, upserts, and returns `204 No Content`.
  - A bad signature returns `400 {"detail":"invalid body payload: Invalid signature"}` (observed).
  - A **stale (older-timestamp) packet also returns 204 but is not stored** (observed: after PUT B then PUT stale A, the server still served B). So a 204 doesn't prove the record took effect.
  - `GET` returns `content-type: application/x-pkarr-signed-packet` with `access-control-allow-origin: *`.
- TTL and republish:
  - `DEFAULT_PKARR_TTL = 30` s (`pkarr.rs:143`). ns1.iroh.link serves the TXT with TTL 30.
  - `DEFAULT_REPUBLISH_INTERVAL = 5 min` (`pkarr.rs:146`). The republish loop is at `pkarr.rs:388-429`; retry backoff is `failed_attempts` seconds (`:399-402`).
  - The default filter is `AddrFilter::relay_only()` (`pkarr.rs:171`).
- Server retention: the default store eviction is **7 days after the packet timestamp**, checked every 10 s (`iroh-dns-server/src/store/signed_packets.rs:98-100`). n0's production setting is unknown. A 5-minute republish is iroh's default, not a server requirement. **Recommendation:** republish from the DO alarm at ≤ 24 h and on any change. A cadence anywhere from 5 min to hours is safe against the 7-day default. The spec's "TTL cadence" wording should change to "republish interval".
- **RISK: rate limiting.** `pkarr_put_rate_limit` defaults to `Simple`, keyed on peer IP, with `per_second(4)` and `burst_size(2)` (`iroh-dns-server/src/http/rate_limiting.rs:59-67`, `config.rs:62,289`). In tower_governor, that means one token every 4 s and a burst of 2, **per source IP**. Worker `fetch` egress uses shared Cloudflare IPs, so many DOs (and unrelated Workers) republishing to dns.iroh.link may get 429s. Whether n0 prod runs `Smart` (X-Forwarded-For) or `Simple` behind a proxy is unknown. I did not probe this, to avoid hammering prod. Mitigations:
  1. Long republish intervals with jitter.
  2. Treat 429 as "retry at the next alarm".
  3. Self-host a pkarr relay or iroh-dns-server, e.g. n0's `iroh-discovery-cloudflare-worker` (§5.9). Stock clients only consult dns.iroh.link, though, so self-hosting breaks P3 unless that server forwards to n0.

  This needs a Phase 1 measurement from a deployed Worker.

### 1.4 Standalone construction on wasm32: proof

Crate: `pkarrcheck` (throwaway, not kept) (`Cargo.toml`, `src/lib.rs`).

```toml
iroh-base = { version = "=1.4.0", default-features = false, features = ["key", "relay"] }
iroh-dns  = { version = "=1.4.0", default-features = false }
```

```rust
EndpointInfo::new(secret.public())
    .with_relay_url(relay)
    .to_pkarr_signed_packet(&secret, 30)   // identical call to PkarrPublisher (pkarr.rs:437-440)
// PUT https://dns.iroh.link/pkarr/{secret.public().to_z32()}  body = pkt.to_relay_payload()
```

- `cargo check --target wasm32-unknown-unknown --lib`: **OK**.
- `cargo build --release --target wasm32-unknown-unknown`: **OK**, with **no `RUSTFLAGS`/`getrandom_backend` cfg needed**. iroh-base already enables `getrandom 0.4` with `wasm_js` on wasm32 (`iroh-base-1.4.0/Cargo.toml:141-144`).
- iroh-dns gates its `dns` module (hickory/n0-dns-resolver, rustls) off on wasm (`iroh-dns-1.4.0/src/lib.rs:9-10`, `Cargo.toml:124-133`). `pkarr`/`endpoint_info` are unconditional.
- Uses wasm-bindgen/js-sys (web-time `Date.now`, `crypto.getRandomValues`), which workers-rs provides. The `pkarr` crate from crates.io is **not** used; iroh 1.x has its own implementation in iroh-dns.
- Size (cdylib, release, no wasm-opt, no LTO tuning): 974,247 B raw, 282,320 B gzip -9. In the real Worker it shares url/idna/ed25519 with iroh, so the marginal cost is small.
- Publishing is a single `fetch(PUT)` with a ≤ 1104-byte body. No streaming and no special headers.

### 1.5 Native round trip against production (2026-10-21)

`cargo run --bin roundtrip` (`src/bin/roundtrip.rs`); output in `roundtrip-run1.txt` (throwaway, not kept):

```
EndpointId Display  : 013e6477…9c3d (len 64)
EndpointId to_z32   : yr9ge793ppirfkkm4edbyu56mbnctp4i9xsf3468xsda9o8cuo6o (len 52)
FromStr(z32)        : Err("failed to decode base32 string")
PUT https://dns.iroh.link/pkarr/yr9ge…6o -> 204 No Content in 738ms
GET  -> 200 OK, ["relay=https://yr9ge…6o.e.example.invalid/"] after 914ms
DNS (system resolver) _iroh.yr9ge…6o.dns.iroh.link -> [relay=…] after 1.097s
stock iroh Endpoint lookup [dns]   -> EndpointAddr{ Relay(https://yr9ge…6o.e.example.invalid/) } in 12ms
stock iroh Endpoint lookup [pkarr] -> EndpointAddr{ Relay(https://yr9ge…6o.e.example.invalid/) } in 725ms
```

**Propagation delay: effectively zero.** The record was readable on the first GET and the first DNS query right after the 204, and the authoritative ns1.iroh.link answers from the store directly.

**Updates (`src/bin/update.rs`, `update-run.txt`):**
- The authoritative server reflects the new record immediately.
- Recursive resolvers may serve the old record for up to the 30 s TTL.

**RISK: negative caching.**
- dns.iroh.link's SOA is `dns.iroh.link. hostmaster.iroh.link. 1 7200 900 1209600 0` (MINIMUM 0, SOA TTL 1800).
- RFC 2308 compliant resolvers (1.1.1.1, 8.8.8.8) don't negatively cache. My LAN router (192.168.7.1) and the Tailscale resolver it fronts cached NXDOMAIN for about 1800 s, the SOA *record* TTL.
- So a client whose resolver saw the name before the first publish can fail the `[dns]` lookup for up to 30 min.

**This is not a blocker for stock clients:**
- `PkarrResolver` (HTTPS) runs in parallel and is unaffected.
- Verified: for the negatively cached id `3fgdccmek5bio88d59bcqrqhmi1rkcdti3i5ctbu5o16jywfi4ao`, a stock `presets::N0` endpoint got `[pkarr] Relay(…b.example.invalid/)` at 762 ms, and `dns` failed at 2.8 s.
- Rule: **publish before handing out an EndpointId**, i.e. at mint time (§13.4).

### 1.6 EndpointId text form

- `pub type EndpointId = PublicKey` (`iroh-base-1.4.0/src/key.rs:70`).
- `Display` = `HEXLOWER` (64 chars) (`key.rs:221-224`).
- `to_z32()` / `from_z32()` use the alphabet `ybndrfg8ejkmcpqxot1uwisza345h769`, 52 chars (`key.rs:20-22,164-174`).
- `FromStr` (`key.rs:249-256`) calls `decode_base32_hex` (`key.rs:475-494`): 64 chars are parsed as hex; anything else is **upper-cased and decoded as RFC 4648 `BASE32_NOPAD`**, not z-base-32.

Consequences for the design:
- **DNS labels must use `to_z32()`.** Hex is 64 octets, over the 63-octet label limit. z32 is 52 octets, lowercase, LDH-safe.
- **Parse hostname labels with `PublicKey::from_z32`, never `.parse::<EndpointId>()`.** Most z32 strings contain `1`, `8` or `9` and fail RFC 4648 decoding. One without those digits (roughly 0.6 % of keys, (29/32)^52) **decodes silently to a different key**, or to an invalid point. That's a correctness/security bug waiting to happen.
- The CLI should accept hex (what `iroh` prints) and z32 explicitly: try `from_z32` when the input is 52 chars of the z32 alphabet, otherwise `FromStr`.
- Spec §5.1 / §13 hostnames `https://<E>.<zone>/` should say `<z32(E)>`.

## 2. Dialing with an explicit relay URL (demo `--relay-url`)

```rust
let addr = iroh::EndpointAddr::new(endpoint_id).with_relay_url(relay_url.parse::<iroh::RelayUrl>()?);
let conn = endpoint.connect(addr, ALPN).await?;   // or connect_with_opts(...) for 0-RTT
```

- APIs: `EndpointAddr::new` (`iroh-base-1.4.0/src/endpoint_addr.rs:96`), `with_relay_url` (`:112`), `From<EndpointId>` (`:155`). `Endpoint::connect(impl Into<EndpointAddr>, alpn)` is at `iroh-1.4.0/src/endpoint.rs:1066`.
- The supplied addresses are inserted as `Source::App` paths, and the resolve request completes immediately when paths exist (`iroh-1.4.0/src/socket/remote_map/remote_state.rs:870-880`; `remote_state/path_state.rs:161-167`).
- Address lookup is still triggered when no path is selected yet (`remote_state.rs:886-891`). It runs alongside and only adds paths, so it's harmless. No discovery is required.
- To guarantee zero discovery traffic, build with `presets::Minimal` + `relay_mode(default_relay_mode())`. Not recommended for the demo, since the point is to use stock N0 defaults.

## 3. Prior art

### 3.1 HeddleCo/weft: **unavailable (private)**

- `gh repo view HeddleCo/weft` and `gh issue view 1274 -R HeddleCo/weft` both fail with "Could not resolve to a Repository". `crates/heddle-iroh-object-provider` can't be read, and its license is **unknown**. Treat it as not reusable until access and license are confirmed.
- Everything below comes from public HeddleCo material that describes it.
- **HeddleCo/heddle#1333** (public, open; repo license Apache-2.0) says:
  - `websocket_transport.rs` is 982 lines with no `wasm_bindgen`/`web_sys`/`js_sys`/`worker::` references, written against `CustomTransport`/`CustomSender`/`PathSelector`.
  - `workerd_clock.rs` is 374 lines: a host-driven `QuicTimeSource` fed via `observe_host_time`/`next_timer`/`fire_timer`.
  - Weft relays browser↔daemon traffic; "browsers cannot hole-punch UDP".
- **Bundle size** of the Weft DO Worker wasm, from `HeddleCo/heddle/docs/security/apex/p0-report.md:97-98,178`. This includes biscuit-auth and the rest of their app.

  | Build | Raw | gzip -9 | brotli 11 |
  |---|---|---|---|
  | current | 6,078,120 B | 2,077,570 B | 1,389,320 B |
  | slimmer biscuit build | 5,358,552 B | 1,846,537 B | — |

  That fits the 10 MB paid Workers limit but not the 3 MB free tier when gzipped. Toolchain was rustc 1.98, wasm-bindgen 0.2.127 (`p0-report.md:23`).
- **ADR 0049** (`HeddleCo/heddle/docs/adr/0049-iroh-hosted-transport-and-weft-browser-relay.md`): Weft runs **its own relay deployment** plus a separate application endpoint. Bootstrap is a **signed HTTPS endpoint descriptor** (not pkarr), and relay admission uses a bootstrap token. 0-RTT is limited to READ_ONLY operations. This confirms the spec's §2.3 reading: no stock-peer pkarr interop.

### 3.2 HeddleCo/iroh fork (`heddleco-iroh` 0.1.0, MIT OR Apache-2.0): directly relevant

Diff `n0-computer/iroh main...HeddleCo:iroh main`, commit `3c0e01bb`, based on upstream iroh **1.0.3**. It adds two unstable features that are **not in upstream 1.4.0**:

1. **`unstable-custom-runtime`: `Builder::quic_time_source(Arc<dyn QuicTimeSource>)`**, where `QuicTimeSource { fn now() -> Instant; fn new_timer(deadline) -> Pin<Box<dyn noq::AsyncTimer>> }`.
   - It overrides `noq::Runtime::now`/`new_timer` in `iroh/src/runtime.rs`.
   - Upstream 1.4.0 `runtime.rs:68-73` hard-wires `new_timer` to tokio or `n0_future::time::sleep_until` (i.e. `setTimeout` on wasm), with no `now()` override.
   - **This is how they drive time in a DO.** The host feeds the clock, reads `next_timer`, and calls `fire_timer` from alarms/events instead of keeping `setTimeout` pending.
   - It's strong evidence for V7's concern: without such a hook, pending noq timers mean pending `setTimeout`, which blocks hibernation. It also gives an upstream proposal shape for §8.
2. **`unstable-custom-transport-backpressure`**: `CustomSender::poll_send` returning `Pending` is propagated to noq, which retains the transmit, instead of upstream's "treat Pending as lost datagram" (`transports.rs`, `Sender::poll_send`).
   - Pitfall they document: this is **connection-wide**, since a blocked transmit blocks all paths of that connection.
   - Relevant if our DO-side WebSocket sender ever backs up.

Also noted: `HeddleCo/noq` is a plain fork with no extra commits on main (behind by 107).

### 3.3 HeddleCo/heddle `provider_transport.rs` (Apache-2.0): client-side WebSocket `CustomTransport`

`crates/hosted-client/src/hosted_runtime/hosted/provider_transport.rs` (590 lines, native/tokio-tungstenite):
- Transport id `0x6864_646c_6577_7301` ("hddlews\x01"). **Note: `CustomAddr` ids are `u64` in 1.4.0** (`iroh-base-1.4.0/src/endpoint_addr.rs:335`), not `u16` as spec §2.2 says. TRANSPORTS.md lists ids like `0x544F52`.
- Each `CustomAddr` data is a random 16-byte "lane handle" mapped to a `wss://…/direct?ticket=…` URL.
- **One WebSocket binary message = one raw QUIC datagram.** There's no iroh-relay framing, so this is bespoke and not stock-interop.
- The lane is lazily connected on the first outgoing packet. Backpressure comes from a bounded mpsc (depth 64) where `Full` maps to `Pending` (pairs with their fork's backpressure feature).
- Close/error surfaces as an `io::Error` on `poll_recv`. WS ping is answered with pong. URL validation requires `wss`, no userinfo.
- Useful as a template for our `CustomSender`/`CustomEndpoint` shape. Our server side differs because we speak the iroh relay protocol.

### 3.4 tmc/go-iroh `internal/relayproto` (MIT): second reference implementation

- `https://github.com/tmc/go-iroh` (public, MIT, last push 2026-10-01) is a clean-room Go port that claims byte-for-byte wire compatibility with golden snapshot tests.
- `internal/relayproto` ports `iroh-relay/src/protos`:
  - Frame types 0–13: ServerChallenge, ClientAuth, ServerConfirmsAuth, ServerDeniesAuth, ClientToRelayDatagram(+Batch), RelayToClientDatagram(+Batch), EndpointGone, Ping, Pong, Health, Restarting, Status (`frame.go`).
  - `MaxPacketSize = 64 KiB`, `PerClientSendQueueDepth = 512`.
  - Handshake: domain separation strings, the `x-iroh-relay-client-auth-v1` header (base64url-no-pad postcard of `{pubkey, sig, 16-byte TLS-exporter suffix}`), and the challenge path (`handshake.go`).
- It also ships `relayserver`, `dnsserver`/`cmd/iroh-dns-server` (pkarr PUT/GET + DNS) and a `cmd/wasmrelaytest` js/wasm relay-only client.
- Useful as an independent oracle for V2–V4 conformance tests and as a self-hostable pkarr server for local mode. As a MIT clean-room port, it's safe to read and cite.

## 4. Implications for the spec

1. §5.9 is confirmed. Use `iroh-dns` (`EndpointInfo::to_pkarr_signed_packet`) in the DO and `fetch(PUT https://dns.iroh.link/pkarr/<z32>, body)`. Don't use the `pkarr` crate.
   - Republish on change and from an alarm at a jittered interval, ≤ 24 h. The default iroh client uses 5 min; the server evicts after a default of 7 days.
   - Treat 4xx other than 429 as permanent and 429/5xx as retry-next-alarm.
   - After publishing, verify via `GET`, because stale PUTs also return 204.
2. Hostnames, CLI parsing and all DNS labels use **z32**. Add a lint or test that no code path calls `EndpointId::from_str` on a hostname label.
3. Publish at mint time (before the id is shared) to avoid negative-cache windows.
4. Add risk: dns.iroh.link per-IP PUT rate limit vs shared Cloudflare egress. Measure in Phase 1.
5. Correct §2.2: custom transport ids are `u64`.
6. V7/V8 upstream item: a `QuicTimeSource`-style hook (HeddleCo fork precedent) is the likely shape for host-driven timers in a hibernating DO.
