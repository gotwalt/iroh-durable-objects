# Phase 0 verification: Cloudflare / workers-rs / OpenTofu (V11, V12, V15, V17, V18, V19, limits)

Date: 2026-10-21. Tool versions: wrangler 4.130.0 (bundled workerd supports compat dates up to `2026-09-15`), OpenTofu 1.13.1, node 24, rustc 1.93.0, **workers-rs `worker` 0.8.7**, **`worker-build` 0.8.7** (`cargo install worker-build --version 0.8.7 --locked`), **Cloudflare provider `cloudflare/cloudflare` 5.28.1** (latest 5.x on registry.opentofu.org; no 6.x exists).

Scope note: per design note 001 (`docs/design-notes/001-path-form-relay-url.md`), the reference deployment uses **path-form URLs** `https://iroh-do.example.com/e/<E>/`. The spike router supports path form first and falls back to the Host label. I tested both forms.

Citation format: `crate-version/path:line` refers to `~/.cargo/registry/src/index.crates.io-*/`. "provider v5.28.1 `file:line`" refers to `cloudflare/terraform-provider-cloudflare@v5.28.1/internal/services/workers_script/`.

## Summary

| Item | Verdict |
|---|---|
| V11 | **Supported, with one gap.** Everything needed is in workers-rs 0.8.7 and was proven in workerd. The gap is **DO RPC**, which does not work with the default `worker-build` shim. Use fetch-to-stub instead. |
| V12 | **Local clock is NOT frozen.** `Date.now()` advanced 4552 ms inside one busy loop. `performance.timeOrigin` is 0 locally, as in production. Local `wrangler dev` **does hibernate** idle hibernatable-WS DOs after about 10 s: the constructor re-runs and in-memory state is lost, while the socket, tags and attachment survive. Tests can force eviction with `evictDurableObject` (vitest integration). |
| V15 | A second-level wildcard (`*.e.zone`) needs ACM ($10/mo per zone). This is moot under path form, because Universal SSL covers `iroh-do.example.com`. |
| V17 | **Supported, no fallback needed.** Provider 5.28.1 does multi-module upload (`main_module` + `content_file` + `files{}` with `application/wasm`), `durable_object_namespace`, `secret_text` and `plain_text` bindings, `migrations.new_sqlite_classes`, and compat date/flags. `tofu validate` passes, as does offline `tofu plan`. |
| V18 | A native client reaches `http://127.0.0.1:<port>/e/<E>/relay` with the upgrade intact (101, subprotocol echoed). wrangler dev also preserves Host. |
| V19 | Deleting the Worker script deletes the DO namespaces it implements, and their data. Re-creating the script does not bring the data back. `tofu destroy` therefore destroys every endpoint identity. |

No blockers in this area.

---

## V11: workers-rs 0.8.7 capabilities

### Answer and evidence

