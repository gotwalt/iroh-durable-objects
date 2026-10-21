# 006: Crate boundaries and demo application protocols

**Status:** accepted (2026-10-21). Amends §11 and §13.5.

## Crates

| Crate | Target | Owns |
|---|---|---|
| `iroh-relay-ingress` | any (no tokio, no hyper) | §5.2 sans-io server side of the relay protocol: upgrade, challenge, frames, dst filter, ping/pong, snapshot/restore. Upstream candidate. |
| `iroh-ingress-transport` | any | §5.3 `CustomTransport` whose paths are peer ids; fed by `IngressSession` outputs; writes frames through a `FrameSink` the host provides. |
| `iroh-do-core` | any | Runtime-agnostic node: lifecycle (§5.5, note 005), Endpoint build/teardown, pre-activation queue, the demo ALPN handlers below, dial-back. Driven by explicit events from a host; natively testable. |
| `iroh-do` | wasm32 (workers-rs) | The one Worker script: router `fetch` + `IrohEndpoint` DO class, hibernatable sockets, attachments, SQLite identity, mint, pkarr publish, alarms. Thin glue over the three crates above. (§11's separate `iroh-do-router` is folded in: one script, §13.2.) |
| `iroh-do-demo` | native | Peer CLI on stock iroh 1.4.0, default features, default discovery (P3). |
| `harness/native` (`iroh-do-harness`) | native | tokio WebSocket server hosting `iroh-relay-ingress` + `iroh-do-core` exactly as the DO does, for Phase 1–2 conformance tests against stock clients. |

## Demo ALPNs

All messages are UTF-8. "Line" = bytes terminated by `\n`.

### `iroh-do/echo/1`
Peer opens one bi stream, writes bytes, finishes. DO writes the same bytes
back and finishes. One stream per connection is typical; more are allowed.

### `iroh-do/chat/1`
Peer opens one bi stream ("control"). Peer → DO: lines. DO → peer: one
reply line per request line, on the same stream.

- `<text>` → `echo: <text>`
- `/status` → one line of JSON:
  `{"id":"<E z32>","state":"Active","wakes":N,"endpoint_builds":M,"connections":K,"bundle":"<sha256 prefix>","since_wake_ms":T}`
  where `wakes` counts DO constructor runs (persisted in SQLite, so an
  increase between two `/status` calls proves the isolate was evicted or
  hibernated in between), and `endpoint_builds` counts Endpoint
  instantiations (proves Active → Warm → Active).
- `/callback <secs>` → `ok: callback in <secs>s`. The DO sets an alarm;
  when it fires, the DO dials the peer (see notify). `secs` ≤ 50, because
  the peer's socket to the DO lives ≤ 60 s after the peer's last send (V5).

**Unprompted DO → peer stream:** after the first request line on a
connection, the DO opens a **uni** stream to the peer and writes one line,
`hello from <E z32>: you are <peer z32>`, then finishes. The peer must
accept it to pass. This proves server-originated streams on a peer-dialed
connection.

### `iroh-do/notify/1`
**DO dials the peer.** The DO calls `Endpoint::connect(peer, "iroh-do/notify/1")`
over the peer's existing inbound socket (V20), opens a uni stream, writes
`callback from <E z32> wakes=<N>\n`, finishes, waits for the peer to close
the connection (≤ 5 s), then closes. The peer's endpoint (`iroh-do-demo
listen` / `lifecycle`) accepts this ALPN and prints the line.

## Debug surface
No HTTP debug endpoints. All lifecycle evidence flows through `/status` on
the chat ALPN, i.e. through iroh itself, which is the thing under test.
`DEMO_DEBUG` is therefore unnecessary and dropped.
