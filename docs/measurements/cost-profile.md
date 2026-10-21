# Cost profile: an iroh endpoint in a Cloudflare Durable Object

Measured 2026-10-21 against the production Worker `iroh-do` (account
`65e90294…`, bundle `05a47e0ad7bd` as reported by `/status`), plus local
Miniflare/workerd runs of `build/worker/` for memory and for the CPU diagnosis.
This answers spec §5.11. Raw data: `docs/measurements/raw/`.

**Summary**

- **Cost is wall-clock duration.** An endpoint is billed 0.128 GB-s for every
  second it is Active. That is $1.6 × 10⁻⁶/s, or **$5.76 per 1,000
  connection-hours**. Requests, rows and storage are second-order.
- A **short cold session** (wake, handshake, one RPC, close, `T_linger`) costs
  **2.1–3.9 s Active ≈ 0.27–0.50 GB-s ≈ $3.4–6.2 × 10⁻⁶** of duration. That
  is about 0.8–1.5 M such sessions inside the Paid plan's 400k GB-s/month.
- **Warm and Cold cost almost nothing.** A Warm relay-ping wake is 35–48 ms
  (≈ 0.005 GB-s). An idle, hibernated endpoint costs only its SQLite storage
  (≈ 40 KiB).
- **CPU/active ratios of 50–63% have a bug behind them, not an inherent
  cost.** Under production's clock semantics a noq timer that `setTimeout`
  fires up to 1 ms *early* (n0-future truncates to whole ms), and the
  connection driver then spins on `setTimeout(0)` without the clock moving.
  Every session close burns 1.5–1.9 s of CPU this way, and quiet Active
  periods burn 1.8–3.6 s spikes. Reproduced locally and fixed by a one-line
  change (see "CPU diagnosis").
- **Memory:** the wasm linear memory is 1.6 MiB idle and grows about
  0.1 MiB per connection. Production isolate memory peaks at 27–38 MB while
  Active, which leaves about 90 MB of the 128 MB limit for application logic.
  That headroom is shared by every object co-resident in the isolate.

## Prices used

Source: <https://developers.cloudflare.com/durable-objects/platform/pricing/>, as
of 2026-10-21 (Workers Paid, $5/month minimum).

| Dimension | Price | Included / month |
|---|---|---|
| Duration | $12.50 / M GB-s, billed at a fixed 128 MB | 400,000 GB-s |
| Requests (HTTP, RPC, alarms; incoming WS messages at 20:1; outgoing WS free) | $0.15 / M | 1,000,000 |
| SQLite rows written | $1.00 / M | 50,000,000 |
| SQLite rows read | $0.001 / M | 25,000,000,000 |
| SQLite storage | $0.20 / GB-month | 5 GB |
| Idle, hibernation-eligible objects | not billed | |

Analytics report `duration` as 0.128 GB-s per active second (7.68 GB-s for a
fully Active minute in S3), so the figures below use **0.128 GB-s/s**. The
dollar figures are marginal list prices beyond the included amounts.

## Methodology

- **One fresh endpoint per scenario**, minted through `POST /mint` (same
  request as `just _mint`). Ids and mint responses are in
  `raw/endpoints.jsonl`. Analytics attribute everything per `objectId`, which
  is mapped to a scenario by the mint invocation's timestamp.
- **Scenarios** started on a minute boundary +3 s and ran one at a time. The
  peer was `target/release/iroh-do-demo`: stock iroh 1.4.0 with
  `--relay-url https://<E>.iroh-do.example.com/`, which skips the
  pkarr/DNS lookup. Logs with UTC start/end times are in `raw/S*.log`.
- **Analytics** came from the Cloudflare GraphQL API:
  - `durableObjectsPeriodicGroups`: per-minute sums of activeTime, cpuTime,
    duration, WS message counts, rows, memory max
    (`raw/periodic-and-minute-invocations.json`);
  - `durableObjectsInvocationsAdaptiveGroups` grouped by **`datetime`
    (1-second resolution)** and `type`. When a second holds one invocation,
    the cpu/wall quantiles are that invocation's exact values
    (`raw/invocations-per-second.json`);
  - `durableObjectsSqlStorageGroups` (`raw/sql-storage.json`).

  The Workers Observability telemetry API returned an authentication error
  for this token, so per-invocation logs were not used.
