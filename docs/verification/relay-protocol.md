# Phase 0 verification: iroh relay protocol (server side), pinned to iroh 1.4.0

Scope: spec §10 items V1–V6, plus the upstream server's accept behaviour and reuse of `iroh-relay` on wasm32.
Sources: `~/.cargo/registry/src/index.crates.io-*/{iroh-relay-1.4.0, iroh-1.4.0, iroh-base-1.4.0}`.
Citations use the form `crate-version/path:line`.

Claims marked **[empirical]** were confirmed by a throwaway harness. The harness is a hand-written relay
server, built with no iroh-relay server code, that ignores the client-auth header and always sends a
challenge. It was driven by the stock `iroh_relay::client::ClientBuilder` 1.4.0, once over `http://<E>.localhost`
and once over real TLS (rcgen certificate). The harness (`relaye2e`) is not kept in the repository.
Its role is now filled by the conformance tests in `crates/iroh-relay-ingress/tests/`.

---

## TL;DR

| Item | Answer |
|---|---|
| **BLOCKERS** | **None.** V3 passes: the challenge path works after the client has sent the TLS-exporter header **[empirical]**. |
| V1 | Path is **not** preserved. `set_path("/relay")` replaces it; the query string **is** preserved. `https://<E>.e.example.com/` → `wss://<E>.e.example.com/relay`. `http://<E>.localhost:8787/` → `ws://<E>.localhost:8787/relay` **[empirical]**. Probes (`/ping`, `/generate_204`, QAD) go only to relays in the client's *own* `RelayMap`, never to a remote's relay URL. Answering them is not required. |
| V2 | One request header `Sec-WebSocket-Protocol: iroh-relay-v2, iroh-relay-v1` (newest first). The server picks the max. The 101 **must** echo exactly one token in `Sec-WebSocket-Protocol`, or the client fails with `BadVersionHeader`. Versions in 1.4.0: `iroh-relay-v1` and `iroh-relay-v2`. |
| V3 | **Yes**, definitively. The client's handshake code has no memory of having sent the header and accepts `ServerChallenge` as the first frame. Native `http://`/`ws://` and wasm clients never send the header **[empirical for http]**. |
| V4 | Full byte reference in §8. Frame types are 0–13. `Health(11)` is v1-only and `Status(13)` is v2-only; the client tears down the connection on a mismatch. There is **no** "peer gone" signal for unknown destinations: upstream drops silently. `EndpointGone(8)` is only sent when a previous *sender* disconnects. |
| V5 | The client connects lazily, on the first datagram queued for that relay URL. A non-home relay connection closes itself **60 s after the client last *sent* a datagram**. If the server closes the socket, the client reconnects **eagerly**: immediately if a Pong had been received, otherwise with exponential backoff from 10 ms. `Restarting` is **ignored** by the client (trace log only). |
| V6 | Client Ping every 15 s; the timer resets on every frame received from the server. Pong deadline: 5 s for the first ping on a connection, then `clamp(3×RTT, 500 ms, 5 s)`. Server pings are not required. **Risk:** a hibernated DO must answer a Ping within as little as 500 ms. |
| V20 | DO dial-back over the peer's existing socket to R **works**. The peer delivers datagrams from any relay connection to noq with no home-relay filter, accepts the connection if its application is accepting, and replies via R. The window is ≤ 60 s after the peer's last *send* to E, because inbound traffic does not extend it. |
| Reuse | `iroh-relay` with `default-features=false` **does compile for wasm32** **[empirical]**, but it is not tokio-free: it pulls tokio, hyper, reqwest, rustls, noq, ws_stream_wasm (243 crates). All encode/decode functions and all handshake frame types are `pub(crate)`. **Not reusable as a codec; hand-roll it.** `iroh-base` (feature `key`) plus `blake3` compiles on wasm32 with 99 crates **[empirical]** and covers key parsing and signature verification. |

Spec corrections (details in each V-item):

- The challenge is **exactly 16 bytes**, not "16+".
- `Restarting` cannot make a 1.4.0 client reconnect lazily.
- Upstream does **not** close a superseded socket.
- Stock peers send QUIC keepalives every **5 s** by default.
- The relay-URL host label must be **z32 (52 chars)**, not the hex `Display` form (64 chars, which exceeds the 63-octet DNS label limit).

---

## V1. `RelayUrl` path handling, dialed URL, probes

### Answer

`RelayUrl` is a plain `Arc<Url>` with no path logic (`iroh-base-1.4.0/src/relay_url.rs:18-27`). The client builds the dial URL as follows:

```rust
let mut dial_url = (*self.url).clone();
dial_url.set_path(RELAY_PATH);            // RELAY_PATH = "/relay"
dial_url.set_scheme(match self.url.scheme() { "http" => "ws", "ws" => "ws", _ => "wss" })
```

- Native: `iroh-relay-1.4.0/src/client.rs:267-281`. Wasm: `iroh-relay-1.4.0/src/client.rs:391-405`.
- `RELAY_PATH` is defined at `iroh-relay-1.4.0/src/http.rs:13`.

`Url::set_path` **replaces** the whole path and keeps the query.

**[empirical]** Relay URL `http://<E>.localhost:PORT/e/some/path/?q=1` produced the request line `GET /relay?q=1` with `Host: <E>.localhost:PORT`.

| Relay URL | Dialed |
|---|---|
| `https://<E>.e.example.com/` | `wss://<E>.e.example.com/relay` (TLS, SNI = `<E>.e.example.com`, port 443) |
| `http://<E>.localhost:8787/` | `ws://<E>.localhost:8787/relay` (plain TCP, port 8787) |
| `https://zone/e/<E>/` | `wss://zone/relay`. **The path form is unusable.** |

Supporting details:

- TLS is used unless the scheme is `http` or `ws` (`iroh-relay-1.4.0/src/client/tls.rs:98-105`).
- The default port comes from the scheme (`iroh-relay-1.4.0/src/client/tls.rs:365-375`).
- `*.localhost` resolves to `127.0.0.1` and `::1` inside hickory without a DNS query. It matches on `LOCALHOST_usage.zone_of(n)` (`hickory-resolver-0.26.1/src/caching_client.rs:137-151`) and is used via `iroh-dns-1.4.0/src/dns.rs:908-946`. **[empirical]** `http://<64-hex>.localhost:PORT` connected. This also settles V18 for native clients: plain `http://` works and no local certificate is needed.
- Wasm clients append `?token=<auth_token>` to the dial URL, because browsers cannot set headers (`iroh-relay-1.4.0/src/client.rs:407-411`).

**Probes.** None of these is sent to a relay URL learned via address lookup:

- `run_https_probe` GETs `relay.join("/ping")` (`iroh-1.4.0/src/net_report/reportgen.rs:817-824`).
- The captive-portal check GETs `http://{host}/generate_204` with `X-Iroh-Challenge` (`iroh-1.4.0/src/net_report/reportgen.rs:621-626`).
- QAD runs over QUIC to port 7842 (`iroh-relay-1.4.0/src/defaults.rs:7`).

