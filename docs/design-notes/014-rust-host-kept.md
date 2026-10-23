# 014: The Rust host stays; the TypeScript host is retired

**Status:** accepted (2026-10-22). Supersedes the decision of 013. Keeps 013's
narrowing of the sans-io principle (P5 in `spec.md` §1), which holds for the
Rust host too. The TypeScript host is preserved at the git tag
`ts-host-final`.

## What 013 built

A TypeScript `IrohDurableObject` base class owned the Durable Object. iroh,
the relay ingress and the connection lifecycle ran as Rust compiled to wasm,
driven from TypeScript, and apps were TypeScript classes using an API modeled
on iroh's Node bindings, `@number0/iroh`. It was built, reviewed, fixed and
deployed to a second Worker, `iroh-ts`, with the echo app.
`docs/verification/ts-host.md` records what was measured.

## Why it is retired

Three independent assessments compared it with the Rust host on the same apps
(echo, and the chat room pinned by the golden fixtures in
`examples/chat/common/`). None recommended keeping it as built. Their measured
points:

- **More code, in two languages.** The host-specific code was about 4,200
  lines, split between `crates/iroh-do-wasm` and `packages/iroh-do`. The Rust
  host's is about 3,000, excluding inline tests. The apps were the same size
  in both languages: the chat app is 1,824 lines of TypeScript against 1,850
  of Rust.
- **A new class of defects at the boundary.** Handing iroh connections and
  streams to JavaScript required:
  - per-isolate node state;
  - wasm instance generations and stale-handle checks after a trap;
  - stream accounting re-implemented over iroh's own;
  - connection lifetimes set by the garbage collector;
  - re-entrancy rules for callbacks.

  One day of review found eight correctness defects of this class. Rust
  ownership and iroh's `Router` rule them out in the Rust host.
- **The boundary leaked into apps.** The TypeScript chat server re-threw wasm
  traps at about 20 call sites. Its logic was tested natively only against a
  hand-written fake of the stream API. The Rust chat app is tested against
  stock iroh peers over real QUIC in the native harness.
- **It served a different audience.** A Rust host runs any iroh
  `ProtocolHandler` unchanged; iroh's `examples/echo.rs` handler is the echo
  app. A TypeScript app has to reimplement its protocol, so existing iroh
  protocols could not be hosted. This project's thesis and readers are iroh's.

The platform layer the TypeScript host had (Workers RPC, alarms with retry,
transactional mint) does not require it:

- **Outbound JavaScript APIs are reachable from Rust.** The Rust host can call
  any of them through wasm-bindgen imports (`#[wasm_bindgen] extern "C"`, or a
  JavaScript module imported with `#[wasm_bindgen(module = ...)]`). workers-rs
  is itself made of such bindings.
- **Inbound Workers RPC needs one small JavaScript class.** It cannot come from
  Rust, because worker-build 0.8.7 exports a `Proxy` that does not extend
  `DurableObject`. The class would extend `DurableObject` and forward to the
  Rust object. It is not built, because nothing needs inbound RPC: the
  router's `/_internal/*` fetch paths work.

## What carries over to the Rust host

- **Platform fixes found in review:**
  - an alarm's work is deleted only after it succeeds, and is retried with
    backoff;
  - mint is atomic;
  - pkarr requests time out;
  - routing failures answer 503.
- **Chat fixes:**
  - a bounded outbox per member;
  - `storage error` only when nothing was stored;
  - an index for room events;
  - the prune alarm armed once per quiet period;
  - a rate limiter that forgets idle peers.
- **The sans-io socket table.** The per-object socket table moves into the
  sans-io core, which removes three copies.
- **Unchanged keepers:**
  - `examples/chat/common/` (protocol spec, constants, golden fixtures) and
    its fixture runner, which also runs against a deployed Worker under
    `wrangler dev`;
  - the corrections to the OpenTofu module: `ignore_changes = [migrations]`
    removed, the second apply after a migration, and secrets that fail when
    missing.

## Removed

`packages/`, `crates/iroh-do-wasm`, `examples/chat/server`,
`examples/echo/server`, `examples/ts-deployment`, the npm workspace, the
TypeScript recipes in the `justfile`, and the `iroh_ts` module instance. The
OpenTofu module no longer has a manifest mode, because only the TypeScript
build produced manifests. The `iroh-ts` Worker in production is destroyed
through a saved plan the maintainer reviews.
