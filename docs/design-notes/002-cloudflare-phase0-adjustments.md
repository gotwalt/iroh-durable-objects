# 002: Cloudflare adjustments from Phase 0

**Status:** accepted (2026-10-21). No §1 principle changes.
**Evidence:** `docs/verification/cloudflare.md` (V11, V12, V17, V19).

1. **Router → DO calls use `stub.fetch` on an internal path, not RPC.**
   worker-build 0.8.7 wraps DO classes in a JS Proxy rather than
   `extends DurableObject`, so RPC fails at runtime. §13.4 mint becomes
   `POST /admin/mint` → router → `stub.fetch("https://do/_internal/mint")`.
   The internal path is reachable only via the stub (the router never forwards
   `/_internal/*` from the public internet).
2. **Local clock is not frozen** (workerd advanced 4.5 s inside one busy loop).
   §5.4 already admits either behaviour; tests must not assume a frozen clock.
3. **A wasm panic re-runs the DO constructor in place.** An in-memory Endpoint
   can therefore disappear while Active. The constructor must always rebuild
   from SQLite + `getWebSockets()` attachments. This is the §5.8 abrupt-eviction
   path; no new mechanism.
4. **(Superseded by note 001: wildcard route + ACM.)** ~~Infra uses `cloudflare_workers_custom_domain`~~ for
   `iroh-do.example.com` (provider 5.28.1). Fallback: proxied AAAA `100::`
   + `cloudflare_workers_route`. The custom domain's edge certificate outlives
   `tofu destroy`; the no-leftovers check allows for it.
5. **The bundle hash is a `plain_text` binding** (`BUNDLE_SHA256`), so a
   wasm-only rebuild always produces a plan diff (R3), and the DO can report
   its build in debug output.
6. **`tofu destroy` deletes the DO namespace and every endpoint identity**
   (V19). `just down` must warn and require confirmation.
