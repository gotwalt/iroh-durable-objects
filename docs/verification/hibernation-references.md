# Forced eviction after the Endpoint ran ("active references")

**Date:** 2026-10-21. **Runtime:** Miniflare `5.20260908.0-alpha` (workerd
`1.20260908.1`), bundle from `just build-worker` (worker-build 0.8.7).
**Verdict:** not a leak. Warm is hibernatable after the Endpoint has run. The
refusal came from the test: the DO it tried to evict was still **Active**.

## Symptom

`harness/workerd/evict-test.mjs` (old case B) connected `iroh-do-demo chat`,
sent `hello` and `/status`, slept 12.5 s ("T_idle + T_linger → Warm") and then
called `unsafeEvictDurableObject(..., { webSockets: "hibernate" })`. The call
failed with:

```
Error: Timed out waiting to evict Durable Object: it still has active references.
    at _Miniflare.unsafeEvictDurableObject (miniflare/dist/src/index.js:83486)
```

## What "active references" means

`Miniflare#unsafeEvictDurableObject` (`miniflare/dist/src/index.js:83470-83487`)
calls the dev-control service, which calls `workerdUnsafe.evict(stub, {webSockets})`
(`miniflare/dist/src/workers/core/dev-control.worker.js`). The check runs inside
workerd (`Server::ActorNamespace::ActorContainer::evictWhenIdle`): it waits for
the actor to have no live references (in-flight events, pending timers, fetches,
outstanding I/O) and gives up after about **30 s** with the error above. So the
old test made a single ~30 s attempt and never retried inside its 15 s loop.

## Cause

`chat` keeps its chat/1 bidirectional stream open for the whole session. The
idle close (note 005) counts a connection as idle only when it has **no open
streams** and no application bytes for `T_idle`
(`crates/iroh-do-core/src/protocols.rs:57-64`, `:112-126`). An open stream means
the connection is never idle-closed, so the node never reaches `T_linger` and the
Endpoint is never closed. The DO stayed Active. `/status` said so
(`"state":"Active","connections":1`). The old test predicate looked for
`state=`, which the current `/status` JSON no longer contains, so the test timed
out before it got that far.

## Evidence

Test-only instrumentation wrapped `setTimeout`/`clearTimeout`/`setInterval`/`fetch`
in the bundle and served the outstanding ones over a `/__timers` hook in the
router. It is kept in `evict-test.mjs` behind `EVICT_DIAG=1`.

**Chat session open, 42 s after connect (eviction refused):** 6 timers, 0 fetches.

| delay | owner (wasm frames) |
|---|---|
| 4 999 ms, re-armed every 5 s | `iroh::runtime::web::Timer as noq::runtime::AsyncTimer::reset` (QUIC per-path keep-alive/loss timer, note 005) |
| 10 000 ms, re-armed every 10 s | `iroh_do_core::protocols::handle_incoming` idle loop (`sleep(t_idle)` while a stream is open) |
| 14 998, 21 569, 59 988 ms | `n0_future::time::wasm::Interval::tick` in Endpoint-internal tasks |
| 60 000 ms | `n0_future::time::wasm::Sleep::reset` in an Endpoint-internal task |

All of these are expected while Active. None is left behind after teardown.

**Real Warm** (`iroh-do-demo listen`: chat/1 hello, then the peer closes its
connection but keeps its relay socket; wait 4 s for T_linger, `endpoint.close()`
and drop): **0 timers, 0 fetches. Forced eviction succeeded in 4–9 ms.** The
dial-back alarm then woke the evicted DO (`wakes` +1), and it dialled the peer
with notify/1 over the hibernated socket.

There are no pending fetches in either state. Locally `PKARR_RELAY` is empty, and
pkarr publishing is driven by alarms, not timers. wasm-bindgen-futures holds no
unresolved promise that keeps the actor referenced: eviction succeeds with the
same wasm instance that ran the Endpoint.

## Fix

No runtime change is needed and nothing upstream is implicated.

- `harness/workerd/evict-test.mjs`: case B now asserts `/status` (JSON) reports
  Active and makes no eviction attempt. The new **case C** is the regression test:
  1. `listen --callback 10 --once` connects and runs a session.
  2. The test asserts a fresh Endpoint build (`endpoint_builds` +1) and waits for Warm.
  3. Forced eviction must succeed within 5 s on the first attempt.
  4. The dial-back must arrive from a re-instantiated DO (`wakes` +1).

  `EVICT_DIAG=1` prints outstanding timers and fetches in Warm and on any refusal.
- `harness/workerd/test.sh`: the chat `/status` check now matches the JSON
  (`"state":"Active"`). It also had the stale `state=` pattern.

Run: `PORT=8833 node harness/workerd/evict-test.mjs` → ALL PASS (with and
without `EVICT_DIAG=1`).

## Production risk

- **Warm hibernation is not at risk:** in Warm there are no timers, fetches or
  tasks, and workerd evicts immediately.
- **Policy consequence (by design, worth stating):** a peer that holds a QUIC
  stream open, even silently, keeps the DO **Active** indefinitely. noq's 5 s
  timer and the 10 s idle-check timer then pin it in memory, billed as duration.
  If that cost matters, the idle close would need a cap on silent open streams.
  That is a note 005 policy knob, not a bug.