The probe plan is built from the client's own `RelayMap` (`iroh-1.4.0/src/net_report/reportgen.rs:377-379`). A relay is only added to that map by explicit configuration or `Endpoint::insert_relay` (`iroh-1.4.0/src/endpoint.rs:998`, `iroh-1.4.0/src/socket.rs:1235-1240`). Data-path connections are started by `RelayActor::try_send_datagram` (`iroh-1.4.0/src/socket/transports/relay/actor.rs:1206-1213`), which never probes.

### Must our server answer them?

No. If someone misconfigures `<E>`'s URL as their own home relay, the effects are:

- HTTPS probes fail, so the relay is never chosen as preferred.
- The captive-portal check may report a portal.
- QAD (UDP) is impossible.

All of these are benign. Answering is cheap in the router Worker, and no DO is needed:

- `GET /ping` → `200`, `Access-Control-Allow-Origin: *`, empty body (`iroh-relay-1.4.0/src/server.rs:1031-1040`).
- `GET /generate_204` → `204`, and if `X-Iroh-Challenge` is present and valid, echo `X-Iroh-Response: response <challenge>` (`iroh-relay-1.4.0/src/server.rs:1054-1075`).

### Design impact

- **Hostname routing is mandatory.** Drop the path-form alternative in §5.1.
- The router must route only `GET /relay` (plus the optional `/ping` and `/generate_204`) and must ignore the query, except for `token`.
- **The label must be z32, not hex.** `PublicKey`'s `Display` is 64-char lowercase hex (`iroh-base-1.4.0/src/key.rs:221-223`). That exceeds the RFC 1035 63-octet label limit and breaks real DNS and certificates; it only worked on `.localhost` because hickory synthesizes the answer.
  - Use `PublicKey::to_z32()` / `from_z32()` (`iroh-base-1.4.0/src/key.rs:164-169`), which give 52 chars. Example: `df9sse9bp1nuftim3yhxiuk6w6r5hddsskjygpydux7esxjstioo`.
  - The router validates with `from_z32`.
  - Update §5.1 and V15 wording accordingly.

---

## V2. Version negotiation

### Answer

**Versions in 1.4.0:** `ProtocolVersion::{V1 = "iroh-relay-v1", V2 = "iroh-relay-v2"}` (`iroh-relay-1.4.0/src/http.rs:50-60`).

- `Ord` puts V2 above V1. `ALL = [V2, V1]` (`iroh-relay-1.4.0/src/http.rs:68`).
- V2 removed `Health` (11) and added `Status` (13) (`iroh-relay-1.4.0/src/http.rs:54-56`).

**Native client request.** A single header, `Sec-WebSocket-Protocol: iroh-relay-v2, iroh-relay-v1` (comma+space joined, newest first). It is built by `ProtocolVersion::all_as_header_value()` (`iroh-relay-1.4.0/src/http.rs:76-83`) and added at `iroh-relay-1.4.0/src/client.rs:309-312`.

**[empirical]** The request seen on the wire:

```
GET /relay HTTP/1.1
Host: localhost:53242
Upgrade: websocket
Connection: Upgrade
Sec-WebSocket-Key: …
Sec-WebSocket-Version: 13
sec-websocket-protocol: iroh-relay-v2, iroh-relay-v1
x-iroh-relay-client-auth-v1: <base64url, 151 chars>     (TLS only; see V3)
[Authorization: Bearer <token>]                          (only if configured; client.rs:320-326)
```

**Wasm client.** It passes `["iroh-relay-v2", "iroh-relay-v1"]` as the browser WebSocket protocols list (`iroh-relay-1.4.0/src/client.rs:415-418`).

**Server selection (upstream).**

- The upstream server splits the header on `,`, trims each token, keeps the known ones, takes `.max()`, and returns 400 if nothing matches or the header is missing (`iroh-relay-1.4.0/src/server/http_server.rs:603-622`).
- It also requires `Upgrade: websocket` with an exact byte match and `Sec-WebSocket-Version: 13` (`iroh-relay-1.4.0/src/server/http_server.rs:586-601`). Otherwise it returns 400, with a `Sec-WebSocket-Version: 13` hint for the version case (`iroh-relay-1.4.0/src/server/http_server.rs:751-766`).

**101 response (upstream)** (`iroh-relay-1.4.0/src/server/http_server.rs:659-671`):

```
HTTP/1.1 101 Switching Protocols
upgrade: websocket
sec-websocket-accept: <derived>
sec-websocket-protocol: iroh-relay-v2
connection: upgrade
```

**What the client checks.**

- The status must be 101, otherwise `UnexpectedUpgradeStatus` (`iroh-relay-1.4.0/src/client.rs:338-343`).
- `Sec-WebSocket-Protocol` must be present and parse *exactly* (strum `EnumString`, no trimming) as `iroh-relay-v1` or `iroh-relay-v2`. Otherwise the client fails with `BadVersionHeader` (`iroh-relay-1.4.0/src/client.rs:345-355`).
- The wasm path does the same with `ws_meta.protocol()` (`iroh-relay-1.4.0/src/client.rs:422-427`).
- The client then decodes all server frames under the version it saw (`iroh-relay-1.4.0/src/client/conn.rs:130-131`).

### Design impact

- `on_upgrade`:
  - Require `GET /relay`.
  - Parse `Sec-WebSocket-Protocol` as comma-separated with trimming, and choose V2 if offered, else V1.
  - Reject (400) if neither is offered.
  - Set `Sec-WebSocket-Protocol: <exact token>` on the 101. (V11 must confirm that workers-rs can set it.)
- Store the version in the session snapshot. It gates the Health/Status frames (§8.4).
- Every 1.4.0 client offers V2, so V1 support only matters for pre-0.98 clients. It costs one branch, so keep it.

---

## V3. TLS-exporter header ignored → challenge path (potential BLOCKER)

### Answer: **works. Not a blocker.**

**Client code path, precisely:**

1. `ClientBuilder::connect` (native).
   - It dials TCP and does TLS if the scheme is not `ws`/`http` (`iroh-relay-1.4.0/src/client/tls.rs:66-95`).
   - It calls `KeyMaterialClientAuth::new(&secret_key, &stream)`. If that returns `Some`, it adds the header `x-iroh-relay-client-auth-v1` (`iroh-relay-1.4.0/src/client.rs:328-335`).
   - Nothing about this is recorded. The header value is consumed by the request builder and the `Conn`/handshake state never sees it.
2. After the 101 and the version check, it runs `Conn::new(...)` → `handshake::clientside(&mut conn, secret_key)` (`iroh-relay-1.4.0/src/client/conn.rs:84-105`).
3. `clientside` (`iroh-relay-1.4.0/src/protos/handshake.rs:340-378`):
   - It reads the first frame and accepts any of `[ServerChallenge, ServerConfirmsAuth, ServerDeniesAuth]` (`:344-352`).
   - If the frame is `ServerChallenge`, it deserializes the frame, builds `ClientAuth::new(secret_key, &challenge)`, writes it, then reads `[ServerConfirmsAuth, ServerDeniesAuth]` (`:354-360`).
   - `ServerConfirmsAuth` → `Ok`. `ServerDeniesAuth` → `Err(ServerDeniedAuth{reason})` (`:365-377`).