| Capability | Status | Evidence (source) | Proven in workerd |
|---|---|---|---|
| `#[durable_object]` struct + `impl DurableObject` | yes | `worker-macros-0.8.7/src/durable_object.rs:143-210`. Without an attribute argument, the macro generates constructor, fetch, alarm, webSocket{Message,Close,Error} and connect bindings (`:166-171`). | yes |
| SQLite `sql.exec` (sync) | yes | `worker-0.8.7/src/durable.rs:657` (`Storage::sql()`), `worker-0.8.7/src/sql.rs:201` (`exec(query, Option<Vec<SqlStorageValue>>)` returns `SqlCursor`, sync), `sql.rs:330` `to_array`, `sql.rs:343` `one`, `sql.rs:388` `raw`. | yes, including from the **constructor** |
| `accept_websocket_with_tags` | yes | `worker-0.8.7/src/durable.rs:372`; `get_tags` `:399` | yes, tags returned after wake |
| `serialize_attachment` / `deserialize_attachment` | yes | `worker-0.8.7/src/websocket.rs:243,249` (serde via `serde_wasm_bindgen`) | yes, survives hibernation |
| `get_websockets` / `get_websockets_with_tag` | yes | `worker-0.8.7/src/durable.rs:380,389` | yes, returns 1 inside the constructor after wake |
| `websocket_message` / `_close` / `_error` handlers | yes | trait `worker-0.8.7/src/durable.rs:1012-1037`; JS names `webSocketMessage/Close/Error` `worker-macros-0.8.7/src/durable_object.rs:83-139`. Binary arrives as `WebSocketIncomingMessage::Binary(Vec<u8>)` (one copy from `Uint8Array`, `:96-99`). | yes |
| `set_websocket_auto_response` | yes (text only, as noted in spec §2.1) | `worker-0.8.7/src/durable.rs:403` | not tested |
| Alarms | yes | `Storage::set_alarm` `durable.rs:597`, handler `durable.rs:1006`, generated `alarm` `durable_object.rs:57-68` | yes, fired after 500 ms |
| `Sec-WebSocket-Protocol` on the 101 | yes | `Response::from_websocket` `worker-0.8.7/src/response.rs:87`, then `headers_mut().set(...)` `response.rs:291` | yes, client saw `protocol="iroh-relay-v1"` |
| Read request headers (Host, Upgrade, custom) | yes | `Request::headers()` | yes |
| `crypto.getRandomValues` | yes, via `getrandom` 0.3.4 with the `wasm_js` feature. On wasm32-unknown-unknown the feature alone is enough in 0.3.4 (`getrandom-0.3.4/src/backends.rs:188-192`); no `--cfg` flag needed. | random instance ids worked |
| RPC worker→DO | **no (default build)** | `Stub::into_rpc` exists (`durable.rs:76`), but worker-build wraps DO classes in a JS `Proxy` and does not extend `DurableObject` (`worker-build-0.8.7/src/main.rs:257-268`; the comment says *"The runtime only exposes RPC on classes deriving from DurableObject"*, and that path applies only to emscripten builds). Runtime error observed: `TypeError: The receiving Durable Object does not support RPC, because its class was not declared with 'extends DurableObject'`. workers-rs calls RPC "experimental" (`worker-0.8.7/src/lib.rs:128-147`). | **fails** |
| Fetch-to-stub | yes | `Stub::fetch_with_request` `durable.rs:52`; `id_from_name` `durable.rs:95` | yes, forwards the Upgrade request intact |
| `!Send` async handlers | yes | `#[allow(async_fn_in_trait)] // Send is not needed` (`durable.rs:985`). Handlers are wasm-bindgen async exports (`worker-macros-0.8.7/src/async_export.rs:43-48`). | yes |
| `wasm_bindgen_futures::spawn_local` | yes, re-exported as `worker::wasm_bindgen_futures` (`worker-0.8.7/src/lib.rs:177`) | a task spawned in `websocket_message` outlived the handler and sent a message 202 ms later via `worker::Delay` (setTimeout-backed, `worker-0.8.7/src/delay.rs:26,52`) |
| Long-lived state in the DO struct (`Cell`/`RefCell`) | yes | The struct is the wasm-bindgen object and lives as long as the JS instance. Handlers take `&self`. | yes: `msgs_seen` counter accumulated across events (1→7) until hibernation reset it |

### Caveats

1. **Panic/abort recovery re-constructs the DO in place.** On a `WebAssembly.RuntimeError`, the shim marks `criticalError`, resets wasm state, and transparently re-runs the constructor on the next property access (`worker-build-0.8.7/src/js/shim.js:20-25,55-62`). An in-memory `Endpoint` vanishes without `close()`, exactly as under hibernation. §5.5 must treat "Endpoint missing while Active" as a normal state. Consider `--panic-unwind` (`shim-unwind.js`, README §panic) for the Endpoint task.
2. **Handlers interleave.** Each event is a separate `async fn(&self)`, so awaits interleave across events. Never hold a `RefCell` borrow across `.await`. Use short borrows or a single-owner task fed by a channel (spec §5.7).
3. **Constructor is sync** (`durable.rs:990`). Use `state.block_concurrency_while` (`durable.rs:339`) for async init, or lazily instantiate the Endpoint on the first datagram (which already matches §5.5).
4. **Router→DO admin calls (§13.4 mint):** use `stub.fetch_with_request` with an internal path and a header, not RPC. Alternatives: a custom shim via `CUSTOM_SHIM` (`worker-build-0.8.7/src/main.rs:91`), or wait for workers-rs RPC support.