- **Mint is subtracted.** Each object's first invocation is the mint (1.0–1.9
  s wall, 31–66 ms CPU, 19 rows written, 23 read, 2 subrequests for the pkarr
  publish), and the scenario rows below exclude it. Alarms (1–4 per object,
  30–130 ms wall, 1–41 ms CPU) are included. They are the post-mint `sweep`
  plus the S6 callback; pkarr republish is every 24 h (`pkarr.rs:16`).
- **Caveats:**
  - Periodic `datetimeMinute` labels are sometimes shifted by one minute
    relative to invocation timestamps (S3, S7), so only per-object totals are
    used from that dataset.
  - `inboundWebsocketMsgCount` was **0 for every object**, while outbound
    counts are populated. This looks like a gap in the metric for
    hibernatable sockets. Each inbound message is delivered as one
    `hibernation`-type invocation (`webSocketMessage`, `webSocketClose`), so
    **inbound WS messages are counted as `hibernation` invocations**. This
    slightly over-counts, because closes are included.
  - Single runs: n = 1 per scenario, and n = 5 (+1) sessions for S7.
    Network RTT to the colo varied from 240 to 1,150 ms in the connect
    figures, and it changes how long a session stays Active.

## Per-scenario results (production, mint excluded)

| | Scenario | Active s | CPU s | CPU/active | GB-s | Inbound WS (hib. inv.) | Outbound WS | HTTP | Alarms | Rows W | Rows R | Max isolate mem |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| S1 | one `ping`, then 5 min idle | 2.14 | 0.25 | 0.12 | 0.27 | 11 | 14 | 1 | 1 | 8 | 24 | 7.9 MB |
| S2 | `listen` (hello, close, hold relay socket 3 min) | 4.09 | 2.53 | 0.62 | 0.52 | 20 (16 session + 4 Warm pings) | 25 | 1 | 1 | 12 | 75 | 36.9 MB |
| S3 | chat held 153 s, 5 lines at 30 s | 156.45 | 18.89 | 0.12 | 20.03 | 79 | 79 | 1 | 1 | 15 | 33 | 38.2 MB |
| S4 | chat, 200 lines at 0.3 s (68 s) | 73.79 | 13.80 | 0.19 | 9.44 | 457 | 444 | 1 | 1 | 406 | 20,520 | 34.8 MB |
| S5 | `bench --mb 16` (16 MiB echoed, 19.1 s, 0.84 MiB/s each way) | 25.19 | 12.70 | 0.50 | 3.22 | 12,449 | 16,693 | 1 | 1 | 8 | 23 | 34.8 MB |
| S6 | `listen --callback 20`, idle 100 s | 7.74 | 4.13 | 0.53 | 0.99 | 32 | 36 | 1 | 2 | 18 | 92 | 37.4 MB |
| S7 | 5 × `ping`, 30 s apart (+1 `/status` chat) = 6 cold sessions | 23.30 | 14.77 | 0.63 | 2.98 | 83 | 105 | 6 | 4 | 38 | 166 | 37.3 MB |

Notes:

- **S7:** `/status` afterwards reported `wakes=7, endpoint_builds=6` (mint +
  6 sessions), so **every session was a cold wake**: the object was evicted
  within 30 s of the socket closing.
- **S2, Warm:** the four relay-ping wakes (19:03:18/34/49, 19:04:03) cost
  35, 0.7, 34 and 48 ms wall and 5.6, 0.4, 2.8 and 11 ms CPU. The client
  dropped its relay socket about 60 s after its last send, as V5 predicts.
- **S3:** the connection was Active for 100% of wall time, at 0.128
  GB-s/s.
  - Each 5 s keep-alive exchange is about 2 invocations at around 1 ms CPU,
    and each invocation's wall time is ≈ 5 s, because the object is never
    idle between them.
  - 74% of the CPU (≈ 14 s) sits in 8 invocations of 0.5–3.6 s CPU each.
    See "CPU diagnosis".