So the challenge branch is unconditional on the header. The module docs say the header "won't always work … might break when there's an HTTPS proxy", and the server "falls back to … a challenge" (`iroh-relay-1.4.0/src/protos/handshake.rs:17-23`, `:412-415`, `:448-449`). That is the designed behaviour.

Upstream tests the case where the client sent a header and the server cannot match it: `test_handshake_mismatching_shared_secrets` → `Mechanism::SignedChallenge`, client `Ok` (`iroh-relay-1.4.0/src/protos/handshake.rs:773-786`). From the client's side, that is the same as a server that ignores the header.

**[empirical]**

- Over real TLS, the stock client sent `x-iroh-relay-client-auth-v1` (113-byte postcard payload). The hand-written server ignored it and sent `00 ‖ 16B challenge`.
- The client answered with a 98-byte `ClientAuth`. Its signature verified with `blake3::derive_key("iroh-relay handshake v1 challenge signature", challenge)` and `ed25519 verify_strict`.
- The server sent `02`, and the client's `connect()` returned `Ok`.
- Datagram, Ping/Pong, Status, Restarting and EndpointGone frames then flowed both ways.

**Does the header get sent in non-TLS or wasm mode? No.**

- Native over `http://`/`ws://`: the stream is `MaybeTlsStream::Raw`, and `export_keying_material` returns `None` for non-TLS (`iroh-relay-1.4.0/src/client/streams.rs:112-127`), so `KeyMaterialClientAuth::new` returns `None` (`iroh-relay-1.4.0/src/protos/handshake.rs:254-260`). **[empirical]** The `http://<E>.localhost` run had no such header.
- Wasm: `KeyMaterialClientAuth::new` is `#[cfg(not(wasm_browser))]` (`iroh-relay-1.4.0/src/protos/handshake.rs:250`). The wasm `connect` never adds it (`iroh-relay-1.4.0/src/client.rs:387-443`), and the wasm `WsBytesFramed` exporter returns `None` (`iroh-relay-1.4.0/src/protos/streams.rs:71-80`).

Through Cloudflare, the TLS session the client sees terminates at the edge. Even a DO that tried to verify the header could not: the exporter material is the edge's.

### Design impact

- §5.2 item 2 is confirmed. Always send `ServerChallenge` immediately after the socket opens. Never try the header; optionally log that it was present.
- **Correction:** the challenge is exactly 16 bytes. `ServerChallenge { challenge: [u8; 16] }` is postcard-encoded as 16 raw bytes (`iroh-relay-1.4.0/src/protos/handshake.rs:76-82`). The spec text "16+ bytes" is wrong.
- Do not send anything other than `ServerChallenge` before the auth result. The client's handshake `read_frame` rejects any other tag with `UnexpectedFrameType` (`iroh-relay-1.4.0/src/protos/handshake.rs:574-595`). In particular, send no `Ping` before `ServerConfirmsAuth`.

---

## V4. Frame set and layout

Answer: see §8 for the full byte reference. Key points:

- **Frame type** is a QUIC varint (RFC 9000 §16) at the start of every binary WebSocket message (`iroh-relay-1.4.0/src/protos/common.rs:80-111`).
  - The values are 0–13 (`iroh-relay-1.4.0/src/protos/common.rs:18-65`), so on the wire every type is 1 byte.
  - Decoders must still parse multi-byte varints, and must treat unknown types as errors (that is what upstream does).
- **Handshake payloads are postcard** (`iroh-relay-1.4.0/src/protos/handshake.rs:554-604`). Relay-phase payloads are hand-packed big-endian (`iroh-relay-1.4.0/src/protos/relay.rs:263-303`, `:335-369`).
- **Version-gated:**
  - `Health(11)`: client accepts it only under V1.
  - `Status(13)`: client accepts it only under V2.
  - On a mismatch, decoding fails with `FrameNotAllowedInVersion` (`iroh-relay-1.4.0/src/protos/relay.rs:439-473`). The client actor then gets `RecvError` and drops the connection (`iroh-1.4.0/src/socket/transports/relay/actor.rs:696`).
  - All other frames are version-independent.
- **Unknown or invalid frames from the server kill the client connection.** This covers any type the client does not expect (`iroh-relay-1.4.0/src/protos/relay.rs:474-476`), wrong lengths, and invalid public keys. The client resolves keys through `KeyCache::key_from_slice`, which checks that the point is valid.
- **Size limits:**
  - `MAX_PACKET_SIZE = 65536` bounds the bytes after the frame type, in both directions (`iroh-relay-1.4.0/src/protos/relay.rs:23`; checked at `:403-406` for client decode and `:543-546` for server decode).
  - The client refuses to *send* a frame whose total `encoded_len` (type byte included) exceeds 65536, and refuses empty datagram contents (`iroh-relay-1.4.0/src/client/conn.rs:147-155`).
  - WebSocket max payload is 1 MiB (`MAX_FRAME_SIZE`) on both client and server (`iroh-relay-1.4.0/src/protos/relay.rs:28-29`; `iroh-relay-1.4.0/src/client.rs:314`; `iroh-relay-1.4.0/src/server/http_server.rs:879-880`).
- **ECN:**
  - Wire byte = `EcnCodepoint as u8`, 0 for none. `Ect0 = 2`, `Ect1 = 1`, `Ce = 3` (`noq-proto-1.4.0/src/shared.rs:181-202`).
  - Decode masks with `& 0b11` (`noq-proto-1.4.0/src/shared.rs:192-201`).
  - The client **drops ECN on receive** (`meta_out.ecn = None`, `iroh-1.4.0/src/socket/transports/relay.rs:186`) but does send it (`iroh-1.4.0/src/socket/transports/relay.rs:329-341`).
- **Batching:**
  - The `*DatagramBatch` frames carry a u16 BE `segment_size`. Contents are concatenated segments, and the last segment may be shorter.
  - A zero segment size decodes as "no segment size" (`NonZeroU16::new`, `iroh-relay-1.4.0/src/protos/relay.rs:291-296`).
  - The client sends batches whenever noq hands it a GSO transmit (`iroh-1.4.0/src/socket/transports/relay.rs:336-339`). **[empirical]** `05 ‖ dst ‖ 03 ‖ 00 03 ‖ "abcdefg"`.
  - On receive, the client re-splits batches to fit its buffer (`iroh-1.4.0/src/socket/transports/relay.rs:128-133`).
- **Peer gone for unknown destinations: none.**
  - Upstream `Clients::send_packet` logs "no connected client, dropped packet" and returns `Ok` without notifying anyone (`iroh-relay-1.4.0/src/server/clients.rs:205-216`).
  - `EndpointGone(X)` is sent only when X disconnects, and only to peers X had successfully sent to (the `sent_to` map) (`iroh-relay-1.4.0/src/server/clients.rs:134-160`, `:217-221`).
  - Client handling only removes X from `endpoints_present` (`iroh-1.4.0/src/socket/transports/relay/actor.rs:742-744`). That set is used only for the `HasEndpointRoute` relay-selection shortcut (`iroh-1.4.0/src/socket/transports/relay/actor.rs:1279-1318`). It has no effect on QUIC or path state.
