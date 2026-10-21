# Phase 0 verification: summary

Date: 2026-10-21. Details and citations live in `docs/verification/*.md`;
design changes in `docs/design-notes/`.

## Pins

| Component | Version |
|---|---|
| iroh, iroh-base, iroh-relay, iroh-dns | 1.4.0 |
| noq, noq-proto | 1.4.0 |
| workers-rs (`worker`), `worker-build` | 0.8.7 |
| Rust | 1.93.0 |
| wrangler (local dev only) | 4.130.0 |
| OpenTofu / cloudflare provider | 1.13.1 / 5.28.1 |

## Answers

| Item | Answer | Impact | Detail |
|---|---|---|---|
| V1 | Path **not** preserved (`set_path("/relay")`); query kept | Hostname routing mandatory (note 001) | relay-protocol |
| V2 | `Sec-WebSocket-Protocol: iroh-relay-v2, iroh-relay-v1`; 101 must echo one | `on_upgrade` | relay-protocol |
| V3 | **Yes**: challenge path completes even after the client sent the exporter header (empirical) | No blocker | relay-protocol |
| V4 | Frames 0–13; Health v1-only, Status v2-only; no peer-gone for unknown dst; challenge is exactly 16 bytes | Codec; drop silently | relay-protocol §8 |
| V5 | Lazy connect on first send; client closes non-home socket 60 s after its last send; eager reconnect on server close; `Restarting` ignored | DO never closes idle sockets; Warm lasts ≤ 60 s after peer's last send | relay-protocol |
| V6 | Client pings every 15 s; pong deadline can be ~500 ms | Hibernated DO must pong fast: measure wake latency | relay-protocol |
| V7 | Builds for wasm32 with only a custom transport; 951 KB gzip after `wasm-opt -Oz`; 0 timers after close+drop (node) | Needs ring provider and **llvm-ar** on macOS | iroh-endpoint |
| V8 | **No**: reset key and CID-check key random per bind, not injectable; restarted endpoint drops old-CID packets silently | Abrupt eviction costs a peer idle timeout; upstream patch (Phase 4) | iroh-endpoint |
| V9 | **No** session-store hook; in-memory store with 0-RTT, lost on every re-bind | 0-RTT lost on every Active→Warm; upstream patch (Phase 4) | iroh-endpoint |
| V10 | Token key random, not settable; NEW_TOKEN off without `bloom` | Perf only | iroh-endpoint |
| V11 | workers-rs 0.8.7 covers DO, SQLite, hibernatable WS, attachments, alarms, subprotocol; **RPC fails** | Router→DO via `stub.fetch` (note 002) | cloudflare |
| V12 | Local clock **not** frozen; local wrangler hibernates after ~10 s; `evictDurableObject` forces it | Tests don't assume frozen clock | cloudflare |
| V13 | Transport id is **u64**; 0x00–0x1F reserved | Provisional id `0x494E4752` | iroh-endpoint |
| V14 | **Yes**: custom-only endpoint accepts unknown peers; never holepunches; ignores advertised addrs | No blocker | iroh-endpoint |
| V15 | `*.iroh-do.<zone>` needs ACM | Chosen (note 001) | cloudflare |
| V16 | Stock client resolves via `dns.iroh.link` pkarr GET + DNS; iroh-dns builds the record on wasm; round trip < 1 s | Publish at mint; z32 ids only (note 004) | discovery |
| V17 | Provider 5.28.1 uploads wasm modules, DO bindings, sqlite migrations | No wrangler fallback | cloudflare |
| V18 | `http://<E>.localhost:8787/` works natively (hickory synthesizes `*.localhost`); wrangler keeps Host | Local mode needs no TLS | relay-protocol, cloudflare |
| V19 | Deleting the script deletes DO data | `just down` warns, confirms | cloudflare |
| V20 | **Yes**: DO can dial a peer over the peer's existing inbound socket; window ≤ 60 s after the peer's last send | Dial-back as designed (note 003) | relay-protocol |

## Design changes

- 001: hostname URLs with ACM (path form impossible).
- 002: router→DO via fetch; constructor rebuilds from storage; bundle hash binding; destroy deletes identities.
- 003: bidirectional chat and DO-initiated dial-back in scope.
- 004: z32 ids only; publish at mint; 24 h republish.
- 005: app-level idle close; Phase 4 upstream patches for V8/V9.
