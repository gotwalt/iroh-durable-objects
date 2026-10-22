# deploy/tofu: OpenTofu module for an iroh-do deployment

This module deploys an iroh-do Worker bundle to a Cloudflare zone: the bundle
`deploy/build-worker.sh` builds from a crate that invokes `iroh_do::worker!`.
One Worker script, `iroh-do` by default, contains the router and two Durable
Object classes: `endpoint`, one object per endpoint, and `registry`, the
endpoint list. Cloudflare names their namespaces `<script>_endpoint` and
`<script>_registry`. Each endpoint `E` is served at
`https://<E>.<hostname_prefix>.<zone>/`. The design is in `docs/spec.md` §13 and
design notes 001, 002 and 012.

The module configures no provider and keeps no state. A root module does both;
`examples/deployment/infra/` is this repository's, for the chat and echo
examples, and the `justfile` drives it. Examples below use the default
`hostname_prefix`, `iroh-do`.

Endpoint secret keys are generated inside the Worker and stored only in Durable
Object storage. They never pass through OpenTofu, your shell or the state file,
which also means Cloudflare holds every endpoint's key (spec P8). `tofu destroy`
deletes all of them, and they cannot be recovered (see [Teardown](#teardown)).

## Using the module

```hcl
provider "cloudflare" {} # reads CLOUDFLARE_API_TOKEN

module "iroh_do" {
  source = "<path or git URL>/deploy/tofu"

  account_id        = "…"
  zone_id           = "…"
  zone_name         = "example.com"
  worker_bundle_dir = "build/worker" # relative to the root module
  admin_token       = var.admin_token # >= 32 characters; authorizes POST /mint only
}
```

The bundle is given in one of two ways; exactly one input must be set.

- `worker_bundle_dir`: the directory `deploy/build-worker.sh` writes, with
  `index.js` and `index_bg.wasm`. The Rust deployment uses this.
- `worker_manifest`: the path to the `manifest.json` of a wrangler dry-run
  outdir (format in `deploy/BUILD.md`). The main module, one uploaded part per
  entry of `modules` (keyed by `name`, which wrangler derives from the content
  hash) and the `BUNDLE_SHA256` binding come from it. Paths in the manifest are
  relative to its directory. A precondition on the script fails the plan unless
  the main module imports each part as `"./<name>"`, because the provider does
  not check part names and a mismatch would fail only at apply.

```hcl
module "iroh_ts" {
  source = "<path or git URL>/deploy/tofu"

  account_id      = "…"
  zone_id         = "…"
  zone_name       = "example.com"
  hostname_prefix = "iroh-ts"
  script_name     = "iroh-ts"
  worker_manifest = "build/iroh-ts/manifest.json"
  admin_token     = var.admin_token_ts

  endpoint_class_name    = "Echo" # default "endpoint"
  endpoint_binding_name  = "ENDPOINTS"
  extra_endpoint_classes = [{ class_name = "Chat", binding_name = "CHAT" }]
  registry_class_name    = "Registry" # default "registry"; null binds no registry
  registry_binding_name  = "REGISTRY"
  migrations = [
    { tag = "v1", new_sqlite_classes = ["Echo"] },
    { tag = "v2", new_sqlite_classes = ["Chat", "Registry"] },
  ]
}
```

`endpoint_class_name`, `endpoint_binding_name`, `registry_class_name` and
`registry_binding_name` default to the Rust deployment's `endpoint`,
`ENDPOINTS`, `registry` and `REGISTRY`. `extra_endpoint_classes` binds further
endpoint classes, for a deployment with several apps (design note 013).

`migrations` lists the Durable Object migrations, oldest first. Unset, there is
one, `v1`, creating every bound class, as before. To add a class to a live
script, append a step with a new tag; never change or remove a step that was
applied. The module reads the live script's tag and sends only the steps after
it: one step as `old_tag`/`new_tag`/`new_sqlite_classes`, several (a new
script) as `steps`. The plan fails if the live tag is not in the list, or if a
bound class is created by no step.

Several instances of the module can share a root module and a zone if each has
its own `hostname_prefix` and `script_name`; each creates its own certificate
pack.

The other inputs and their defaults are in `variables.tf`; the outputs
(`admin_url`, `endpoint_url_template`, `zone_suffix`, `script_name`) in
`outputs.tf`. The root module should
encrypt its state: the admin token is stored there
(`examples/deployment/infra/encryption.tf`).

## What it creates

- `cloudflare_workers_script.iroh_do`: the bundle (with `worker_bundle_dir`,
  `index.js` as the main module and `index_bg.wasm` imported by it; with
  `worker_manifest`, the modules the manifest lists), the bindings `ENDPOINTS`
  (the `endpoint` namespace), `REGISTRY` (the `registry` namespace; omitted when
  `registry_class_name` is null), `ADMIN_TOKEN` (secret), `ZONE_SUFFIX`,
  `BUNDLE_SHA256`, `PKARR_RELAY` and `LOG_LEVEL`, the Durable Object migrations
  (by default `v1`, whose `new_sqlite_classes` lists the bound classes), and Workers Logs and
  traces (`observability`). The binding and class names given here are the
  defaults; the `*_binding_name` and `*_class_name` variables change them.
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
decide whether the migration still needs to be sent; see `worker.tf`.
`worker.tf` holds the script, `hostnames.tf` the DNS record, route and
certificate.

Versions are pinned exactly: OpenTofu 1.13.1 and `cloudflare/cloudflare` 5.28.1,
with no other providers (`versions.tf`; this repository's lock file is in
`examples/deployment/infra/`). The root module's state is local by default;
`examples/deployment/infra/backend-r2.tf.example` configures an R2 backend
instead.

## Before the first apply

1. The zone must be active on Cloudflare with full DNS setup.
2. Enable ACM on the zone (dashboard: SSL/TLS, Edge Certificates). This module
   does not manage the subscription. Universal SSL does not cover a two-level
   wildcard such as `*.iroh-do.<zone>`, and without ACM the certificate pack
   fails to create.
3. Workers Paid is recommended. SQLite-backed Durable Objects also run on the
   Free plan, within its limits.
4. For this repository's deployment, copy
   `examples/deployment/infra/terraform.tfvars.example` to
   `examples/deployment/infra/<name>.auto.tfvars` (gitignored) and set
   `account_id`, `zone_id` and `zone_name`.
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

## This repository's deployment

`examples/deployment/infra/` instantiates the module twice: `module.iroh_do`,
the Rust bundle from `build/worker/` at `*.iroh-do.<zone>`, and
`module.iroh_ts`, the TypeScript deployment (`examples/ts-deployment`: the
chat and echo classes and the registry) from `build/iroh-ts/manifest.json` at
`*.iroh-ts.<zone>` (design note 013). They are separate scripts with separate
Durable Object namespaces, certificates and admin tokens. The classes and
bindings in `module "iroh_ts"` must match `examples/ts-deployment/wrangler.toml`.
`just build-ts` builds `build/iroh-ts/`, and `just plan` and `just apply` cover
both modules; they need that directory to exist. `TS=1 just mint chat` mints in
the TypeScript deployment.

Run everything through the `justfile` at the repository root. It passes secrets
through the environment and never prints them.

```sh
export CLOUDFLARE_API_TOKEN=...
just up       # build, tofu init + apply, wait until the certificate serves
just mint chat   # POST https://admin.iroh-do.<zone>/mint, writes E to .demo/endpoint
just demo     # iroh-do-chat lifecycle <E>
just mint echo && just echo "$(cat .demo/endpoint)"
just plan     # should report zero diff
just down     # typed confirmation, then tofu destroy (deletes every identity)
```

`just up` creates three secrets in `.demo/` (gitignored, mode 600).
`.demo/admin_token` becomes `TF_VAR_admin_token`; it authorizes `POST /mint`
and nothing else, and there is no endpoint that returns keys.
`.demo/admin_token_ts` is the same for `module.iroh_ts`
(`TF_VAR_admin_token_ts`); `just plan`, `apply` and `down` create it if missing.
`.demo/state_passphrase` becomes `TF_VAR_state_passphrase` and encrypts state
and plan files. To run `tofu` by hand, export both variables the same way.
`just infra-check` runs `fmt` and `validate` without credentials.

### Logs

The Worker writes one JSON object per event to Workers Logs. `log_level` sets
the `LOG_LEVEL` binding, which selects the events; changing it is an in-place
update of the script. The events, their fields and example queries are in
`docs/design-notes/010-structured-logs.md`.

### State encryption

The root module's `encryption.tf` (`examples/deployment/infra/encryption.tf`)
uses OpenTofu's client-side encryption: a `pbkdf2` key provider
on `var.state_passphrase`, the `aes_gcm` method, and `enforced = true` for state
and plan. The admin token is a `secret_text` binding and is stored in state, so
state is never written in plaintext. The encryption block is evaluated early,
so every `tofu` command, including `init`, needs `TF_VAR_state_passphrase`.

Losing the passphrase loses the state file, not the endpoints: identities live
only in Durable Object storage. To recover, choose a new passphrase, start from
empty state and `tofu import` each resource. Do not apply from empty state over
a live script; see Teardown for why.

### Plan stability

The bundle files are uploaded from `worker_bundle_dir` with their SHA-256; this
repository's root passes `build/worker` at the repository root. `BUNDLE_SHA256 = sha256(sha256(index.js)
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
  replacement deletes every identity. Deleting a script also deletes the routes
  that point at it, so a replacement leaves the route to be created again by a
  second apply (design note 011, phase 5).
- Everything else can be destroyed and re-applied freely; the cost is minting
  new endpoints with new ids.

## Verified and unverified

Verified in production, with the configuration as it was before design note
012 (one root module, the same resources): after the first apply, `just plan`
reported no changes, and again after an apply that only changed the bundle
(which also exercised the skipped migration on an already-migrated script).
Replacing the script (design note 011, phase 5) showed that deleting a script
deletes its routes; the route is now replaced with the script
(`replace_triggered_by`). Those applies used a token with more permissions
than the table above. It showed one requirement that is easy to miss:
refreshing the route reads it back with `GET /zones/{zone}/workers/routes/{id}`,
which returns 403 (code 10000) unless the token holds Workers Routes: Edit on
the zone, even when listing and creating routes succeed.

Verified by reading the provider (5.28.1, `internal/services/workers_script/`
`resource.go`) and OpenTofu (1.13.1, `internal/tofu/node_resource_abstract_instance.go`),
not by a deploy: on create and on update the provider sends `migrations` from
the configuration and stores that value in state. OpenTofu applies with the
configuration as written, not with `ignore_changes` applied, and then requires
the stored value to equal the planned one. Design note 012 had added
`ignore_changes = [migrations]` to hide the diff left after the first apply,
when the configuration stops sending the migration. With it, the plan keeps the
old value, so any later update of a script whose state holds a migration
(`module.iroh_ts`, created with `v1`) would fail with "Provider produced
inconsistent result after apply" after the API call had succeeded. Step 2 of
design note 013 removed it. As before note 012, an apply that sends a migration
leaves a diff (`migrations` to null) that one more apply clears.
`module.iroh_do`'s state holds null (design note 011, phase 5), so its plan is
unchanged.

Not yet verified:

- that the five permissions above are sufficient on their own;
- a first deploy on a fresh account with this module.

Certificate issuance takes minutes. `just up` polls `https://admin.<suffix>/mint`
for up to 15 minutes. While it waits, the pack's status is in
`tofu state show module.iroh_do.cloudflare_certificate_pack.wildcard`.
