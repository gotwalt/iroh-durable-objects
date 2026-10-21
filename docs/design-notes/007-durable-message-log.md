# 007: Durable message log in the demo DO

**Status:** accepted (2026-10-21). Extends note 006 (`iroh-do/chat/1`,
`iroh-do/notify/1`, `iroh-do/echo/1`).

## Why

The example should show what makes a DO endpoint different from an iroh node
on a server: a single, globally unique, strongly consistent SQLite database
addressed by the endpoint's key, which survives hibernation, eviction and
redeploys. Every call a peer makes lands in that database; peers see the
running totals, and `lifecycle` shows them persist across hibernation.

## Storage

Table `log` in the DO's SQLite (sync `sql.exec` only, one INSERT per logged
event, never per packet):

| column | type | notes |
|---|---|---|
| `seq` | INTEGER PRIMARY KEY | monotonic |
| `ts_ms` | INTEGER | `Date.now()` at insert |
| `peer` | TEXT | peer EndpointId, z32 |
| `kind` | TEXT | `chat` \| `echo` \| `callback` |
| `text` | TEXT | chat line (≤ 1024 bytes, truncated); for `echo`, `"<n> bytes"`; for `callback`, `"delivered"` or `"failed: <reason>"` |

Index on `peer`. Retention: keep the newest 10 000 rows (delete older in the
same transaction as the insert). `total` counts all rows ever inserted
(`max(seq)`), so it stays monotonic despite retention.

## Protocol additions

`iroh-do/chat/1` (still one reply line per request line):

- `<text>` → `echo: <text> (#<seq>, total <T>, yours <Y>)`
  where `seq` is this message's row, `T` total records, `Y` records from this peer.
- `/count` → `{"total":T,"yours":Y,"peers":P}` (P = distinct peers in retained rows).
- `/log [n]` → JSON array of the newest `n` rows (default 5, max 50), oldest
  first: `[{"seq":1,"ts_ms":…,"peer":"…","kind":"chat","text":"…"}]`.
- `/status` gains `"records":T`.
- `/status`, `/count`, `/log` and `/callback` are not themselves logged.

`iroh-do/echo/1`: each stream is logged as `kind=echo`.

`iroh-do/notify/1`: the dial-back line becomes
`callback from <E> wakes=<N> records=<T>`; the attempt is logged as
`kind=callback` after it completes or fails.

## Host interface

`iroh-do-core` stays storage-agnostic: the `Host` trait gains synchronous
methods (`log_event(peer, kind, text) -> LogStats { seq, total, yours }`,
`count(peer) -> Counts`, `recent(n) -> Vec<LogRow>`). The DO implements them
over SQLite; `harness/native` implements them in memory.

## Evidence in `lifecycle`

`records` strictly increases across sessions and across hibernation
(`wakes` increased in between), and `/log` after hibernation contains the
lines sent before it.