- **Restarting** has a payload of `u32 BE reconnect_in_ms ‖ u32 BE try_for_ms` (`iroh-relay-1.4.0/src/protos/relay.rs:357-363`). The 1.4.0 server never sends it (it appears in no file under `server/`), and the 1.4.0 client ignores it (see V5).

### Design impact

- §5.2 item 4: for a datagram whose `dst != E`, **drop silently and count it**, matching upstream. Sending `EndpointGone(dst)` is harmless (it only edits the client's route-hint set) but non-standard.
  - Compare the 32 raw bytes against E directly. There is no need to decompress the key: upstream's `key_from_slice` closes the connection on an invalid key, which is acceptable but not required.
  - Rate-limit as planned.
- §5.2 item 5: preserve ECN and `segment_size` into the transport on ingress.
  - On egress, emit type `07` when the transport gives a `segment_size` and type `06` otherwise.
  - Keep `32 + 1 (+2) + payload ≤ 65536`, or the client rejects the frame and drops the socket. This bounds the DO-side transport's max GSO batch.
- Never emit `Status` to V1 or `Health` to V2. Never emit any frame type the client does not decode (0–5 are client→server or handshake-only).

---

## V5. Client behaviour toward a remote's home relay

### Answer

- **Lazy connect.**
  - An `ActiveRelayActor` for a URL is created on the first datagram queued for that URL: `RelayActor::try_send_datagram` → `active_relay_handle_for_endpoint` → `start_active_relay` (`iroh-1.4.0/src/socket/transports/relay/actor.rs:1206-1213`, `:1279-1341`). It is also created for the client's home relay (`:1259-1272`).
  - Shortcut: if another already-connected relay has received from that endpoint (`HasEndpointRoute`), that relay is used instead (`:1288-1317`). This does not apply here, because E only ever talks via `<E>`'s URL.
  - The actor dials immediately (`CONNECT_TIMEOUT` 10 s covers DNS, TCP, TLS, upgrade and handshake; `:83-87`, `:538-549`). Datagrams queued while dialing are dropped every 3 s (`:89-96`, `:514-523`).
- **Inactivity cleanup for non-home relays: 60 s since the client last *sent* a datagram** to that relay (`RELAY_INACTIVE_CLEANUP_TIME`, `:67-68`).
  - The timer is reset only when datagrams are sent (`:672`) and on the 3 s dialing flush tick (`:514-515`). It is **not** reset by received datagrams or by Pings.
  - When the timer fires while connected, the actor exits with `Ok` and closes the WebSocket sink gracefully (`:699-702`, `:706-710`).
  - The home relay never times out (`if !self.is_home_relay`).
- **Server closes the socket**, or the client sees a read error or ping timeout:
  - The client gets `StreamClosedServer` / `ClientStreamRead` / `PingTimeout` (`:686-698`, `:614-616`), mapped to `Established` if a Pong was ever received on this connection and to `Handshake` otherwise (`:888-895`, `:746-759`).
  - The run loop then **reconnects immediately** when established (backoff reset, no sleep) (`:329-353`).
  - Otherwise it sleeps with exponential backoff: min 10 ms, max 16 s, jitter, unlimited retries (`:337-347`, `:392-399`).
  - This happens **whether or not there is data to send**. It continues until the 60 s inactivity timer fires; that timer is checked in the dialing and connected loops (`:524-527`, `:699-702`) but not during the backoff sleep (`:366-390`).
- **`Restarting`** is handled as `trace!("Ignoring {msg:?}")` (`:773-775`). It has no effect on reconnect timing.
- **`Status`:** `Healthy` is logged at info, `RateLimited` at warn plus a metric, anything else at warn (`:760-772`). **No status causes a disconnect.** That includes `SameEndpointIdConnected`.
- **Auth denied** (`ServerDeniesAuth`) is a dial error. The client backs off, and the reason surfaces as `auth_denied_reason` (`:1041-1057`).

**Peer-side traffic that keeps the relay socket busy:**

- iroh 1.4.0's default QUIC config sets connection `keep_alive_interval = 5 s`, per-path `keep_alive = 5 s` and `path_max_idle = 15 s` (`iroh-1.4.0/src/endpoint/quic.rs:155-160`, `iroh-1.4.0/src/socket.rs:105-117`).
- Relay paths use a 30 s path idle timeout (`iroh-1.4.0/src/socket.rs:118-129`).
- So while a stock peer has *any* QUIC connection to E, it sends a datagram over `<E>` at least every ~5 s. That counts as activity for the 60 s timer.

### Design impact (§5.5)

1. **The client cannot be closed lazily.** A DO-initiated close within the client's 60 s activity window causes an **immediate reconnect**: one wake, an upgrade and a handshake.
   - It is not a storm unless the DO repeats it.
   - `Restarting` cannot change this in 1.4.0, so "close with Restarting so the client reconnects lazily" is not achievable. Listing "Restarting changes reconnect policy" as an upstream proposal is reasonable.
2. **Stock clients close Warm sockets themselves**, about 60 s after their last send. In practice that is QUIC connection close plus ~60 s.
   - The recommended default is to **never close idle sockets from the DO**, i.e. `T_warm = ∞` or ≥ 60 s with no effect. Let the client close and let the DO go Cold on `webSocketClose`.
   - A `T_warm` shorter than 60 s only adds reconnect wakes.
   - Keep a hard cap only as a safety valve, for example against a peer's home relay being misconfigured to `<E>`, which would never close.
3. **Warm lasts ≤ ~60 s per peer and costs about 4 Ping wakes** (one every 15 s; see V6). Warm saves the handshake round trip on quick redials within that minute, which is the stated purpose. The cost is bounded. Keep Warm.
4. **Active is held open by the peer's 5 s keepalives** for as long as the peer keeps a QUIC connection, regardless of the DO's `max_idle_timeout`.
   - Keepalive PINGs are ack-eliciting, so a connection with keepalive never idles out. The DO's advertised idle timeout bounds nothing while the peer keeps alive.
   - §5.5's "If a peer enables keepalive" is the **default** for stock peers. Make the operator cap ("max connection age without open streams") on by default, or accept 5 s wakes for every open connection. **Flag for design review.**
5. **Supersede semantics (§5.2 item 8).** Upstream does *not* close the old socket.
   - It demotes it to inactive, sends it `Status::SameEndpointIdConnected` (V2) or `Health` (V1), and routes to the new one (`iroh-relay-1.4.0/src/server/clients.rs:79-101`).
   - If the new socket closes, it promotes the old one back and sends `Status::Healthy` (`iroh-relay-1.4.0/src/server/clients.rs:117-133`).
   - Closing the old socket instead would make an old-but-alive client reconnect immediately (point 1). With two live processes sharing a key, that becomes a reconnect ping-pong. **Recommendation:** mirror upstream (demote and Status, do not close), or close the old socket only after it has been silent for longer than one ping interval.

---

## V6. Ping interval and silent-server timeout

### Answer

- **Client sends `Ping(8 random bytes)`** on connect (the first tick is immediate) and then every 15 s (`PING_INTERVAL`).
  - The interval is **reset by every frame received** from the server (`iroh-1.4.0/src/socket/transports/relay/actor.rs:70-74`, `:585-588`, `:617-621`, `:693-694`).
  - It also pings on `CheckConnection` after network changes (`:639-645`).
- **Client timeout** (`iroh-relay-1.4.0/src/ping_tracker.rs:4-8`, `:49-66`, `:83-91`):
  - Only the most recent ping is tracked.
  - Its deadline is `PING_TIMEOUT = 5 s` while no RTT has been measured. The tracker is per connection (`actor.rs:572`), so this applies to the first ping on each connection.
  - After that the deadline is `clamp(3 × last_RTT, 500 ms, 5 s)`.
  - On expiry the client raises `PingTimeout`, then disconnects and reconnects (`actor.rs:614-616`, `:836-838`).
- **Write timeout:** 15 s for any client send (`actor.rs:805-817`).
- **Silent server.** There is no other read-idle timeout. A server that never sends anything but answers every Ping promptly is fine.
- **Server pings are not required.** The client answers them if sent (`actor.rs:745`, `:591-595`).
- **Pong payload** must echo the 8 bytes. A Pong with other data is ignored, so the deadline still fires (`ping_tracker.rs:72-81`).

### Design impact

- Answer `Ping` with `Pong` (same 8 bytes) **immediately**, in every state, including Warm and just-woken-from-hibernation.
- **Risk (not a blocker):**
  - After the first Pong, the deadline can be as short as **500 ms**: an RTT under about 167 ms gives `3 × RTT < 500 ms`, and the clamp holds it at 500 ms.
  - A Ping arriving at a hibernated DO must therefore be answered within ~0.5 s, including isolate wake and wasm instantiation.
  - Missing the deadline costs an immediate reconnect, a new handshake, and a 5 s first-ping budget. That is self-healing but wasteful.
  - Phase 0/6 must measure wake-to-first-byte for the DO.
  - Mitigation: handle `09 ‖ 8B` → `0a ‖ 8B` in the thinnest possible layer (the JS shim, before any wasm instantiation). It is a 9-byte compare-and-copy that needs no session state beyond "authenticated", and the socket attachment can carry that.
  - `setWebSocketAutoResponse` cannot do it: it is text-only, and the native client skips text messages (`iroh-relay-1.4.0/src/protos/streams.rs:101-104`).
- Do not originate server pings while Warm (consistent with §5.2 item 6). Client pings every 15 s already detect dead sockets on the client side. On the DO side, the platform's close events (and their absence) are all we get. Upstream's server-side pings exist to reap dead clients, and a hibernated socket costs nothing.

---

## V20. DO-initiated dial back to a peer over its existing socket to R

Scenario:

- Stock peer P has home relay H. P dialed E, whose relay URL is R (the ingress), so P's relay client holds a socket to R.
- P's QUIC connection to E is later closed, but the socket to R is still open.
- E now dials P by sending QUIC Initial packets as `06`/`07` frames with `src = E` over that socket.

### (a) Does P deliver the packets and accept the connection? **Yes.** There is no home-relay or known-relay filter anywhere on the path.

Receive path, step by step:

1. `ActiveRelayActor::handle_relay_msg` forwards every `RelayToClientMsg::Datagrams` into the shared `relay_datagrams_recv` queue, tagged `url = R, src = E`. It records `E` in `endpoints_present`, and it does not check `is_home_relay` (`iroh-1.4.0/src/socket/transports/relay/actor.rs:717-741`). The same receive loop runs for home and non-home actors (`:686-698`).
2. `RelayTransport::poll_recv` drains that queue and emits `RecvInfo::from_addr((R, E))`, with ECN dropped (`iroh-1.4.0/src/socket/transports/relay.rs:91-189`).
3. `Socket::process_datagrams` maps `Addr::Relay(R, E)` to a `RelayMappedAddr` using `mapped_addrs.relay_addrs.get(&(R, E))` and sets it as noq's source address (`iroh-1.4.0/src/socket.rs:579`, `:635-646`). `AddrMap::get` **creates** a mapping for a previously unseen `(url, endpoint)` pair (`iroh-1.4.0/src/socket/mapped_addrs.rs:336-351`), so a never-before-seen relay is fine. The QUIC Initial then reaches noq as an ordinary datagram from a mapped socket address.
4. On accept, `Incoming::remote_addr` converts that address back to `Relay(R, E)` (`iroh-1.4.0/src/endpoint/connection.rs:203-213`). iroh adds no relay-based admission filter: the decision belongs to the application's accept or `Router` (ALPN). Identity comes from the TLS handshake (E's key), not from the relay.

