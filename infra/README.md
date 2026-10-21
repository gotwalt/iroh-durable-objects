# infra/: OpenTofu module for the example deployment

This module deploys the iroh-do Worker to a Cloudflare zone. One Worker script,
`iroh-do-chat`, contains the router and two Durable Object classes: `room`, one
object per endpoint, and `registry`, the endpoint list. Cloudflare names their
namespaces `iroh-do-chat_room` and `iroh-do-chat_registry`. Each endpoint
`E` is served at `https://<E>.iroh-do.<zone>/`. The design is in `docs/spec.md`
§13 and design notes 001 and 002.

Endpoint secret keys are generated inside the Worker and stored only in Durable
Object storage. They never pass through OpenTofu, your shell or the state file,
which also means Cloudflare holds every endpoint's key (spec P8). `tofu destroy`
deletes all of them, and they cannot be recovered (see [Teardown](#teardown)).

## What it creates

- `cloudflare_workers_script.iroh_do`: the bundle (`index.js` as the main
  module, `index_bg.wasm` imported by it), the bindings `ROOMS` (the `room`
  namespace), `REGISTRY` (the `registry` namespace), `ADMIN_TOKEN` (secret),
  `ZONE_SUFFIX`, `BUNDLE_SHA256`, `PKARR_RELAY` and `LOG_LEVEL`, the Durable
  Object migration `v1` (`new_sqlite_classes = ["room", "registry"]`), and
  Workers Logs and traces (`observability`).
- `cloudflare_dns_record.wildcard`: a proxied `AAAA 100::` record for
  `*.iroh-do.<zone>`. It only makes the names resolve to Cloudflare; the route
  serves every request, so nothing is sent to `100::`.
- `cloudflare_workers_route.wildcard`: `*.iroh-do.<zone>/*` to the script. It
  covers the endpoint hostnames and the reserved `admin.iroh-do.<zone>`.
- `cloudflare_certificate_pack.wildcard`: an Advanced Certificate Manager (ACM)
  certificate for `<zone>`, `iroh-do.<zone>` and `*.iroh-do.<zone>`. The API
  requires the apex in every pack. Validation is by TXT record, which Cloudflare
  manages on full-setup zones.

It also reads the account's script list (`data.cloudflare_workers_scripts`) to
decide whether the migration still needs to be sent; see `main.tf`.

Versions are pinned exactly: OpenTofu 1.13.1 and `cloudflare/cloudflare` 5.28.1,
with no other providers (`versions.tf`, `.terraform.lock.hcl`). State is local
by default; `examples/backend-r2.tf.example` configures an R2 backend instead.

## Before the first apply

1. The zone must be active on Cloudflare with full DNS setup.
2. Enable ACM on the zone (dashboard: SSL/TLS, Edge Certificates). This module
   does not manage the subscription. Universal SSL does not cover a two-level
   wildcard such as `*.iroh-do.<zone>`, and without ACM the certificate pack
   fails to create.
3. Workers Paid is recommended. SQLite-backed Durable Objects also run on the
   Free plan, within its limits.
4. Copy `examples/terraform.tfvars.example` to `infra/<name>.auto.tfvars`
   (gitignored) and set `account_id`, `zone_id` and `zone_name`.
5. Create an API token with these permissions, with Account Resources limited
   to your account and Zone Resources limited to the one zone:

   | Scope | Permission | Used for |
   |---|---|---|
   | Account | Workers Scripts: Edit | the script, its bindings and migration, and the script list |
   | Zone | Workers Routes: Edit | the wildcard route |
   | Zone | DNS: Edit | the wildcard record |
   | Zone | SSL and Certificates: Edit | the certificate pack |
   | Zone | Zone: Read | zone lookups by the provider |

   Export it as `CLOUDFLARE_API_TOKEN`. The provider reads it from the
   environment; no file in this repository contains it.

## Usage

Run everything through the `justfile` at the repository root. It passes secrets
through the environment and never prints them.

```sh
export CLOUDFLARE_API_TOKEN=...
just up       # build, tofu init + apply, wait until the certificate serves
just mint     # POST https://admin.iroh-do.<zone>/mint, writes E to .demo/endpoint
just demo     # iroh-do-demo lifecycle <E>
just plan     # should report zero diff
just down     # typed confirmation, then tofu destroy (deletes every identity)
```

`just up` creates two secrets in `.demo/` (gitignored, mode 600).
`.demo/admin_token` becomes `TF_VAR_admin_token`; it authorizes `POST /mint`
and nothing else, and there is no endpoint that returns keys.
`.demo/state_passphrase` becomes `TF_VAR_state_passphrase` and encrypts state
and plan files. To run `tofu` by hand, export both variables the same way.
`just infra-check` runs `fmt` and `validate` without credentials.

### Logs

The Worker writes one JSON object per event to Workers Logs. `log_level` sets
the `LOG_LEVEL` binding, which selects the events; changing it is an in-place
update of the script. The events, their fields and example queries are in
`docs/design-notes/010-structured-logs.md`.

### State encryption

`encryption.tf` uses OpenTofu's client-side encryption: a `pbkdf2` key provider
on `var.state_passphrase`, the `aes_gcm` method, and `enforced = true` for state
and plan. The admin token is a `secret_text` binding and is stored in state, so
state is never written in plaintext. The encryption block is evaluated early,
so every `tofu` command, including `init`, needs `TF_VAR_state_passphrase`.

Losing the passphrase loses the state file, not the endpoints: identities live
only in Durable Object storage. To recover, choose a new passphrase, start from
empty state and `tofu import` each resource. Do not apply from empty state over
a live script; see Teardown for why.

### Plan stability

The bundle files are uploaded from `worker_bundle_dir` (default
`../build/worker`) with their SHA-256. `BUNDLE_SHA256 = sha256(sha256(index.js)
‖ sha256(index_bg.wasm))` is also a plain-text binding, so a rebuild that
changes only the wasm still shows in the plan, and an identical rebuild shows
nothing. `just build` prints the same value, and the object reports a prefix of
it in `/status`.

## Costs

Prices are from Cloudflare's pricing pages on 2026-10-21.

ACM costs $10 per month per zone. It is the one fixed cost the wildcard
hostname forces, and `just down` does not cancel it.

Workers Paid costs at least $5 per month and includes 1M Durable Object
requests and 400,000 GB-s of duration per month. Beyond that, requests cost
$0.15 per million; incoming WebSocket messages count 20:1, so each relayed batch
of QUIC datagrams costs 1/20 of a request, and outgoing messages are free.
Duration costs $12.50 per million GB-s, billed at 128 MB per object. An object
that is hibernated, or eligible to hibernate, is not billed for duration; a Warm
object stays resident at most 60 s after a peer's last send (design note 005).
SQLite use is negligible: an identity row, counters, a schedule and the demo
message log per endpoint.

A single endpoint used for a few demo runs a day stays within the included
amounts, so the practical cost is ACM plus the Workers plan. On Workers Free the
daily limits are 100,000 requests and 13,000 GB-s.

## Teardown

`just down` prints a warning, asks you to type `destroy <script>`, then runs
`tofu destroy`. This deletes the Worker script, the wildcard DNS record, the
route and the certificate pack.

Deleting the script also deletes its Durable Object namespace and all of its
storage, which holds every endpoint's secret key
(`docs/verification/cloudflare.md`, V19). This cannot be undone. Applying again
creates a new, empty namespace, and every previously minted EndpointId is
unreachable from then on. Its pkarr record keeps pointing at
`https://<E>.iroh-do.<zone>/` until n0's server expires it (7 days by default),
but no object can answer for it.

`just down` does not delete the zone's ACM subscription (cancel it in the
dashboard), the API token, or the secrets in `.demo/`.

Because of this:

- Do not rename `script_name` or a class once endpoints matter. This module
  has no `renamed_classes` or `transferred_classes` migration, so a rename is
  a delete plus a create.
- If a plan shows `cloudflare_workers_script.iroh_do` must be replaced, stop:
  replacement deletes every identity.
- Everything else can be destroyed and re-applied freely; the cost is minting
  new endpoints with new ids.

## Verified and unverified

Verified in production: after the first apply and one refresh, `just plan`
reported no changes, and again after an apply that only changed the bundle
(which also exercised the skipped migration on an already-migrated script).
Those applies used a token with more permissions than the table above. It
showed one requirement that is easy to miss: refreshing the route reads it back
with `GET /zones/{zone}/workers/routes/{id}`, which returns 403 (code 10000)
unless the token holds Workers Routes: Edit on the zone, even when listing and
creating routes succeed.

Not yet verified:

- that the five permissions above are sufficient on their own;
- a first deploy on a fresh account with the current `main.tf` (migration sent
  with `old_tag = null`), including the refresh step inside `just up`.

Certificate issuance takes minutes. `just up` polls `https://admin.<suffix>/mint`
for up to 15 minutes, then runs a refresh-only apply so the active certificate
status is recorded in state.
