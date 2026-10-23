# 015: Hardening the Rust host

**Status:** accepted (2026-10-22). Carries out what 014 lists under "What
carries over to the Rust host". Amends 011 (the app API: `App::on_alarm`,
`Ctx::transaction`) and 006 (the socket table moves into
`iroh-relay-ingress`). No principle in `spec.md` §1 changes; the socket table
moves code toward P5.

## Alarms

Before, the alarm handler deleted a schedule row and then ran its work, and
`App::on_alarm` was synchronous, so the chat app spawned its dial-back as a
task. Work that failed, or that a trap or an eviction cut short, was lost.

Now each due row is leased before its work runs: moved to its retry time,
with an attempt count, and the alarm is armed for the result. The row is
deleted only after the work succeeds, and only if the work did not schedule
the key again. Retries wait 30 s after the first failure, doubling up to 1 h.
The schedule table gains a column, `attempt`. The alarm is deleted when the
table is empty, and `Ctx::cancel` re-arms.

`App::on_alarm(&self, ctx, alarm) -> impl Future<Output = Result<(),
AlarmError>> + Send` replaces the synchronous `on_alarm(ctx, key, payload)`.
`Alarm` carries the key, the payload and the attempt count. The node keeps
the endpoint bound while the future runs. Apps await their work inside it
instead of spawning it.

The app's work runs in tasks that start as the alarm handler returns, not
inside the handler. A panic traps the wasm instance. workers-rs then leaves
the promise of the event it was handling unsettled, and workerd delivers no
further alarm to the object while an alarm event is unsettled. On Miniflare,
work that panicked inside the handler was never retried; started after the
handler returned, it was retried 60 s later and succeeded
(`tests/workerd/fault-test.mjs`). The library's own work (republish, sweep)
does not panic by design and runs inside the handler, so its failures are
counted in the `alarm` event.

## Transactions

workers-rs 0.8.7 does not bind `ctx.storage.transactionSync`. The object
binds it with a `#[wasm_bindgen] extern "C"` block
(`crates/iroh-do/src/glue/tx.rs`) and passes it a borrowed closure
(`ScopedClosure::borrow_mut_aborting`). A closure that returns an error
throws, which rolls the transaction back; a panic traps, and the trap is
thrown through `transactionSync` and rolls it back too.

- **Mint** stores the identity, creates the tables, counts the wake and
  builds the node and the app in one transaction. If any step fails, nothing
  is stored, the object logs `mint_failed`, and the mint answers 503. Work the
  app schedules in its constructor is part of the transaction.
- **`Host::transaction` and `Ctx::transaction`** give apps the same: the chat
  room writes an event and the member's new state together. The native
  harness implements it with a rusqlite transaction.

## The socket table

The table of an object's relay sockets (session per socket, socket per peer,
superseding, restore from attachments, the handshake sweep) was written twice,
in the Durable Object glue and in the native harness, and the two differed:
the harness did not close a superseded socket. It is now
`iroh_relay_ingress::Sockets<K>`, sans-io like the session it holds. Each
call takes the current time and returns `Effect`s for the host to carry out
in order. Both hosts use it, and `crates/iroh-relay-ingress/tests/table.rs`
tests it alone.

Moving it fixed a defect both copies shared: a session that closed itself (a
protocol error, a text message, too many destination violations) no longer
reports its peer, so the object neither unbound the peer's socket nor told
the node. The table finds the peer through its routes instead.

## Smaller changes

- pkarr requests are aborted after 10 s, and their bodies are read to the end.
- The router answers 503 when a binding, an object or the registry fails, and
  logs `router_error`. The admin token is compared by SHA-256 digest, so its
  length does not leak through timing.
- `bound_at_ms` in `counters` is set while an `Endpoint` is bound. A
  constructor that finds it logs `evicted_active`.
- The socket table logs `ingress_auth`, `socket_close` and `dst_violation`
  under its own target, which the default `LOG_LEVEL` filtered out through
  `iroh=warn`. The default (`crates/iroh-do/src/log.rs`, the deployment's
  `wrangler.toml`, `deploy/tofu/variables.tf`) gains
  `iroh_relay_ingress=info`; the next plan of a deployment shows the changed
  binding.
- The chat app bounds a member's outbox at 500 lines, replies `storage error`
  only when nothing was stored and resets the stream when it cannot tell,
  reads room history through a partial index, arms the prune alarm once per
  quiet period, and forgets rate-limit windows that have emptied.

## Not applicable to the Rust host

From the review of the TypeScript host (014): the status a router answers
without the admin token already holds only the node's and the app's fields,
which any peer can read with `/status` on `iroh-do/ctl/1`; the router does
not look endpoints up in the registry, so unregistered ids do not wake it;
stream guards are values owned by the handler, not by a garbage collector.
