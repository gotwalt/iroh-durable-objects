# 010: Structured logs, and the script and class names

**Status:** accepted (2026-10-21).

## Decision

The Worker installs a `tracing` subscriber that writes every event as one JSON
object to the console, so Workers Logs indexes its fields. The hand-written
JSON `console_log!` lines are gone; every log line, ours and iroh's, goes
through `tracing`. Workers traces are enabled in `infra/main.tf`.

The script is renamed `iroh-do-chat` and the classes `room` and `registry`, so
the namespaces are `iroh-do-chat_room` and `iroh-do-chat_registry`. Hostnames
do not change.

## Format

`crates/iroh-do/src/log.rs` is a `tracing_subscriber` layer over the
`Registry`. For each event it writes an object with:

- `level`: `error`, `warn`, `info`, `debug` or `trace`. Errors go to
  `console.error`, warnings to `console.warn`, the rest to `console.log`.
- `target`: the module path, e.g. `iroh_do::glue::endpoint`,
  `iroh_do_core::room`, `noq_proto::connection`.
- `message`: the event's message, or its `event` field if it has none.
- `spans`: enclosing span names, outermost first, joined with `:`.
- the fields of every enclosing span, then the event's fields, flattened. An
  inner field replaces an outer one of the same name.

The layer serializes with `serde_json`; the Worker parses the line back with
`JSON.parse` and logs the object, because Workers Logs indexes the fields of a
logged object and not those of a string. There is no timestamp field: Workers
Logs records one per line.

Every node task runs inside a `node` span with field `endpoint`, so iroh's
own events from those tasks carry the endpoint too. The object's glue sets
`endpoint` on each of its events explicitly.

`LOG_LEVEL` is a plain-text binding (`infra/` variable `log_level`, wrangler
var for local runs). It takes `level` and `target=level` directives separated
by commas, the subset of `EnvFilter` syntax without span and field filters,
parsed by `tracing_subscriber::filter::Targets`. A directive matches every
target it is a prefix of, and the longest match wins. The default is
`info,iroh=warn,noq=warn,netwatch=error,iroh_do=info`: `iroh=warn` also covers
`iroh_relay` and our `iroh_relay_ingress` and `iroh_ingress_transport`, and
`iroh_do=info` takes `iroh_do` and `iroh_do_core` back to `info`. `netwatch`
warns on every endpoint bind because workerd has no `navigator.onLine`. An
invalid value falls back to the default and logs `log_level_invalid`. The
subscriber is installed once per isolate, by the router's `fetch` or by the
first object constructed, whichever runs first; a changed `LOG_LEVEL` takes
effect in new isolates, which a deploy creates.

`EnvFilter` was not used: it depends on `regex-automata`. With `Targets` the
subscriber and the events below add 89,219 B to `index_bg.wasm` (34,145 B
gzip -9), 3,623,366 B to 3,712,585 B, measured with `just build-worker`.

## Events

Each has a stable `event` field. Endpoints and peers are z-base-32
EndpointIds; durations are milliseconds. Inside an object, `Date.now()` and
`performance.now()` advance only across I/O, so a duration measures waiting on
the network or storage, not CPU.

