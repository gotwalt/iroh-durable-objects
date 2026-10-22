# Building the iroh-do Worker bundle

The bundle is one Worker script, `iroh-do`: the router `fetch` handler
and the Durable Object classes `endpoint` (one object per endpoint) and `registry`
(spec §13.2, design note 009). `wrangler dev` runs it locally
(`tests/workerd/`) and OpenTofu uploads the same files to production
(`deploy/tofu/`, through `examples/deployment/infra/`).

## Command

```sh
deploy/build-worker.sh        # or: just build-worker
```

Requirements: Rust 1.93.0 with `wasm32-unknown-unknown` (`rust-toolchain.toml`),
`worker-build` 0.8.7 (`cargo install worker-build --version 0.8.7 --locked`),
node for the import check, and on macOS Homebrew LLVM.

On macOS the script sets `CC_wasm32_unknown_unknown` and
`AR_wasm32_unknown_unknown` to Homebrew's `clang` and `llvm-ar`. ring compiles C
for wasm32, and Apple's `ar` silently writes empty archives for it
(`docs/verification/iroh-endpoint.md`, V7). The resulting module imports
unresolved `env.*` symbols, so the script fails if the wasm imports anything
other than wasm-bindgen glue.

`worker-build --release` uses the workspace release profile (`opt-level = "z"`,
LTO, `codegen-units = 1`, `panic = "abort"`) and runs `wasm-opt`.

## Output

```
build/worker/                  # <repo>/build/worker, gitignored
  index.js                     # ESM main module (shim + wasm-bindgen glue); imports "./index_bg.wasm"
  index_bg.wasm                # the wasm module
  BUNDLE_SHA256                # sha256(sha256(index.js) ‖ sha256(index_bg.wasm)), hex
```

Two modules are uploaded: `index.js` as the main module
(`application/javascript+module`) and `index_bg.wasm` (`application/wasm`),
which `index.js` imports. The script deletes worker-build's `worker/shim.mjs`
alias and `package.json`, which are not part of the upload.

## TypeScript host bundle

The TypeScript host (design note 013) is built by wrangler instead of
worker-build. The deployment's build (`npm --prefix examples/ts-deployment run
build`, called by `just build-ts`) runs `wrangler deploy --dry-run --outdir
dist` and writes `manifest.json` next to the output; `just build-ts` copies
the directory to `build/iroh-ts/`. The script exports the classes `Echo`,
`Chat` and `Registry`, bound as `ENDPOINTS`, `CHAT` and `REGISTRY`, with the
migrations `v1` (`Echo`) and `v2` (`Chat`, `Registry`):

```
build/iroh-ts/
  worker.js                              # ESM main module; imports "./<hash>-iroh_do_wasm_bg.wasm"
  <hash>-iroh_do_wasm_bg.wasm            # named by wrangler after the wasm's content hash
  manifest.json
```

```json
{ "main_module": "worker.js",
  "modules": [ { "name": "<hash>-iroh_do_wasm_bg.wasm",
                 "path": "<hash>-iroh_do_wasm_bg.wasm",
                 "content_type": "application/wasm" } ],
  "bundle_sha256": "<hex>" }
```

`path` is relative to the manifest's directory; the main module is listed
only as `main_module`. `name` is the part name OpenTofu uploads and must equal
the import specifier in the main module without its `./`. `deploy/tofu` reads
the manifest through its `worker_manifest` input (`deploy/tofu/README.md`),
binds `bundle_sha256` as `BUNDLE_SHA256`, and checks the names against the main
module at plan time. `just bundle-info-ts` prints the hash and sizes.

The worker-build instructions above stay in force for the Rust deployment
(`module.iroh_do`) until it is retired.

## Bindings

| Name | Type | Value |
|---|---|---|
| `ENDPOINTS` | Durable Object namespace | class `endpoint` |
| `REGISTRY` | Durable Object namespace | class `registry` (design note 009) |
| `ZONE_SUFFIX` | plain text | e.g. `iroh-do.example.com`; `localhost` locally |
| `ADMIN_TOKEN` | secret | bearer token for `POST admin.<zone>/mint`; unset or empty disables minting |
| `PKARR_RELAY` | plain text | optional, default `https://dns.iroh.link/pkarr`; empty disables publishing |
| `BUNDLE_SHA256` | plain text | contents of `build/worker/BUNDLE_SHA256`, reported by `/status` on `iroh-do/ctl/1` |
| `LOG_LEVEL` | plain text | optional, `tracing` filter directives for the JSON logs; default `info,iroh=warn,noq=warn,netwatch=error,iroh_do=info` (design note 010) |

Durable Object classes: one migration, `v1`, declares `endpoint` and `registry` as
`new_sqlite_classes`. workers-rs 0.8.7's `#[durable_object]` exports the class
under the Rust type's name and has no `js_name` option, so the Rust types are
lowercase (`#[allow(non_camel_case_types)]`). Cloudflare names a namespace
`<script>_<class>`, here `iroh-do_endpoint` and `iroh-do_registry`
(design note 010). The SQLite schema inside each object has no migrations and
is created as is. Compatibility date: `2026-09-15`, the newest that wrangler
4.130.0's workerd accepts.

## Size

On 2026-10-21: `index.js` 41,975 B and `index_bg.wasm` 3,712,585 B raw
(1,431,047 B gzip -9), against a 64 MiB Workers limit. The JSON log subscriber
and the events added with it (design note 010) account for 89,219 B raw
(34,145 B gzip) of that. The script prints the exact sizes of each build.