- **S6:** the dial-back alarm (19:14:23) built the endpoint and dialed. The
  alarm invocation used 41 ms CPU and 107 ms wall. The following 9 events
  used 3.96 s wall, including a 2.05 s CPU spin at close.

Approximate list-price cost of each scenario (duration + requests + rows written):

| S1 | S2 | S3 | S4 | S5 | S6 | S7 (per session) |
|---|---|---|---|---|---|---|
| $11.8 µ | $19.0 µ | $266 µ | $528 µ | $142 µ | $31 µ | $12.9 µ |

S1–S3 and S7 are dominated by duration and by rows written. Rows are only
billed beyond 50 M/month. S4 is dominated by the 2 rows written per message.
S5 is dominated by requests: 12,449 inbound messages ÷ 20.

## Derived parameters

| Parameter | Value | Range / uncertainty | Source |
|---|---|---|---|
| **Active second** (connection open, keep-alives only) | **0.128 GB-s** ($1.60 µ) | exact (billing definition) | S3 |
| Active-second inbound WS msgs | 0.52 /s (≈ 0.026 billable req/s) | ±20% (keep-alive cadence 5 s each way) | S3 |
| Active-second CPU, excluding spin | ≈ 0.2 ms/s (~1 ms per 5 s keep-alive event) | median of 60+ single-event seconds | S3 |
| Active-second CPU, as measured today | 0.12 s/s | 0.12–0.63 by scenario | S3, S7 |
| **Cold session** (wake + bind + QUIC/TLS handshake + 1 RPC + close + `T_linger` 2 s) | **2.1–3.9 s Active = 0.27–0.50 GB-s** ($3.4–6.2 µ) | S1 = 2.14 s (no spin), S7 mean = 3.88 s (spins at close) | S1, S7 |
| Cold session requests | 1 HTTP upgrade + ~14 inbound WS (0.7 billable) ≈ **1.7 requests** | 11–16 WS msgs | S1, S7 |
| Cold session rows | ~6–8 written (counters: wakes, builds; schedule), ~25 read | | S1, S7 |
| Wake itself (constructor, attachment restore; HTTP upgrade invocation) | 2–14 ms CPU | 5 samples | S7 |
| Activation (endpoint bind + handshake), no spin | ≈ 0.1 s CPU (largest S1 event 103 ms) | 0.1 s (S1) to 0.3–0.87 s (S7 largest connect event, likely including spin) | S1, S7 |
| CPU burned by close-spin, per session | **1.47–1.94 s** (mean 1.73 s) in one invocation of 2.1–2.8 s wall | 5/5 S7 sessions; 0/1 in S1 | S7 |
| **Warm ping wake** | 35–48 ms wall ≈ **0.005 GB-s** ($0.06 µ), 0.4–11 ms CPU, 1 inbound WS msg (0.05 req) | 4 samples | S2 |
| Warm period per peer (≤ 60 s after its last send, V5) | ~4 ping wakes ≈ 0.02 GB-s + 0.2 req | | S2 |
| **Chat message** (open connection) | ~2.2 inbound + 2.2 outbound WS msgs (0.11 billable req); **2.0 rows written** (INSERT into `log` + `log_peer` index) | ±0.1 | S4 |
| Chat message rows read | **= current log size** (full scan, `store.rs:219` `sum(peer = ?) FROM log`), so up to 10,000 at retention | measured mean 102 for log sizes 1–200 | S4 |
| Chat message CPU | ≈ 3 ms without spin (median event 1.3 ms × 2.2 events); 69 ms/message measured overall | lower bound from per-second P50s | S4 |
| Chat message duration | none beyond keeping the session Active (0.128 GB-s/s while open) | | S4 |
| **WS messages per MiB** (echo, 16 MiB each way) | **≈ 780 inbound** (39 billable req) and ≈ 1,040 outbound per MiB echoed | 1 WS message per QUIC packet (no batching): ~1.35 KB per data datagram, plus ACKs | S5 |
| Duration per MiB echoed | 1.57 s Active = **0.20 GB-s/MiB** ($2.5 µ) at 0.84 MiB/s | throughput is RTT-bound here; local workerd reaches 6–9 MiB/s | S5 |
| Requests cost per MiB echoed | 39 req = $5.9 µ, **more than duration** | | S5 |
| CPU per MiB echoed | 0.79 s (production, includes spin); local workerd 0.15–0.22 s | | S5, local |
| Dial-back (alarm → bind → dial → notify → close) | alarm 41 ms CPU / 107 ms wall + ≈ 4 s Active (0.5 GB-s) | 1 sample | S6 |
| Mint (one-time) | 1.0–1.9 s Active (0.13–0.24 GB-s), 19 rows written, 2 subrequests | 7 samples | all |
| SQLite stored bytes per log row | ≈ 140 B payload (84 B row + 56 B `log_peer` index entry), ≈ 200 B on disk at typical page fill | local SQLite `dbstat`, 70 rows | local |
| SQLite fixed size per endpoint | ≈ 40 KiB (10 tables/indexes at 4 KiB pages); a full 10k-row log adds ≈ 2 MB | namespace sample: 45,056 B | local, prod |
| Billable requests formula | HTTP upgrades + alarms + inbound WS msgs / 20 | | |