Preconditions:

- P must be accepting connections. A plain dial-only client with no `Router` or `accept()` loop will not take the connection.
- The connection must use an ALPN that P serves.

### (b) Where do P's replies go? **Back via R, with no address lookup needed.**

- noq replies to the source address of the Initial, which is the `RelayMappedAddr`.
- The send path resolves `MultipathMappedAddr::Relay` → `FourTuple::Relay { url: R, endpoint_id: E }` through `relay_addrs.lookup` (`iroh-1.4.0/src/socket/transports.rs:1222-1233`). It is then queued as a `RelaySendItem { url: R, remote_endpoint: E }`, and `RelayActor` routes it to the existing actor for R (`actor.rs:1206-1213`, `:1284-1286`).
- Once the connection is up, the path is registered as an open relay path with `Source::Connection` and the 30 s relay-path idle timeout (`iroh-1.4.0/src/socket/remote_map/remote_state.rs:996-1023`).
- Any later address lookup for E would yield E's published relay, which is also R. That is consistent and harmless either way.

### (c) How long is dial-back possible? **Until 60 s after P last *sent* a datagram to E. Inbound traffic does not extend it.**

- The non-home actor's `inactive_timeout` (60 s, `actor.rs:67-68`) is reset only when P **sends** datagrams (`:672`) or on the dialing flush tick (`:514-515`).
- When it fires, the actor closes the socket gracefully (`:699-702`, `:706-710`).
- Received datagrams do **not** reset it: the code comments say "Inactivity is only tracked on the last datagram sent to the relay, received datagrams will trigger QUIC ACKs" (`:148-153`).
- The only client-side "keepalive" on that socket is the relay `Ping` every 15 s (`:585-588`, `:617-621`). It keeps the WebSocket verified but does **not** extend the actor's life.
- If E dials within the window, P's handshake replies are sends over R, so they reset the 60 s timer and the connection keeps itself alive from then on, helped by the 5 s QUIC keepalives (`iroh-1.4.0/src/endpoint/quic.rs:157-158`).
- Edge case: if the 60 s expire while E's Initial is in flight, P closes the socket first. E's packets are lost, and the DO sees `webSocketClose`.
- After the socket closes, R has no way to reach P. Dial-back would require the DO to dial P's own home relay H outbound, which is out of scope for this document. Outbound sockets pin the DO; see §2.1.

### Design impact

