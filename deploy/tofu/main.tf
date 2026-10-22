# An iroh-do deployment: one Worker script, built from a crate that invokes
# `iroh_do::worker!`, containing the router and the `endpoint` and `registry`
# Durable Object classes, served at https://<E>.<hostname_prefix>.<zone_name>/
# through a wildcard route. The module owns every Cloudflare resource of the
# deployment; the root module configures the provider and keeps the state.
# Endpoint secret keys are generated inside the Worker (POST /mint) and never
# pass through OpenTofu.

locals {
  zone_suffix = "${var.hostname_prefix}.${var.zone_name}"

  bundle_js   = "${var.worker_bundle_dir}/index.js"
  bundle_wasm = "${var.worker_bundle_dir}/index_bg.wasm"

  # Content address of the bundle, also bound as BUNDLE_SHA256. The provider
  # treats files[*].content_sha256 as computed, so a rebuild that changes only
  # the wasm is not guaranteed to show in the plan; this binding makes it show.
  # The object reports a prefix in /status; `just build` prints the same value.
  bundle_sha256 = sha256("${filesha256(local.bundle_js)}${filesha256(local.bundle_wasm)}")

  # The Durable Object classes, declared in a single migration. Cloudflare names
  # each namespace `<script_name>_<class>`, e.g. `iroh-do_endpoint`. It is sent only
  # when the live script has not applied it: the API rejects an upload whose
  # migration `old_tag` differs from the script's current tag (412, code
  # 10079), so an unconditional `migrations` block could create the script but
  # never update it. There is no upgrade path; changing the classes means
  # recreating the deployment.
  do_migration_tag = "v1"
  deployed_migration_tag = try(one([
    for s in data.cloudflare_workers_scripts.account.result : s.migration_tag
    if s.id == var.script_name
  ]), null)
  do_migrations = local.deployed_migration_tag == local.do_migration_tag ? null : {
    new_tag            = local.do_migration_tag
    new_sqlite_classes = ["endpoint", "registry"]
  }
}

data "cloudflare_workers_scripts" "account" {
  account_id = var.account_id
}

resource "cloudflare_workers_script" "iroh_do" {
  account_id  = var.account_id
  script_name = var.script_name

  # worker-build output: the ESM entry and the wasm module it imports as
  # "./index_bg.wasm" (deploy/BUILD.md).
  main_module    = "index.js"
  content_file   = local.bundle_js
  content_sha256 = filesha256(local.bundle_js)
  files = {
    "index_bg.wasm" = {
      content_file = local.bundle_wasm
      content_type = "application/wasm"
    }
  }

  compatibility_date  = var.compatibility_date
  compatibility_flags = []

  bindings = [
    {
      type       = "durable_object_namespace"
      name       = "ENDPOINTS"
      class_name = "endpoint"
    },
    {
      type       = "durable_object_namespace"
      name       = "REGISTRY"
      class_name = "registry"
    },
    {
      type = "secret_text"
      name = "ADMIN_TOKEN"
      text = var.admin_token
    },
    {
      type = "plain_text"
      name = "ZONE_SUFFIX"
      text = local.zone_suffix
    },
    {
      type = "plain_text"
      name = "BUNDLE_SHA256"
      text = local.bundle_sha256
    },
    {
      type = "plain_text"
      name = "PKARR_RELAY"
      text = var.pkarr_relay
    },
    {
      type = "plain_text"
      name = "LOG_LEVEL"
      text = var.log_level
    },
  ]

  # Renaming a class or the script deletes its namespace and every endpoint
  # key in it: there is no renamed_classes or transferred_classes migration.
  migrations = local.do_migrations

  # Every attribute is spelled out, including defaults: the API fills in any
  # that are omitted, and provider 5.28.1 then reports them as drift on the
  # next plan.
  observability = {
    enabled            = true
    head_sampling_rate = 1
    logs = {
      enabled            = true
      head_sampling_rate = 1
      invocation_logs    = true
      persist            = true
    }
    traces = {
      enabled            = true
      head_sampling_rate = 1
      persist            = true
    }
  }
}

# Makes the wildcard names resolve to Cloudflare's edge. The route serves every
# request, so nothing is sent to 100::.
resource "cloudflare_dns_record" "wildcard" {
  zone_id = var.zone_id
  name    = "*.${local.zone_suffix}"
  type    = "AAAA"
  content = "100::"
  proxied = true
  ttl     = 1
  comment = "iroh-do example: Worker route target for <E>.${local.zone_suffix} (managed by OpenTofu)"
}

resource "cloudflare_workers_route" "wildcard" {
  zone_id = var.zone_id
  pattern = "*.${local.zone_suffix}/*"
  script  = cloudflare_workers_script.iroh_do.script_name
}

# Universal SSL covers only one level below the apex, and *.iroh-do.<zone> is
# two levels deep, so it needs an Advanced Certificate Manager pack. The API
# requires the apex in every pack's host list. The zone's ACM subscription is
# not managed here (README.md).
resource "cloudflare_certificate_pack" "wildcard" {
  zone_id               = var.zone_id
  type                  = "advanced"
  hosts                 = [var.zone_name, local.zone_suffix, "*.${local.zone_suffix}"]
  validation_method     = "txt"
  validity_days         = var.certificate_validity_days
  certificate_authority = var.certificate_authority
  cloudflare_branding   = false
}