### Worked cost examples (marginal list price)

- **A peer that connects, does one RPC and leaves:**
  - ≈ 0.27–0.50 GB-s + 1.7 requests + ~7 rows ≈ $3.7–6.5 µ of duration and
    requests, plus $7 µ of rows if above 50 M rows/month.
  - **≈ 150k–270k sessions per $1** of duration.
- **A peer holding a connection open (keep-alives, occasional traffic):**
  - $5.76 per 1,000 connection-hours.
  - One endpoint held Active 24/7 is 337k GB-s/month ($4.21), which is
    84% of the Paid plan's included 400k GB-s.
  - `t_silent` (60 s) and `t_idle` (10 s) are therefore the main cost
    controls (note 005).
- **Bulk data:**
  - ≈ $8.4 µ per MiB echoed: $2.5 µ duration + $5.9 µ requests.
  - That is ≈ $8.6 per GiB each way at today's 0.84 MiB/s.
  - Higher throughput lowers the duration part. The request part is fixed
    by the 1-packet-per-WS-message framing, so relay datagram batching (§5.11)
    is the lever: k datagrams per WS message divides the request cost by k.
- **Idle endpoint (no peers):** storage only, ≈ 40 KiB, so effectively $0.
  Republish alarms run once per 24 h, at about 0.01 GB-s each.

### iroh-do overhead vs. demo-app work

- **App-only costs.** The demo's handlers add two things beyond iroh-do
  itself:
  - SQLite rows: 2 written per chat/echo message, and a full-table read per
    chat message, which is a demo inefficiency. Keep `yours` in a counter
    row, or use `count(*) WHERE peer = ?` on the `log_peer` index, to make
    it O(1)/O(yours).
  - Under 1 ms of CPU per message.
- **iroh-do costs.** Everything else in the per-message cost is iroh-do and
  the QUIC transport. That covers the WS message counts (≈ 2.2 in / 2.2 out
  per request/response, including ACKs), the per-packet crypto and framing,
  and the Active duration.
- **Per session**, iroh-do's own rows (wake/build counters and schedule
  upserts, ≈ 6/session) cost as much as the duration at list price ($6 µ
  vs $3–6 µ). Batching or dropping those counters is worth doing if rows
  ever exceed the included 50 M/month.
- **Local cross-check** with the real clock (`raw/clock-real-fix0.json`):
  20 chat lines plus session setup and teardown used 0.09–0.13 s of workerd
  process CPU in total. That puts iroh-do + app at ≤ 5 ms CPU per message
  once the spin is gone.

## CPU diagnosis

**Finding: hypothesis (b) is confirmed and the mechanism is located.**
Hypotheses (a) and (c) are minor.

### Evidence from production