- **Feasible:** the DO can dial back a peer as long as that peer's socket to R is open.
- **The window is short:** at most ~60 s after the peer's last send to E. In practice that is ≤ ~60 s after its previous QUIC connection to E closed: the closing exchange is the last send, and with stock 5 s keepalives there is no long silent tail.
- The ingress transport's `send(dst = peer)` must treat "socket present" as the only route. If the socket is gone, return "no route" and do not queue. This matches §5.3.
- Dial-back over R is a convenience for "recently connected" peers, not a general mechanism. A general "E dials arbitrary P" feature needs outbound relay-client support in the DO (dialing H), with the cost model in §2.1.
- Do not make dial-back depend on Warm sockets lasting longer: the **client** closes them, not the DO (V5).

---

## 7. What the upstream server does on accept (to mirror)

`RelayService` (`iroh-relay-1.4.0/src/server/http_server.rs`):

1. **Establish timeout: 30 s**, from TCP accept until the relay handshake has completed and the client is registered (`ESTABLISH_TIMEOUT`, `:72-76`; enforced at `:1043-1049`; cleared at `:640-648`).
   - Our analogue: close a socket that has not sent a valid `ClientAuth` within N s. Use 30 s for parity; 10 s is ample in practice, since the client's own `CONNECT_TIMEOUT` is 10 s.
2. **Upgrade validation:** `GET /relay` only. Required headers: `Upgrade: websocket`, `Sec-WebSocket-Key`, `Sec-WebSocket-Version: 13`, and a `Sec-WebSocket-Protocol` containing a known version; otherwise 400 (`:573-622`, `:745-766`). Other paths return registered handlers or 404 (`:769-788`).
3. **Accept:**
   - Disable Nagle.
   - Wrap reads in an optional per-client byte-rate limiter (`Limits::client_rx`, unset by default; `iroh-relay-1.4.0/src/server.rs:490-511`).
   - Set the WebSocket max payload to 1 MiB (`:869-880`).
4. **Handshake** (`iroh-relay-1.4.0/src/protos/handshake.rs:421-474`):
   - Try the header.
   - Otherwise send `ServerChallenge` (16 random bytes), read exactly one `ClientAuth`, and verify.
   - On a bad signature, send `ServerDeniesAuth{"signature invalid"}` and close.
5. **Authorization:** `AccessControl::on_connect(ClientRequest)` runs **after** authentication.
   - Allow → `ServerConfirmsAuth`. Deny → `ServerDeniesAuth{reason or "not authorized"}` (`iroh-relay-1.4.0/src/protos/handshake.rs:489-551`).
   - The bearer token comes from the first `Authorization: Bearer` header, or else the `?token=` query parameter (`iroh-relay-1.4.0/src/server.rs:253-275`).
   - 1.4.0 has no built-in token format; it is an opaque string passed to `AccessControl`. The spec's §5.2 item 3 token semantics must therefore be defined by us or by a newer upstream.
   - Recommendation: deny via `ServerDeniesAuth` after the challenge, not with an HTTP 401, so the client surfaces the reason.
6. **Register:** apply the supersede semantics described in V5 point 5.
7. **Per-client loop** (`iroh-relay-1.4.0/src/server/client.rs:367-445`):
   - Answer a `Ping` with `Pong` immediately (`:511-515`).
   - The server pings only after it has received nothing for `15 s + rand(1..=5) s` (`:368-377`, `:406-407`, `:428-433`). Its pong timeout is 5 s; on expiry it closes (`:424-427`).
   - Write timeout: 2 s; a stalled write closes the client (`iroh-relay-1.4.0/src/defaults.rs:49-51`; `client.rs:449-452`).
   - The per-client send queue holds 512 packets; when it is full, packets are dropped (`iroh-relay-1.4.0/src/protos/relay.rs:38-40`; `clients.rs:224-229`).
   - `Status::RateLimited` is sent once per connection to V2 clients when the limiter first engages (`:388-400`).
   - A frame decode error, a stream end or a write error ends the connection (`:487-492`, `:409-418`).
8. **Disconnect:** send `EndpointGone(self)` to every peer this client had successfully sent to (`clients.rs:134-160`).
9. **Probes:** `GET /ping` → 200 with ACAO `*` (`iroh-relay-1.4.0/src/server.rs:739`, `:1031-1040`). `GET /generate_204` → 204 with the challenge echo (`:1054-1075`, `:1174-1176`).

Mapping to the DO:

- Items 1, 2, 4, 5 and the Ping→Pong rule are required for conformance.
- The server pings in item 7 conflict with hibernation; skip them while Warm (V6).
- The write timeout and queue depth have no direct analogue (`ws.send` does not block), but apply a bounded outbound queue if the transport can outrun the socket.
- Item 8 does not apply. The DO has exactly one remote "sender" (E itself), and E disappearing means the DO is gone.

---

## 8. Wire reference (implement against this)

Conventions:

- One relay frame = one **binary** WebSocket message.
- Text messages: upstream clients skip them (`iroh-relay-1.4.0/src/protos/streams.rs:101-104`), so never send them. Treat them as protocol errors when received.
- WebSocket Close = end of stream.
- `varint(q)` = QUIC varint; all frame types here are < 64, so it is 1 byte.
- `leb(n)` = postcard unsigned LEB128.
- `key` = 32-byte ed25519 public key (compressed Edwards Y).
- All multi-byte integers in relay frames are **big-endian**.

### 8.1 Frame type registry (`iroh-relay-1.4.0/src/protos/common.rs:18-65`)

| Type (hex) | Name | Direction | Phase | Version |
|---|---|---|---|---|
| `00` | ServerChallenge | S→C | handshake | all |
| `01` | ClientAuth | C→S | handshake | all |
| `02` | ServerConfirmsAuth | S→C | handshake | all |
| `03` | ServerDeniesAuth | S→C | handshake | all |
| `04` | ClientToRelayDatagram | C→S | relay | all |
| `05` | ClientToRelayDatagramBatch | C→S | relay | all |
| `06` | RelayToClientDatagram | S→C | relay | all |
| `07` | RelayToClientDatagramBatch | S→C | relay | all |
| `08` | EndpointGone | S→C | relay | all |
| `09` | Ping | both | relay | all |
| `0a` | Pong | both | relay | all |
| `0b` | Health | S→C | relay | **V1 only** |
| `0c` | Restarting | S→C | relay | all |
| `0d` | Status | S→C | relay | **V2 only** |

The server must accept only `04 05 09 0a` from the client after authentication (upstream: `iroh-relay-1.4.0/src/protos/relay.rs:548-577`), and only `01` during the handshake.

### 8.2 Handshake (postcard payloads; `iroh-relay-1.4.0/src/protos/handshake.rs:58-107`, `:554-604`)

```
ServerChallenge     = 00 ‖ challenge[16]                         ; exactly 17 bytes. [u8;16] → 16 raw bytes, no length prefix
ClientAuth          = 01 ‖ public_key[32] ‖ 40 ‖ signature[64]   ; exactly 98 bytes
                      ; public_key: PublicKey serde, non-human-readable → [u8;32] tuple → 32 raw bytes (iroh-base-1.4.0/src/key.rs:78-103)
                      ; signature: [u8;64] with #[serde(with="serde_bytes")] → leb(64)=0x40 ‖ 64 bytes   [empirical]
ServerConfirmsAuth  = 02                                          ; unit struct, empty payload
ServerDeniesAuth    = 03 ‖ leb(len(reason)) ‖ utf8(reason)        ; postcard String
```