| `event` | Level | Fields | When |
|---|---|---|---|
| `wake` | info | `endpoint`, `minted`, `wakes`, `sockets`, `sockets_closed`, `refused` | Object constructed: first use, after hibernation, eviction or a panic. `sockets` were restored from attachments; `sockets_closed` were closed instead (lost, expired or superseded). |
| `state` | info | `endpoint`, `from`, `to`, `prev_ms`, `sockets` | Cold, Warm, Active transition. `prev_ms` is the time spent in `from` within this wake. |
| `endpoint_bind` | info, error | `endpoint`, `ok`, `ms`, `builds`, `queued`, `dials`, `error` | `Endpoint` bound (or failed). `queued` frames and pending `dials` were waiting for it. |
| `endpoint_close` | info | `endpoint`, `ms`, `active_ms`, `leftover_tasks` | Teardown after `t_linger`. `ms` covers the close and the wait for node tasks, `active_ms` the time since bind. |
| `ingress_auth` | info, warn | `endpoint`, `ok`, `peer`, `superseded`, `code`, `reason` | Relay handshake succeeded, or the socket was closed before authenticating (bad signature, handshake timeout, protocol error). |
| `dst_violation` | warn | `endpoint`, `peer`, `count` | A peer sent a datagram addressed to an endpoint other than `E`. |
| `socket_close` | info, warn | `endpoint`, `peer`, `by`, `code`, `reason`, `clean`, `error` | An authenticated relay socket closed. `by` is `peer`, `object` or `error`. `peer` is empty if the socket never authenticated. |
| `conn_open` | info | `endpoint`, `peer`, `alpn`, `dir` | QUIC connection accepted (`dir` `in`) or dialed (`out`). |
| `conn_close` | info | `endpoint`, `peer`, `alpn`, `dir`, `reason`, `ms` | Connection closed: `idle`, `silent`, `done` or the peer's close reason. |
| `conn_refused` | warn | `endpoint`, `peer`, `alpn`, `dir` | Unsupported ALPN. |
| `room_request` | info | `endpoint`, `peer`, `kind`, `nick`, `ok` | One room request. `kind` is `join`, `say`, `me`, `nick`, `who`, `history`, `since`, `quit` or `unknown`. |
| `room_event` | info | `endpoint`, `peer`, `kind`, `seq`, `recipients` | A room event was stored and queued for `recipients` present members. |
| `room_delivery` | info, warn | `endpoint`, `peer`, `events`, `via`, `ok`, `ms` | One delivery stream: `events` in the batch, `via` `open` (reused connection), `dial` or `none`. |
| `dial_back` | info, warn | `endpoint`, `peer`, `ok`, `ms`, `error` | A scheduled `/callback`. |
| `alarm` | info | `endpoint`, `due`, `ran`, `failed`, `ms` | One alarm run. `due` lists the kinds (`republish`, `sweep`, `callback`), never a peer. |
| `pkarr_publish` | info, warn | `endpoint`, `ok`, `ms`, `status`, `failures`, `next_in_s` | Publish and confirm. |
| `mint` | info | `endpoint`, `published`, `label`, `ms` | Identity stored by the object. |
| `registry_register` | info, warn | `endpoint`, `from`, `ok`, `error` | The object recorded itself in the registry (`from` `mint` or `backfill`). |
| `registry_entry` | info | `endpoint` | The registry stored an entry. |
| `admin_auth` | info, warn | `ok`, `path` | A `POST /mint` bearer token was accepted or refused. |

Errors carry `event` too: `config_error`, `identity_load_failed`,
`identity_invariant_violated`, `mint_name_mismatch`, `schema_failed`,
`schedule_failed`, `alarm_entry_failed`, `set_alarm_failed`, `log_sql_failed`,
`upgrade_failed`, `attachment_write_failed`, `rng_failure`,
`timer_round_up_failed`.

## Secrets

No event takes the endpoint's secret key, the mint request's `secret_hex`,
the admin token or the `Authorization` header, and room events record the
request kind and nick but never message text. Two tests in `log.rs` hold this:
`no_log_macro_names_a_secret` fails if any log macro in `iroh-do` or
`iroh-do-core` passes a value whose expression mentions `secret`, `token`,
`authorization` or `expected` (the router's name for the admin token), and
`secret_key_is_redacted` checks that a `SecretKey` logged by mistake prints as
`SecretKey(..)`.

## Querying

In the dashboard, Workers & Pages → `iroh-do-chat` → Observability, the query
builder filters and groups on any of these fields. For example:

- one endpoint's history: `endpoint = <E>`;
- wakes per hour: `event = wake`, count, grouped by time;
- how long objects stay Active: `event = state` and `from = Active`, p50 and
  p99 of `prev_ms`;
- failed relay handshakes: `event = ingress_auth` and `ok = false`, grouped
  by `reason`;
- delivery latency: `event = room_delivery`, p99 of `ms`, grouped by `via`;
- iroh's own warnings: `level = warn` and `target` starting with `iroh`.

`wrangler tail iroh-do-chat --format json` streams the same objects.

## Names

Cloudflare names a namespace `<script>_<class>`, and a class name cannot
contain a hyphen. workers-rs 0.8.7's `#[durable_object]` re-emits the struct
under `#[wasm_bindgen]` and generates the `impl` block without `js_class`, so
the JS class takes the Rust type's name; a `js_name` on the struct would not
reach the generated `impl` block. worker-build re-exports
every class under the name wasm-bindgen gives it. The Rust types are therefore
lowercase, `room` and `registry`, with `#[allow(non_camel_case_types)]`. The
bindings are `ROOMS` and `REGISTRY`.

There is no `renamed_classes` migration: the single migration `v1` declares
both classes, and applying the rename to an existing deployment deletes the
old script's namespaces and every endpoint key in them.