### worker-build packaging

`worker-build --release` produces:

```
build/index.js          # ESM main module (esbuild-bundled shim + wasm-bindgen glue, minified, 28 KB)
                        #   contains: import Z from "./index_bg.wasm";  exports default + Hello (DO class)
build/index_bg.wasm     # 440,106 B raw / 169,019 B gzip -9 (for the dohello spike)
build/worker/shim.mjs   # back-compat alias re-exporting ../index.js. Do NOT upload.
build/package.json, .gitignore
```

The upload is **two modules**: `index.js` (`application/javascript+module`, `main_module`) and `index_bg.wasm` (`application/wasm`). Evidence: `worker-build-0.8.7/src/main.rs:110-139,340-370,403-420`.

### Design impact

- The DO shell can be pure Rust. A thin JS class is **not** needed for V11.
- Replace "router calls the DO over RPC" (§13.4 step 2) with fetch-to-stub on an internal path, or plan a custom shim.
- Treat shim re-init as a third "Endpoint lost" cause, alongside hibernation and eviction.

---

## Proof: `dohello` spike

Location: `dohello`, an experiment crate not kept in the repository.

```
dohello/
  Cargo.toml        # worker =0.8.7, wasm-bindgen =0.2.129, wasm-bindgen-futures =0.4.79, js-sys =0.3.106,
                    # serde, serde_json, getrandom =0.3.4 [wasm_js]; crate-type cdylib; release opt-level "s", lto
  wrangler.toml
  src/lib.rs        # router + DO (below)
  client.mjs        # node global WebSocket client: subprotocols, binary echo, scripted commands with waits
  rawws.mjs         # node:http raw upgrade to 127.0.0.1 with arbitrary Host/path; prints 101 headers
  build/            # worker-build output (see above)
```

`wrangler.toml`:

```toml
name = "dohello"
main = "build/index.js"
compatibility_date = "2026-09-15"   # wrangler 4.130.0's workerd rejects later dates locally

[build]
command = "worker-build --release"

[[durable_objects.bindings]]
name = "HELLO"
class_name = "Hello"

[[migrations]]
tag = "v1"
new_sqlite_classes = ["Hello"]
```

Run: `WRANGLER_SEND_METRICS=false CI=1 wrangler dev --port 8799 --ip 127.0.0.1 --inspector-port 9799`

<details><summary><code>src/lib.rs</code> (key parts)</summary>