**Signature** (`iroh-relay-1.4.0/src/protos/handshake.rs:209-230`):

```
msg = blake3::derive_key(context = "iroh-relay handshake v1 challenge signature", key_material = challenge[16])   // 32 bytes
sig = ed25519_sign(client_secret, msg)
verify: PublicKey::from_bytes(pk)? ; pk.verify(msg, Signature::from_bytes(sig))   // ed25519-dalek verify_strict (iroh-base-1.4.0/src/key.rs:134-138)
```

Notes:

- The challenge must come from a CSPRNG (upstream: `rand::rng()`, `iroh-relay-1.4.0/src/protos/handshake.rs:200-207`, `:452`).
- Decode `ClientAuth` strictly. Require length 98, `p[0]=01`, `p[33]=40`, and a valid key. Otherwise send `ServerDeniesAuth` or close.

**Header form (ignored by us; documented for completeness)**:

- `x-iroh-relay-client-auth-v1: base64url_nopad(postcard{ public_key[32] ‖ 40 ‖ sig[64] ‖ key_material_suffix[16] })`, which is 113 bytes before base64 **[empirical]**.
- `sig` is over the first 16 bytes of `TLS-Exporter(label="iroh-relay handshake v1", context=public_key, len=32)` (`iroh-relay-1.4.0/src/protos/handshake.rs:54-73`, `:254-278`).

**Golden vectors** (deterministic ed25519). Secret key = `[42u8; 32]`, which is the same key upstream's snapshot tests use.

```
pk          = 197f6b23e16c8532c6abc838facd5ea789be0c76b2920334039bfa8b3d368d61
pk (z32)    = df9sse9bp1nuftim3yhxiuk6w6r5hddsskjygpydux7esxjstioo
challenge   = 11111111111111111111111111111111
derive_key  = d9f7b5a31b89580cdc27cb4f323ae7dcda22f834b9f35451cb9217c9e0be8601
signature   = 8962427ee2d717ba0a250113d637b7c1d84baaec8aab6353bb93de2e3950a14d
              82b6d4ea97e1f88530e8b9b7addee9427cdd45686b8132d0b2d285d974058a0e
ServerChallenge   = 00 11111111111111111111111111111111
ClientAuth (98 B) = 01 197f6b23e16c8532c6abc838facd5ea789be0c76b2920334039bfa8b3d368d61
                    40 8962427ee2d717ba0a250113d637b7c1d84baaec8aab6353bb93de2e3950a14d
                       82b6d4ea97e1f88530e8b9b7addee9427cdd45686b8132d0b2d285d974058a0e
ServerConfirmsAuth = 02
ServerDeniesAuth("not authorized") = 03 0e 6e6f7420617574686f72697a6564
```

### 8.3 Relay phase, client → server

```
ClientToRelayDatagram       = 04 ‖ dst_key[32] ‖ ecn[1] ‖ payload[n]                 ; n ≥ 1 (client never sends empty)
ClientToRelayDatagramBatch  = 05 ‖ dst_key[32] ‖ ecn[1] ‖ segment_size[2, BE] ‖ payload[n]
                              ; payload = ⌈n/segment_size⌉ datagrams; all but the last are exactly segment_size; segment_size=0 ⇒ treat as single
Ping                        = 09 ‖ data[8]          ; exactly 9 bytes
Pong                        = 0a ‖ data[8]          ; exactly 9 bytes (reply to a server Ping)
```

- `ecn`: `00` none, `01` ECT(1), `02` ECT(0), `03` CE. Mask the low 2 bits.
- Constraints:
  - Bytes after the type ≤ 65536 (upstream server decode check).
  - Datagram frames need ≥ 33 bytes after the type, and the batch form ≥ 35 (`iroh-relay-1.4.0/src/protos/relay.rs:280-286`, `:549-551`).
- **[empirical]** frames captured from the stock client:
  - `05 fd1724…f618 03 0003 61626364656667`: batch, CE, segment size 3, payload "abcdefg".
  - `04 fd1724…f618 00 78`: single datagram, no ECN, payload "x".

### 8.4 Relay phase, server → client

```
RelayToClientDatagram       = 06 ‖ src_key[32] ‖ ecn[1] ‖ payload[n]
RelayToClientDatagramBatch  = 07 ‖ src_key[32] ‖ ecn[1] ‖ segment_size[2, BE] ‖ payload[n]
EndpointGone                = 08 ‖ key[32]                                   ; exactly 33 bytes
Ping                        = 09 ‖ data[8]
Pong                        = 0a ‖ data[8]                                   ; echo the client's 8 bytes
Health      (V1 only)       = 0b ‖ utf8(problem)                             ; no length prefix
Restarting                  = 0c ‖ reconnect_in_ms[4, BE u32] ‖ try_for_ms[4, BE u32]   ; exactly 9 bytes; ignored by 1.4.0 clients
Status      (V2 only)       = 0d ‖ code[1]       ; 00 Healthy, 01 SameEndpointIdConnected, 02 RateLimited, other = Unknown(n)
```

Constraints the client enforces. Violating any of them kills the connection.

- Bytes after the type ≤ 65536.
- `src_key` must be a valid ed25519 point. For us it is always E.
- `EndpointGone`, `Ping` and `Pong` have exact lengths.
- `Restarting` is exactly 8 bytes after the type.
- `Health` must be valid UTF-8.
- The type must be allowed in the negotiated version.

Upstream snapshot vectors (`iroh-relay-1.4.0/src/protos/relay.rs:608-760`; key = `SecretKey([42;32]).public()`). The Ping/Pong layout is shared by both directions; for client→server, types 04/05 replace 06/07.

```
EndpointGone   = 08 197f6b23e16c8532c6abc838facd5ea789be0c76b2920334039bfa8b3d368d61
Ping([42;8])   = 09 2a2a2a2a2a2a2a2a
Pong([42;8])   = 0a 2a2a2a2a2a2a2a2a
RelayToClientDatagramBatch(ecn=CE, seg=6, "Hello World!")
               = 07 197f6b23…8d61 03 0006 48656c6c6f20576f726c6421
RelayToClientDatagram(ecn=CE, "Hello World!")
               = 06 197f6b23…8d61 03 48656c6c6f20576f726c6421
ClientToRelayDatagramBatch(ecn=CE, seg=6, "Hello World!")
               = 05 197f6b23…8d61 03 0006 48656c6c6f20576f726c6421
ClientToRelayDatagram(ecn=CE, "Hello World!")
               = 04 197f6b23…8d61 03 48656c6c6f20576f726c6421
Restarting(10ms, 20ms) = 0c 0000000a 00000014
Status(SameEndpointIdConnected) = 0d 01
Status(RateLimited)             = 0d 02
Health("Hello? Yes this is dog.") = 0b 48656c6c6f3f2059657320746869732069732064 6f672e
```

**[empirical]** The stock client decoded all of the following correctly over V2:

- `06…02 6869` → `Datagrams{ecn: Ect0, segment_size: None}`.
- `07…00 0003 61626364656667` → `segment_size: Some(3)`.
- `09 0102030405060708`, `0d 00`, `0c 0000000a 00000014`, `08 <key>`.