- **CPU scales with timer expiries, not with wakes or Active seconds.**
  - S3 (153 s Active, one wake) spent 74% of its CPU in 8 single
    invocations of 0.5–3.6 s CPU. Its ≈ 60 keep-alive invocations cost
    ≈ 1 ms each.
  - In S7, each of the 5 cold sessions ends in one invocation of 1.47–1.94 s
    CPU and 2.1–2.8 s wall. That is the client's close followed by
    noq's draining timer (3 × PTO, an RTT-derived, fractional deadline)
    during `T_linger`. The cold-wake invocation (HTTP upgrade, constructor)
    costs only 2–14 ms CPU.
  - Hypothesis (a), wasm instantiation per wake, is therefore not the
    driver.
  - Crypto, hypothesis (c), is bounded by S1, a cold session with no spin
    whose activation and handshake took ≈ 0.1 s CPU, and by S5's bulk
    transfer, where CPU per MiB is 4× local workerd.
- The spikes run at ≈ 65–70% CPU over their wall time. That is a
  macrotask-bound loop yielding to the event loop on every turn.

### Mechanism

1. **workerd's clock in a timer callback is the timer's scheduled time.**
   `setTimeoutImpl` computes `when = context.now() + msDelay` ("so the delay
   appears exact to the application"). While a fired timer's callback runs,
   the visible clock is clamped to the earliest fired deadline
   (`TimeoutManagerImpl::getClockClamp`, `src/workerd/io/io-context.c++`).
   Time catches up with real time only when the next I/O is delivered
   (`IncomingRequest::delivered` → `syncTime()`). So **`setTimeout(0)` never
   advances `Date.now()` / `performance.now()`.** Locally the clock is real
   time (V12), which is why this never showed up in tests.
2. **n0-future truncates.** `n0-future-0.3.2/src/time.rs:76` and `:147`
   (`sleep_impl`, `Sleep::reset`) pass `duration.as_millis() as i32` to
   `setTimeout`. A timer for a deadline 4.7 ms away fires at `now + 4 ms`,
   which is 0.7 ms *before* its deadline by the clock it then sees.
3. **noq re-arms with a zero delay and spins.**
   - `noq-1.4.0/src/connection.rs:1845-1886` (`drive_timer`) sees
     `now < deadline`. The fired flag is still set, so `delay.poll()` is
     `Ready`. It calls `handle_timeout(now)` (a no-op), clears
     `timer_deadline` and returns `true`, and the driver wakes itself
     (`:280-282`).
   - On the next poll it `reset`s the timer to the same deadline, which is
     `setTimeout(0)`.
   - That callback sees the same clamped clock, and the loop repeats until
     some real I/O arrives or a later-scheduled timer fires and moves the
     clock past the deadline.
   - In S7 the loop ends when `T_linger`'s 2,000 ms timer fires. In S3 it
     ends on the next keep-alive packet, so it can last up to ~5 s.
   - Deadlines are fractional whenever they derive from RTT (PTO, loss
     detection, draining, pacing). Whole-second timers such as `T_linger`,
     `t_idle` and the 5 s keep-alive are integral against an integer-ms
     clock and do not spin.

### Local reproduction (`docs/measurements/clock-emulation.mjs`)

The script runs `build/worker/` in Miniflare from an in-memory copy (nothing
on disk is modified). With `CLOCK=frozen`, the copy emulates the production
semantics above:

- integer-ms `Date.now`/`performance.now`, frozen per event;
- re-synced to real time at every DO/router entry;
- set to `max(clock, when)` in timer callbacks.

It counts zero-delay `setTimeout` calls and workerd process CPU over a fixed
peer workload:

| Workload (wall) | real clock: CPU s / `setTimeout(0)` | frozen clock: CPU s / `setTimeout(0)` | frozen + fix: CPU s / `setTimeout(0)` fired |
|---|---|---|---|
| 3 pings + 15 s idle each (47 s) | 0.15 / 9 | 0.61–0.62 / 24,896–56,065 | 0.33 / 0 † |
| chat 20 lines @ 0.3 s + 15 s (22 s) | 0.09 / 3 | 1.49–1.76 / 153k–184k | 0.10–0.14 / 0 |
| quiet chat held 40 s + 15 s (56 s) | 0.08 / 6 | **9.3–15.0 / 505k–993k** | 0.05–0.07 / 0 |
| bench 4 MiB + 15 s (16 s) | 0.88 / 3 | 1.11–1.43 / 4.4k–33k | 0.39–0.62 / 0 |

Ranges cover two runs, on bundles `1927f996…` and `f13b10b7…`; the other
engineer rebuilt `build/worker` between runs. `raw/clock-frozen-fix1.json` holds the
`f13b10b7…` run. † Its first phase read −0.66 s, because a workerd process exited
mid-phase, so the 0.33 s comes from the `1927f996…` run (stderr only, not kept as JSON). Stack sampling
(`STACKS=1`, `raw/clock-frozen-fix0-stacks.json`) attributes **1,376 of 1,377**
sampled zero-delay timers to

```
n0_future::time::wasm::set_timeout <- n0_future::time::wasm::Sleep::reset
  <- <iroh::runtime::web::Timer as noq::runtime::AsyncTimer>::reset
  <- (noq ConnectionDriver poll) <- js_sys::futures::queue
```

The remaining sample was a one-off `interval` in `Builder::bind`.

### Fix (not applied: crates are being edited by someone else)

**Root cause, upstream in n0-future** (`src/time.rs`, both `sleep_impl` and
`Sleep::reset`). Round the delay **up**, so a timer never fires before its
deadline by the clock its callback sees. This also removes the documented
i32 wrap hazard:

```rust
// was: duration.as_millis() as i32
let ms = duration.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32;
```

Optional hardening in noq `drive_timer`, for any runtime whose timers can fire
early: when the timer fired but `now < deadline`, re-arm with a minimum 1 ms
delay instead of re-polling immediately.

**Workaround in this repo, no dependency patch.** It is P-compatible: stock
iroh/noq/n0-future, glue only. Install a `setTimeout` wrapper once per
isolate that adds 1 ms to every delay. n0-future calls
`globalThis.setTimeout(cb, ms)` by property lookup (`index.js`:
`e.setTimeout(t,n)`), so the wrapper intercepts every iroh/noq timer.

In `crates/iroh-do/src/glue/endpoint.rs`, call it first thing in the
`IrohEndpoint` constructor (and in the router `fetch` for completeness):

```rust
/// workerd runs a timer callback at exactly `now + floor(ms)` and does not advance the clock
/// for setTimeout(0); n0-future truncates sub-ms remainders, so noq's driver would spin on
/// setTimeout(0) until the next I/O (docs/measurements/cost-profile.md). Add 1 ms to every delay.
fn install_timer_round_up() {
    thread_local!(static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    if DONE.with(|d| d.replace(true)) { return; }
    let g = js_sys::global();
    let key = wasm_bindgen::JsValue::from_str("setTimeout");
    let Ok(orig) = js_sys::Reflect::get(&g, &key).and_then(|f| f.dyn_into::<js_sys::Function>()) else { return };
    let this = g.clone();
    let wrapper = wasm_bindgen::closure::Closure::<dyn Fn(wasm_bindgen::JsValue, f64) -> wasm_bindgen::JsValue>::new(
        move |cb, ms: f64| {
            let ms = if ms.is_finite() && ms > 0.0 { ms.floor() } else { 0.0 } + 1.0;
            orig.call2(&this, &cb, &ms.into()).unwrap_or(wasm_bindgen::JsValue::UNDEFINED)
        });
    let _ = js_sys::Reflect::set(&g, &key, wrapper.as_ref());
    wrapper.forget();
}
```

Every iroh timer then fires ≤ 1 ms late, which is harmless for QUIC: noq
checks the clock, not the timer. The emulation's `FIX=1` is exactly this +1 ms
wrapper, and it brings zero-delay timer firings to 0 and CPU back to the
real-clock baseline (table above).

The same pattern exists in `crates/iroh-do-core/src/protocols.rs:118-129`, the
idle loop's `sleep(d)` with `d = limit − elapsed`. It is safe today only
because `t_idle`/`t_silent` are whole seconds against an integer-ms clock.
The wrapper or the n0-future fix covers it too.

### Cost impact of the fix

- **Duration (the billed dimension):** little change in most cases. The
  spins run inside windows that are billed anyway: `T_linger`, draining, and
  gaps between keep-alives with timers pending. S7 sessions (3.9 s with
  spin) vs S1 (2.1 s, no spin) bound the saving at **≤ ~1.7 s ≈ 0.2 GB-s per
  session**.
- **Gains are mostly outside the bill:**
  - No multi-second CPU invocations. The DO CPU limit is per invocation, and
    the largest seen was 3.6 s.
  - No starvation of co-resident objects in the same isolate.
  - Lower and more stable latency: echo RTTs of 200–525 ms during S3 vs
    72–90 ms in S4.
  - Once fixed, the CPU/active ratio should drop to the local figures,
    ≈ 1–5%.

## Memory budget

Production `memoryUsageBytes` is the per-minute max of isolate memory: JS heap,
wasm linear memory, compiled code. From `raw/periodic-and-minute-invocations.json`:

| State | Isolate memory |
|---|---|
| Mint only / Warm after teardown (no Endpoint) | 7.3–8.6 MB |
| Active, 1 connection (S1–S4, S6, S7) | 27–38 MB |
| Active, 16 MiB bench (S5) | 34.8 MB |

Wasm linear memory (`exports.memory.buffer.byteLength`) was measured locally
with `docs/measurements/memory-probe.mjs`. That script loads an in-memory
copy of `build/worker/` with a `/__mem` route, and the router and DO share
the instance. Raw data is in `raw/memory-peers.json` and
`raw/memory-bench.json`. Results:

| Point | Wasm linear memory |
|---|---|
| Isolate up, module instantiated | 1.63 MiB |
| (a) DO constructed / cold wake, no Endpoint | 1.63 MiB |
| (b) Active, 1 chat connection | 1.75 MiB |
| (c) Active, 10 connections | 2.56 MiB |
| (c) Active, 50 connections | 6.50 MiB (6.88 after one more message each) |
| (d) all peers gone, Endpoint torn down (+14 s) | 6.88 MiB (high-water mark; wasm memory never shrinks) |
| (d') second Active period, 10 connections, same isolate | 6.88 MiB (freed space is reused, no growth) |
| (e) after `bench --mb 16`, 1 connection | 4.81 MiB (+3.2 MiB: stream/flow-control buffers) |

- **Per-connection increment:** ≈ **90–100 KiB** of wasm memory per open
  connection with one stream: (2.56 − 1.75)/9 and (6.50 − 2.56)/40. That
  rises to ≈ 105 KiB after traffic. A bulk stream adds up to ~3 MiB of
  buffers while it runs.
- **What dominates the isolate.** Most of the 27–38 MB is not wasm linear
  memory. It is the V8 heap plus compiled code for the 3.5 MB module, which
  is present once the endpoint runs. The 8 MB → 30+ MB jump between
  "no Endpoint" and "Active" is mostly code tiering and JS-side objects,
  not per-connection state.
- **Headroom.** Against the 128 MB isolate limit, ≈ **90 MB** remains with
  one active connection. Each further connection costs ~0.1 MiB, so
  hundreds of concurrent connections per endpoint fit before memory
  matters. Two caveats:
  1. The headroom is per isolate, not per object. Co-resident DOs of this
     class share one wasm instance and one 128 MB budget.
  2. Wasm memory never shrinks, so an app's peak allocation stays reserved
     until the isolate is recycled.
- **JS heap alone** is not observable in workerd (no `process.memoryUsage`),
  so it is reported only through the production isolate figure.
- **Billing is unaffected by memory.** It is fixed at 128 MB, so memory use
  does not change duration cost.

## Reproducing

```sh
# production scenarios: mint per scenario (POST /mint with .demo/admin_token via curl -K -), then e.g.
scripts/macos-sign-run.sh target/release/iroh-do-demo --relay-url https://<E>.iroh-do.example.com/ ping <E>
# local memory and CPU diagnosis (Miniflare from harness/workerd/node_modules)
node docs/measurements/memory-probe.mjs peers   > docs/measurements/raw/memory-peers.json
node docs/measurements/memory-probe.mjs bench   > docs/measurements/raw/memory-bench.json
CLOCK=real   node docs/measurements/clock-emulation.mjs
CLOCK=frozen node docs/measurements/clock-emulation.mjs
CLOCK=frozen FIX=1 node docs/measurements/clock-emulation.mjs
```

Miniflare echoes worker `console.log` lines to stdout, so the JSON result is
the last object in the output; the files in `raw/` were trimmed to it.