```rust
/// Path form: `/e/<label>/...`
fn path_label(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("/e/")?;
    let (label, tail) = rest.split_once('/').unwrap_or((rest, ""));
    if label.is_empty() { return None; }
    Some((label.to_ascii_lowercase(), format!("/{tail}")))
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let host = req.headers().get("host")?.unwrap_or_default();
    let url = req.url()?;
    let (label, via) = match path_label(url.path()) {
        Some((l, _)) => (l, "path"),
        None => match label_of(&host) { Some(l) => (l, "host"),
            None => return Response::error("no endpoint label", 400) },
    };
    let stub = env.durable_object("HELLO")?.id_from_name(&label)?.get_stub()?;
    // (/rpc branch: stub.into_rpc::<HelloRpc>().rpc_hello(..) -> fails, see V11)
    stub.fetch_with_request(req).await           // forwards Upgrade + Host intact
}

#[durable_object]
pub struct Hello { state: State, env: Env, instance: String,
                   msgs_seen: Cell<u64>, log: RefCell<Vec<String>> }

impl DurableObject for Hello {
    fn new(state: State, env: Env) -> Self {
        let instance = rand_hex(4);                                  // getrandom -> crypto.getRandomValues
        let sql = state.storage().sql();
        sql.exec("CREATE TABLE IF NOT EXISTS ctor(id INTEGER PRIMARY KEY, instance TEXT, at REAL)", None).unwrap();
        sql.exec("INSERT INTO ctor(instance, at) VALUES (?, ?)", vec![instance.clone().into(), now_ms().into()]).unwrap();
        console_log!("[do] CONSTRUCTOR instance={instance} ... live_websockets={}", state.get_websockets().len());
        Self { state, env, instance, msgs_seen: Cell::new(0), log: RefCell::new(vec![]) }
    }
    async fn fetch(&self, req: Request) -> Result<Response> {
        let proto = req.headers().get("sec-websocket-protocol")?
            .and_then(|p| p.split(',').next().map(|s| s.trim().to_string()));
        let pair = WebSocketPair::new()?;
        self.state.accept_websocket_with_tags(&pair.server, &[&label, "peer"]);
        pair.server.serialize_attachment(Attach { label, host, proto: proto.clone(), connected_at: now_ms(), nonce: rand_hex(8) })?;
        let mut resp = Response::from_websocket(pair.client)?;
        if let Some(p) = proto { resp.headers_mut().set("Sec-WebSocket-Protocol", &p)?; }
        Ok(resp)
    }
    async fn websocket_message(&self, ws: WebSocket, msg: WebSocketIncomingMessage) -> Result<()> {
        self.msgs_seen.set(self.msgs_seen.get() + 1);
        match msg {
            WebSocketIncomingMessage::Binary(b) => ws.send_with_bytes(&b)?,          // echo
            WebSocketIncomingMessage::String(s) => match s.as_str() {
                "attach" => /* ws.deserialize_attachment::<Attach>() */,
                "info"   => /* instance, msgs_seen, ctor_count (sql), get_websockets().len(), get_tags */,
                "busy"   => /* Date.now()/performance.now() deltas around a 300M-iteration loop */,
                "spawn"  => /* wasm_bindgen_futures::spawn_local(Delay 200ms; ws.send) */,
                "alarm"  => /* storage().set_alarm(500) */,
                ..
            }
        }
        Ok(())
    }
    async fn alarm(&self) -> Result<Response> { /* send to all get_websockets() */ }
    async fn websocket_close(..) / websocket_error(..)
}

#[wasm_bindgen] impl Hello {                       // extra method: NOT reachable via DO RPC
    #[wasm_bindgen(js_name = "rpcHello")] pub async fn rpc_hello(&self, s: String) -> String { .. }
}
```
</details>

### Results (wrangler dev, 127.0.0.1:8799)

```
# path form, 54-char label
$ node client.mjs ws://127.0.0.1:8799/e/k51qzi5uqu5dh9ihdfafkfg8bbmoc7s1hejf1kqs2wl4ex6ylxkxdh/relay bin attach info close
open protocol= "iroh-relay-v1"                        <- subprotocol echoed on 101
recv binary(5) 010203feff                             <- binary echo
recv attach {"connected_at":...,"host":"127.0.0.1:8799","label":"k51qzi...","nonce":"88b4...","proto":"iroh-relay-v1"}
recv info instance=2dcaa934 msgs_seen_in_memory=3 ctor_count=2 websockets=1 tags=["k51qzi...", "peer"]

# host form (before the scope change): same flow to ws://<54-char>.localhost:8799/relay, plus
recv spawned done after 202ms                         <- spawn_local + setTimeout
recv alarm fired instance=db26220b                    <- alarm, same in-memory instance
recv busy date_delta=4552 perf_delta=4552 perf_now=1792600310728   <- V12

$ curl http://127.0.0.1:8799/e/<E>/rpc
rpc rejected: TypeError: The receiving Durable Object does not support RPC, because its class was not declared with `extends DurableObject`...
```

The hibernation run, with the attachment surviving (`hib.out`):

```
[+37ms]     open protocol= "iroh-relay-v1"
[+110ms]    info instance=f894ac90 msgs_seen_in_memory=1 ctor_count=1
            sleep 5000
[+5181ms]   info instance=f894ac90 msgs_seen_in_memory=2 ctor_count=1      <- still resident after 5 s idle
            sleep 12000
[+17197ms]  info instance=99080567 msgs_seen_in_memory=1 ctor_count=2      <- hibernated; ctor re-ran on message
[+17203ms]  attach {"connected_at":1792600332680.0,...,"nonce":"a44576810d4eaa3e"}   <- attachment intact
            sleep 30000
[+47208ms]  info instance=a60b26e6 msgs_seen_in_memory=1 ctor_count=3
            sleep 75000
[+122221ms] info instance=d4a17c86 msgs_seen_in_memory=1 ctor_count=4
[+122224ms] attach {... same nonce a44576810d4eaa3e ...}
[+122227ms] recv binary(5) 010203feff                                       <- same socket, still open
wrangler log: [do] CONSTRUCTOR instance=99080567 ctor_count=2 live_websockets=1 ...
```

