# Instructions for coding agents

iroh-do runs an iroh 1.x endpoint inside a Cloudflare Durable Object, reachable
by stock iroh peers. Read `README.md`, then `docs/design.md` (the design as
built). `docs/spec.md` is the original plan and defines the principles P1–P8
that reviews refer to; `docs/design-notes/` records every change to it.

## Rules

- **Stock iroh only.** Every crate builds against published iroh `=1.4.0`.
  Do not add `[patch.crates-io]` or path or git dependencies on iroh, and do
  not wire in `upstream/`. Read `upstream/AGENTS.md` before touching that
  directory.
- **The demo peer stays stock** (P3). `crates/iroh-do-demo` uses iroh with
  default features and default discovery. Do not change that to make a test
  pass.
- **Design changes need a design note** in `docs/design-notes/`, numbered
  next in sequence, and `docs/design.md` must be updated to match. Changes
  that touch a principle in `docs/spec.md` §1 need the user's review first.
- **Releases belong to the user.** `just release VERSION` signs with the
  user's Developer ID, notarizes, tags and publishes to GitHub. Run it only
  when asked; `just release-dry VERSION` builds and signs without publishing.
  The package `iroh-do-demo` builds the binary `iroh-do-chat`.
- **Production belongs to the user.** Do not run `tofu apply`, `just up` or
  `just down` against Cloudflare without the user's approval for that run.
  Save the plan with `-out` and show it first. Credentials come from the
  environment; never print token values.
- **Endpoint ids in URLs and DNS labels** use `PublicKey::to_z32` and
  `PublicKey::from_z32`, never `Display` or `FromStr` (design note 004).

## Building and testing

- Build one crate at a time: `cargo build -p <crate>`. `crates/iroh-do`
  builds only for wasm32, through `just build-worker`, which sets the llvm
  `clang` and `llvm-ar` that ring needs on macOS. With Apple's `ar` the module
  links but fails at instantiation.
- Native binaries run through `scripts/macos-sign-run.sh`, configured as the
  cargo runner, which signs them so the macOS firewall does not prompt. Start
  them with `cargo run`, `cargo test` or that script.
- Tests:
  - `cargo test -p iroh-relay-ingress`: protocol conformance against the
    upstream codec and a stock client.
  - `cargo test -p iroh-do-harness`: the full node in a tokio host against
    stock peers.
  - `node harness/workerd/evict-test.mjs`: forced eviction in Miniflare.
  - `LOCAL_PORT=8799 just local`: the scripted session on workerd. Port 8787
    is taken by another program on the maintainer's machine.

## Writing

Documentation and comments are read by iroh maintainers. Write plain, exact
statements. Use the terms as `docs/design.md` uses them (endpoint, peer,
object, ingress, Cold/Warm/Active, dial-back). Give numbers only when they
were measured, and say where. Comments explain why, not what the next line
does.
