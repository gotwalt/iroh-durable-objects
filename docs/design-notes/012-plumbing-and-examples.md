# 012: Plumbing and examples in separate trees

**Status:** accepted (2026-10-22). Amends 006 and 011 (crate layout).

## Decision

The repository separates what iroh-do provides from what is built on it. The
rule: nothing outside `examples/` names an example app.

| Path | Was | Holds |
|---|---|---|
| `crates/iroh-relay-ingress`, `iroh-ingress-transport`, `iroh-do-core`, `iroh-do` | same | The library |
| `crates/iroh-do-harness` | `harness/native` | The native test host, which app authors use too |
| `deploy/build-worker.sh`, `deploy/BUILD.md` | `crates/iroh-do-worker/` | Builds the bundle of any deployment crate, given its path |
| `deploy/tofu/` | `infra/` | An OpenTofu module for any deployment; no provider configuration, no state |
| `tests/workerd/` | `harness/workerd/` | Miniflare tests of the library on workerd |
| `examples/chat/app` | `apps/chat` | The chat app |
| `examples/chat/client` | `crates/iroh-do-demo` | The chat client; the package is renamed `iroh-do-chat`, after its binary |
| `examples/echo/app`, `examples/echo/client` | `apps/echo`, `apps/echo-client` | The echo app and its stock client |
| `examples/deployment` | `crates/iroh-do-worker`, `harness/workerd/wrangler.toml` | This repository's deployment crate and its `wrangler.toml` |
| `examples/deployment/infra` | `infra/` | The OpenTofu root module: provider, state encryption, the module call, variables and state |

Tests follow the rule. The chat's integration tests (`node.rs`, `room.rs`, and
the dial-back test of `persistence.rs`) move to `examples/chat/app/tests`, with
`iroh-do-harness` as a dev-dependency. The harness keeps the tests that need no
example: `library.rs`, which now carries its own copy of iroh's echo handler,
and the destination-violation test, which runs an app with no protocols.

One exception: `tests/workerd/` drives the example bundle and the example
clients, because a workerd test needs a bundle and a peer. What those tests
check is the library's behaviour (eviction, the timer fix, destroy, the
registry), so they stay outside `examples/`.

## OpenTofu

`infra/` was already generic: script name, zone, hostname prefix and bundle
directory were variables. Only the variable values, the encrypted state and the
passphrase tied it to this deployment. It is split along that line: the module
in `deploy/tofu` declares the four resources and the script-list lookup; the
root in `examples/deployment/infra` configures the provider and encryption,
calls the module, and passes its variables. `worker_bundle_dir` has no default
in the module; the root passes `../../../build/worker`.

The resources keep their state through `moved` blocks in the root, from
`cloudflare_*.…` to `module.iroh_do.cloudflare_*.…`. A plan after the move shows
the four moves and one in-place update of the script: the provider stores
`content_file` as written, and the path changed from `../build/worker/index.js`
to `../../../build/worker/index.js`. Applying it uploads the same bundle again,
which restarts live objects but keeps their storage.

### Cleanup

The split was also a chance to remove what earlier workarounds had left:

- **The module's files.** `main.tf` is split into `worker.tf` (the script and
  its migration lookup) and `hostnames.tf` (DNS record, route, certificate).
- **The migration.** The lookup that sends the migration only while the live
  script lacks it stays: provider 5.28.1 sends `migrations` from the
  configuration on every update (`resource.go`, Update), and the API rejects a
  migration whose `old_tag` is not the current tag. What goes is the second
  apply `just up` ran to clear the diff left after the first deploy, when the
  configuration stops sending the migration; `ignore_changes = [migrations]`
  suppresses that diff instead. This is from the provider source, not from a
  fresh deploy.
- **The route.** It is replaced with the script (`replace_triggered_by`), so a
  replaced script no longer leaves an update of a deleted route (note 011,
  phase 5).
- **The certificate output.** `certificate_status` changed from `pending` to
  `active` after the first deploy, which made `just plan` report a change, so
  `just up` ran a refresh-only apply. The output is removed; `just up` already
  waits on TLS.
- **The root's variables.** The root declared all twelve module inputs again,
  with their validations. It now declares the three it sets, the admin token
  and its state passphrase; the module's defaults apply to the rest.
- **Saved plans.** `just plan` saves its plan to `.demo/plan.tfplan`, and
  `just apply` applies that file and nothing else.

## Results

- `cargo test` for `iroh-relay-ingress`, `iroh-do`, `iroh-do-harness` and
  `iroh-do-chat-app`: all pass.
- `node tests/workerd/{evict,clock,destroy,registry}-test.mjs`: all pass.
- `LOCAL_PORT=8799 just local`: all checks pass.
- `just infra-check` (fmt and validate of the root with the module): passes.
- `just plan` against production after the cleanup: three moves, the script's
  in-place update (bundle path, and the wasm rebuilt from the new layout), and
  the removed `certificate_status` output. The fourth move, of the script
  itself, shows as its update.