---

## V12: local clock and forcing hibernation

**Answer.**

1. **Not frozen locally.** A 300M-iteration loop inside one `websocket_message` measured `Date.now()` delta = 4552 ms and `performance.now()` delta = 4552 ms. `performance.now()` returned an epoch value (1.79e12), so `timeOrigin == 0` as in production. Cloudflare documents the same: "In local development, however, timers will increment regardless of whether I/O happens or not" (https://developers.cloudflare.com/workers/runtime-apis/performance/). Production locks `Date.now()` while code runs (https://developers.cloudflare.com/workers/reference/security-model/).
2. **Local wrangler dev hibernates.** An idle DO with only hibernatable WebSockets was re-constructed on the next message after ≥12 s idle, but not after 5 s. This is consistent with the documented production rule of "after 10 seconds of inactivity" (https://developers.cloudflare.com/durable-objects/concepts/durable-object-lifecycle/). Local hibernation exists since wrangler 3.13.2 / Miniflare v3.20231016.0 (websockets best-practices page). The socket, tags, attachment and SQLite survived; in-memory `Cell`/`RefCell` state reset. `get_websockets()` already works inside the constructor after wake.
3. **Forcing it in tests.** The `cloudflare:test` API (https://developers.cloudflare.com/workers/testing/vitest-integration/test-apis/) provides:
   - `evictDurableObject(stub, { webSockets: "hibernate" | "close" })` (default: hibernate the sockets), and `evictAllDurableObjects()`;
   - `abortAllDurableObjects()`;
   - `runDurableObjectAlarm(stub)`, `runInDurableObject`, `listDurableObjectIds`, `reset()`.

   Cloudflare is moving users from `@cloudflare/vitest-pool-workers` to `@cloudflare/vitest-plugin` (≥1.0.0 for some APIs).

**Design impact.**

- §5.4: production and local clock semantics **differ**. Local is monotonic real time; production is frozen. The sans-io argument covers both, so tests must not assume a frozen clock. For deterministic tests, keep the option of injecting time.
- §5.5 "Warm is hibernatable" can be tested end to end without a fork:
  - **plain wrangler dev:** wait ≥10 s and assert the constructor count rises (cheap, used here);
  - **vitest-plugin:** use `evictDurableObject(..., {webSockets:"hibernate"})` for deterministic tests.

  Assert hibernation on SQLite-backed counters, not in-memory ones.
- Pending `setTimeout`/`spawn_local` tasks block hibernation (spec §2.1). The same harness can check that the `T_linger` teardown really leaves zero timers: if a test sees no constructor re-run after idle, the DO was pinned.

---

## V15: certificates (brief; avoided by path form)

- Universal SSL covers the apex and first-level subdomains only. `*.e.zone` (two levels) is **not** covered: "Deeper subdomains ... are not covered" (https://developers.cloudflare.com/ssl/edge-certificates/universal-ssl/limitations/).
- Covering it needs **Advanced Certificate Manager** at $10/mo per zone (https://www.cloudflare.com/plans/). With ACM you either order an advanced cert listing `*.e.zone`, or enable Total TLS (ACM-only) (https://developers.cloudflare.com/ssl/edge-certificates/advanced-certificate-manager/, .../additional-options/total-tls/).
- Label length: a 52-char label is under the 63-char label limit. A wildcard cert never contains the label, so the 64-char CN limit does not apply.
- Workers Custom Domains do not support wildcards. Routes allow a leading `*.` (`*.e.example.com/*`). The limit is 1,000 routes per zone.
- **Decision (note 001):** use path form under `iroh-do.example.com`, which Universal SSL covers on the Free plan. No certificate resource and no SSL token permission.

---

## V17: OpenTofu + Cloudflare provider 5.28.1

**Answer: everything required is supported. The §13.3 `wrangler deploy` fallback is not needed.**

From the provider schema (`tofu providers schema -json`, provider 5.28.1):

- **`cloudflare_workers_script`:**
  - `main_module` ("Indicates a module syntax Worker").
  - `content_file` + `content_sha256` ("Must be provided when content_file is specified").
  - `files` = map of `{content_file | content_base64, content_type (required), content_sha256 (computed)}`, described as "Additional modules and data files to include in the multipart Worker upload. Map keys are multipart part names ... referenced by ... module imports".
  - `compatibility_date`, `compatibility_flags` (set).
  - `bindings[].type` includes `durable_object_namespace` (with `class_name`, optional `script_name`), `secret_text` / `plain_text` (with `text`), and `wasm_module`.
  - `migrations { new_tag, old_tag, new_sqlite_classes, new_classes, deleted_classes, renamed_classes, transferred_classes, steps[] }`.
  - `assets`, `observability`, `force`.
  - `exports` exists but is described only as per-entrypoint cache config. The new declarative DO `exports` lifecycle (which Cloudflare now calls the replacement for "legacy" `migrations`) is **not clearly exposed**, so stay on `migrations`. Cloudflare: "Once a Worker has been deployed with `exports`, subsequent deploys cannot return to the legacy `migrations` array."
- **Multipart semantics** (provider v5.28.1 source):
  - `model.go:66-104`: the main module part is named `main_module`, with content type `application/javascript+module`; each `files` key becomes a part with its `content_type`.
  - `resource.go:103-111`: `content_file` is read into `content`.
  - `resource.go:578-606`: `content_file` under `files` is read and base64-encoded; `metadata` and the main module name are reserved part names.
  - So `files = { "index_bg.wasm" = { content_type = "application/wasm" } }` satisfies `import ... from "./index_bg.wasm"`.
- **`cloudflare_workers_route`:** `{zone_id, pattern, script}`.
- **`cloudflare_workers_custom_domain`:** `{account_id, hostname, service, zone_id|zone_name, environment}`, plus computed `cert_id`.
- **`cloudflare_dns_record`:** `{zone_id, name, type, content, proxied, ttl (required; 1 = auto), comment, tags}`.

**Route versus custom domain.** I recommend **`cloudflare_workers_custom_domain`**:

- one resource, with no placeholder `AAAA 100::` record;
- the Worker is the origin;
- Cloudflare says custom domains are "a recommended replacement" for `example.com/*` + `100::` routes (https://developers.cloudflare.com/workers/configuration/routing/custom-domains/).

Permissions:

- The API reference for "Attach Worker to Domain" lists only **`Workers Scripts Write`** under accepted permissions (https://developers.cloudflare.com/api/resources/workers/subresources/domains/methods/update/). So no DNS:Edit, Workers Routes:Edit or SSL permission should be needed, even though Cloudflare creates the DNS record and issues a certificate on your behalf.
- **This is unverified without credentials.** Confirm it in the first `tofu apply` with a token scoped to Workers Scripts:Edit only.

Caveats:

- "deleting a Custom Domain does not delete its certificate". `tofu destroy` leaves an edge certificate behind, and the nightly "no leftovers" check must allow for it or delete it.
- Creation fails if a CNAME already exists at the hostname.

Fallback: `attach = "route"` (proxied AAAA `100::` + route `iroh-do.example.com/*`). It needs DNS:Edit + Workers Routes:Edit (zone) and tears down cleanly.

**Validated snippet** (`tofu fmt -check` OK; `tofu validate` reports "Success! The configuration is valid.").

`tofu plan -refresh=false` with a dummy token and dummy IDs gives "Plan: 2 to add" for custom_domain and "3 to add" for route. Planning makes no API calls. Scratch dir: `tofu` (a throwaway crate, not kept in the repository).

```hcl
terraform {
  required_version = "= 1.13.1"
  required_providers {
    cloudflare = {
      source  = "cloudflare/cloudflare"
      version = "= 5.28.1"
    }
  }
}

variable "account_id" { type = string }
variable "zone_id" { type = string }
variable "hostname" {
  type    = string
  default = "iroh-do.example.com"
}
variable "worker_bundle_dir" {
  type    = string
  default = "./bundle"
}
variable "compatibility_date" {
  type    = string
  default = "2026-09-15"
}
variable "admin_token" {
  type      = string
  sensitive = true
}
# "custom_domain" (recommended) or "route" (proxied AAAA 100:: + workers route)
variable "attach" {
  type    = string
  default = "custom_domain"
  validation {
    condition     = contains(["custom_domain", "route"], var.attach)
    error_message = "attach must be custom_domain or route"
  }
}

locals {
  main_js = "${var.worker_bundle_dir}/index.js"
  wasm    = "${var.worker_bundle_dir}/index_bg.wasm"
}

resource "cloudflare_workers_script" "iroh_do" {
  account_id  = var.account_id
  script_name = "iroh-do"

  # worker-build output: ESM entry + the wasm module it imports as "./index_bg.wasm"
  main_module    = "index.js"
  content_file   = local.main_js
  content_sha256 = filesha256(local.main_js)
  files = {
    "index_bg.wasm" = {
      content_file = local.wasm
      content_type = "application/wasm"
    }
  }

  compatibility_date  = var.compatibility_date
  compatibility_flags = []

  bindings = [
    { type = "durable_object_namespace", name = "ENDPOINTS", class_name = "IrohEndpoint" },
    { type = "secret_text", name = "ADMIN_TOKEN", text = var.admin_token },
    { type = "plain_text", name = "PUBLIC_BASE", text = "https://${var.hostname}/e/" },
  ]

  migrations = {
    new_tag            = "v1"
    new_sqlite_classes = ["IrohEndpoint"]
  }

  observability = { enabled = true }
}

resource "cloudflare_workers_custom_domain" "iroh_do" {
  count      = var.attach == "custom_domain" ? 1 : 0
  account_id = var.account_id
  zone_id    = var.zone_id
  hostname   = var.hostname
  service    = cloudflare_workers_script.iroh_do.script_name
}

resource "cloudflare_dns_record" "iroh_do" {
  count   = var.attach == "route" ? 1 : 0
  zone_id = var.zone_id
  name    = var.hostname
  type    = "AAAA"
  content = "100::"
  proxied = true
  ttl     = 1
  comment = "iroh-do example (Worker route target; managed by OpenTofu)"
}

resource "cloudflare_workers_route" "iroh_do" {
  count   = var.attach == "route" ? 1 : 0
  zone_id = var.zone_id
  pattern = "${var.hostname}/*"
  script  = cloudflare_workers_script.iroh_do.script_name
}

output "endpoint_url_template" {
  value = "https://${var.hostname}/e/{E}/"
}
```

(The bindings list is shown here in compact one-line form; the validated file uses the multi-line form produced by `tofu fmt`.)

**Open points, to check on the first real apply:**

1. **Wasm-only rebuilds and plan diffs.** `files[*].content_sha256` is *computed*. A rebuild that changes only the wasm may not produce a plan diff unless the provider re-hashes at plan time. For R3, also feed `filesha256(local.wasm)` into something that shows up in the plan, for example a `plain_text` binding `BUNDLE_SHA256 = sha256(join(filesha256(js), filesha256(wasm)))`.
2. **Migration tags across applies.** Check that `migrations` with `new_tag = "v1"` is accepted on re-apply when the remote tag is already v1 (zero diff expected; the provider computes `migration_tag`).
3. **State encryption.** `secret_text.text` is sensitive and lands in state, so §13.3 state encryption still applies.

---

## V18: local mode reachability

**Answer: yes.**

- **Path form (now the default):** a native (node `http`, non-browser) client sent a raw HTTP/1.1 upgrade to `http://127.0.0.1:8799/e/<E>/relay`. It got back `101`, `upgrade: websocket`, a valid `sec-websocket-accept`, and `sec-websocket-protocol: iroh-relay-v1`. The node `WebSocket` client to the same URL completed echo and attachment round trips.
- **Host preservation (now moot):** wrangler dev in local mode does **not** rewrite Host. The router saw:
  - `abc.localhost:8799` and `abc.lvh.me:8799` (both routed to the same DO);
  - an explicit `Host: rawhost.e.example.com` sent to 127.0.0.1 (`req.url()` became `http://rawhost.e.example.com/relay`).
- **Resolution on macOS:** `*.localhost` resolves via the system resolver (getaddrinfo → `::1`, `127.0.0.1`; `dscacheutil` agrees). Note that wrangler bound only 127.0.0.1 with `--ip 127.0.0.1`. A client that tries `::1` first will fall back after a refused connection. Path form with a literal `127.0.0.1` avoids this.

**Design impact:**

- `just local` should use `http://127.0.0.1:<port>/e/<E>/`.
- Whether stock iroh accepts an `http://` relay URL is a separate client-side question, outside this document (V18 is iroh-side).

---

## V19: deleting the Worker

**Answer.**

- **Deleting the script deletes its DO namespaces and data.** Provider 5.28.1 `schema.go:62-63` documents the `force` delete flag, a Cloudflare API parameter: *"If true, delete the Worker even when other Workers still reference it. ... Durable Object namespaces implemented by the deleted Worker are deleted even if other Workers reference them."* `Delete` calls `Workers.Scripts.Delete` with `force` (`resource.go:608-636`).
- `wrangler delete`: "Delete your Worker and all associated Cloudflare developer platform resources."
- Class-level deletes:
  - With `exports` deletes: "Deleting a class removes its namespace and all of its stored data permanently — this is not a soft delete ... There is no Trash" (https://developers.cloudflare.com/durable-objects/reference/durable-objects-migrations/).
  - With legacy `deleted_classes`: "will delete all Durable Objects ... including all of their stored data."
- **Re-creating under the same name does not restore data.** A new namespace is created by the new `new_sqlite_classes` migration. No document describes reuse, so treat it as orphaned or gone. Untested; it needs an account.

**Design impact:**

- `just down` (`tofu destroy`) **destroys every endpoint identity**. Keep the §13.6 warning, and consider `lifecycle { prevent_destroy = true }` on the script, with a separately named `just destroy-identities`.
- Never rename the class or script without a `renamed_classes` / `transferred_classes` migration.

---

## Limits and pricing (current docs, 2026-10-21)

| Item | Value | Source |
|---|---|---|
| Worker size | **64 MiB uncompressed, Free and Paid; no compressed limit** (the old 3/10 MB gzip limits are gone) | https://developers.cloudflare.com/workers/platform/limits/ |
| Startup | global scope must parse and execute within 1 s (error 10021) | same |
| DO WebSocket message (received) | 32 MiB | https://developers.cloudflare.com/durable-objects/platform/limits/ |
| `serializeAttachment` | 16,384 bytes max serialized | https://developers.cloudflare.com/durable-objects/best-practices/websockets/ |
| Hibernatable WS per DO | 32,768; tags ≤10 per socket, ≤256 chars each | https://developers.cloudflare.com/durable-objects/api/state/ |
| Storage | 10 GB per DO; account unlimited (Paid) / 5 GB (Free). Free plan: SQLite-backed DOs only. | DO limits |
| Routes per zone | 1,000 | Workers limits |
| DO requests | Free 100k/day; Paid 1M/mo + $0.15/M. **Incoming WS messages billed 20:1**; outgoing messages and protocol pings free. | https://developers.cloudflare.com/durable-objects/platform/pricing/ |
| DO duration | Free 13,000 GB-s/day; Paid 400,000 GB-s/mo + $12.50/M GB-s, billed at 128 MB regardless of use. Idle objects eligible for hibernation are not billed for duration, even before they hibernate. | same |
| SQLite | rows read 25 B/mo incl. + $0.001/M; rows written 50 M/mo incl. + $1.00/M; storage 5 GB-mo + $0.20/GB-mo | same |
| Workers Paid | $5/mo minimum | https://developers.cloudflare.com/workers/platform/pricing/ |
| Production hibernation | after 10 s inactivity; non-hibernatable idle eviction 70–140 s | DO lifecycle page |

**Design impact:**

- The spike wasm is 440 KB. Even an iroh build of several MB is far below 64 MiB. Watch the 1 s startup instead: wasm compile and instantiate happen on every cold start.
- The 16 KiB attachment holds the per-socket peer state (EndpointId, protocol version, auth result) with room to spare.
- At 20:1, each relayed QUIC datagram batch costs 1/20 of a request. Client pings cost a wake but are free only if they are WS protocol pings, not relay-protocol Ping frames (feeds V6).