### 8.5 Session sequence (what the ingress implements)

```
C: GET /relay  (Sec-WebSocket-Protocol: iroh-relay-v2, iroh-relay-v1 [, Authorization: Bearer …] [, x-iroh-relay-client-auth-v1: …ignored…])
S: 101  Sec-WebSocket-Protocol: iroh-relay-v2
S: 00 ‖ challenge[16]                                  (immediately on open; nothing else first)
C: 01 ‖ pk ‖ 40 ‖ sig                                  (reject anything else; deadline ~10–30 s)
S: 02            | 03 ‖ leb(len) ‖ reason              (after verify + optional admission check)
C: 09 ‖ d[8]     (sent right after auth by iroh)       → S: 0a ‖ d[8]   (within ≤500 ms…5 s)
C: 04/05 ‖ E ‖ …  → deliver to transport  (dst ≠ E → drop+count)
S: 06/07 ‖ E ‖ …  ← from transport
C: 09 every 15 s of server silence → S: 0a
C: WebSocket Close ≈60 s after the client's last datagram (non-home relay)
```

---

## 9. Reusing `iroh-relay` from a no-tokio wasm32 crate

**Tried.** Throwaway crate `relaycheck` (not kept), with `iroh-relay = { version = "=1.4.0", default-features = false }`, built with `cargo check --target wasm32-unknown-unknown`.

- **Compiles** without extra features. iroh-relay already enables `getrandom/wasm_js` for `wasm32-unknown-unknown` (Cargo.toml target deps), and iroh-base does the same (`iroh-base-1.4.0/Cargo.toml:141-144`). The build-time `wasm_browser` alias is `all(target_family="wasm", target_os="unknown")` (`iroh-relay-1.4.0/build.rs`).
- **Not tokio-free.**
  - `tokio` is a non-optional dependency with `io-util, macros, sync, rt` (plus `time` via others).
  - It also pulls `hyper`, `hyper-util`, `reqwest`, `rustls`, `tokio-rustls`, `noq`, `noq-proto`, `noq-udp`, `iroh-dns`, `ws_stream_wasm` and `wasm-bindgen-futures`: **243 crates** in the wasm graph.
  - It compiles, but it is heavy, and it is unknown whether every one of these runs in workerd.
- **`features = ["server"]` on wasm32 fails** (mio: "This wasm target is unsupported by mio").

**Public and usable without `server`:**

- `protos::relay::{RelayToClientMsg, ClientToRelayMsg, Datagrams (pub fields), Status, MAX_PACKET_SIZE, Error}`
- `protos::common::FrameType` (`Into<u32>`)
- `http::{ProtocolVersion (+ match_from_str, all_joined, to_str), RELAY_PATH, RELAY_PROBE_PATH, CLIENT_AUTH_HEADER}`
- `KeyCache`
- `PingTracker`
- `Datagrams::take_segments`
- `RelayToClientMsg::typ()`

**Not usable (compiler-confirmed):**

- `RelayToClientMsg::write_to` / `to_bytes` / `encoded_len`: `pub(crate)` and `#[cfg(feature="server")]` (`iroh-relay-1.4.0/src/protos/relay.rs:326-388`); no method is found without `server`.
- `RelayToClientMsg::from_bytes`: `pub(crate)` (`:396`), error E0624.
- `ClientToRelayMsg::to_bytes` / `write_to`: `pub(crate)` (`:497-522`), error E0624.
- `ClientToRelayMsg::from_bytes`: `pub(crate)` and server-only (`:538-540`).
- `FrameType::write_to` / `from_bytes`: `pub(crate)` (`common.rs:82-111`).
- All handshake types (`ServerChallenge`, `ClientAuth`, `ServerConfirmsAuth`, `ServerDeniesAuth`, `KeyMaterialClientAuth`): `pub(crate)`, error E0603. `handshake::serverside` is server-only and async over a `Sink`/`Stream`, so it is not sans-io.

**Lean alternative, tried.** `iroh-base = { version="=1.4.0", default-features=false, features=["key"] }` plus `blake3` compiles on wasm32 with **99 crates** and gives `PublicKey::from_bytes`, `verify` (strict), `Signature::from_bytes` and `to_z32`/`from_z32`.

### Design impact

- `iroh-relay-ingress` should **hand-roll the codec** from §8. That is about 300 LOC with no async and depends only on `iroh-base[key]` and `blake3`.
- Do **not** depend on `iroh-relay` at runtime. The only things that could be borrowed are the `ProtocolVersion` strings, and copying two constants is cheaper than 243 crates.
- Conformance (spec §5.2 "golden tests"), all as native dev-dependencies:
  1. Copy upstream's snapshot hex vectors (`iroh-relay-1.4.0/src/protos/relay.rs:608-760`) and the handshake vectors above.
  2. Build an e2e test that drives the codec over a real WebSocket with the stock `iroh_relay::client::ClientBuilder`, over both `http://` and TLS. This was the throwaway harness `relaye2e`, about 200 LOC with tokio-websockets, rcgen and tokio-rustls.
  3. A full `iroh::Endpoint` peer test as specified.
  4. The upstream golden functions are private, so byte equivalence comes from (1) plus the live client in (2).
- **Upstream proposal candidate:** make the relay frame `write_to`/`from_bytes` and handshake frame types `pub`, and drop the `server` gating. A `protos`-only feature without tokio/hyper would also help.

---

## 10. Spec edits implied

| § | Current text | Change |
|---|---|---|
| 5.1 | Path form `https://<zone>/e/<E>/` acceptable if V1 holds | Remove it: the path is replaced by `/relay`. Mandate a z32 (52-char) label, not hex. |
| 5.2.2 | "challenge is 16+ bytes" | Exactly 16 bytes. Send nothing before it. |
| 5.2.4 | "peer gone / not here signal if one exists" | None exists for unknown destinations. Drop silently and count. |
| 5.2.6 | determine client timeout | Client pings every 15 s. Pong deadline is 5 s at first, then `clamp(3·RTT, 0.5 s, 5 s)`. Pong fast-path before wasm instantiation. |
| 5.2.7 | Restarting/Status on Warm→Cold so the client reconnects lazily | 1.4.0 ignores Restarting and reconnects eagerly on any close. Don't close idle sockets; the client closes ~60 s after its last send. |
| 5.2.8 | Supersede by closing the old socket | Upstream demotes the old socket and sends Status (`SameEndpointIdConnected`) without closing. Mirror that to avoid reconnect ping-pong. |
| 5.5 | `T_warm` default 10 min, Warm→Cold close | Default: no DO-initiated close. Warm naturally lasts ≤ ~60 s (about 4 ping wakes). |
| 5.5 | Active idle via DO `max_idle_timeout`; "if a peer enables keepalive" | Stock peers default to 5 s keepalive (`iroh-1.4.0/src/endpoint/quic.rs:157-158`), so the operator cap should default on, or Active-while-connected wakes every 5 s must be costed. |
| 5.2.3 | iroh relay-token semantics | 1.4.0 has an opaque bearer string only (header, or `?token=` from wasm). We define the semantics. Deny via `ServerDeniesAuth` after the challenge. |
