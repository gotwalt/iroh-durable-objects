# upstream/: instructions for coding agents

This directory is **reference material**: a proposed iroh patch set plus the
tests that justify it. The rest of the repository must not depend on it.
Read `README.md` here for the why; these are the rules.

## Rules

1. **Never wire it into the build.** Do not add `[patch.crates-io]`, a path
   dependency or a git dependency on `upstream/iroh` to the root `Cargo.toml` or
   to any crate under `crates/` or `harness/`. The example runs on published
   iroh 1.4.0 by design: it shows the approach needs no iroh changes (design
   note 005). The demo peer must also stay stock, and a workspace-level patch
   would silently change it too.
2. **Do not "fix" the known gaps by reaching for this patch.** If a task asks
   for 0-RTT after hibernation or fast stateless resets in the example, stop and
   tell the user it requires this patch, which the user decided to keep out of
   the example. Do not work around it inside `crates/` either.
3. **`upstream/iroh/` is a generated tree**: the crates.io `iroh-1.4.0` package
   with `patches/iroh/*.patch` applied. Change the patches, not the tree, then
   regenerate the tree (see README). Never hand-edit both and let them drift.
4. **Edit here only when asked** to work on the upstream proposal itself:
   rebasing onto a new iroh tag, addressing maintainer review, or extending the
   tests. Keep diffs minimal and in iroh's style; defaults must stay
   behaviour-identical to unpatched iroh.
5. **Build and test from inside each sub-crate**, never from the repo root:
   `restart-tests/` and `wasm-check/` are standalone workspaces.

## Commands

```sh
cd upstream/restart-tests && cargo test   # 7 tests: reset, 0-RTT after restart, replay rejection, defaults
cd upstream/wasm-check && \
  CC_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/clang \
  AR_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/llvm-ar \
  cargo build --target wasm32-unknown-unknown --release
```

## When this directory should go away

When upstream iroh ships equivalent hooks, or when the example moves to an iroh
version these patches no longer apply to and nobody is pursuing the proposal.
Deleting it is a user decision; ask first.
