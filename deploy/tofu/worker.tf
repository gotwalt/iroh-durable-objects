# The Worker script: a bundle built from a crate that invokes `iroh_do::worker!`
# (deploy/build-worker.sh), containing the router and the Durable Object classes
# `endpoint` and `registry`.

locals {
  zone_suffix = "${var.hostname_prefix}.${var.zone_name}"

  bundle_js   = "${var.worker_bundle_dir}/index.js"
  bundle_wasm = "${var.worker_bundle_dir}/index_bg.wasm"

  # Content address of the bundle, bound as BUNDLE_SHA256 and reported by every
  # endpoint's status. The provider treats files[*].content_sha256 as computed,
  # so a rebuild that changes only the wasm would not show in a plan without
  # this binding. `just build` prints the same value.
  bundle_sha256 = sha256("${filesha256(local.bundle_js)}${filesha256(local.bundle_wasm)}")

  # The Durable Object classes, created by one migration. It is sent only while
  # the live script has not applied it, as wrangler does: the provider sends
  # `migrations` from the configuration on every update (provider 5.28.1,
  # internal/services/workers_script/resource.go, Update), and the API rejects a
  # migration whose old_tag is not the script's current tag (412, code 10079).
  # Changing the classes means recreating the deployment; there is no upgrade
  # path.
  migration_tag = "v1"
  applied_tag = try(one([
    for s in data.cloudflare_workers_scripts.account.result : s.migration_tag
    if s.id == var.script_name
  ]), null)
  migrations = local.applied_tag == local.migration_tag ? null : {
    new_tag            = local.migration_tag
    new_sqlite_classes = ["endpoint", "registry"]
  }
}

data "cloudflare_workers_scripts" "account" {
  account_id = var.account_id
}

resource "cloudflare_workers_script" "iroh_do" {
  account_id  = var.account_id
  script_name = var.script_name

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
    { type = "durable_object_namespace", name = "ENDPOINTS", class_name = "endpoint" },
    { type = "durable_object_namespace", name = "REGISTRY", class_name = "registry" },
    { type = "secret_text", name = "ADMIN_TOKEN", text = var.admin_token },
    { type = "plain_text", name = "ZONE_SUFFIX", text = local.zone_suffix },
    { type = "plain_text", name = "BUNDLE_SHA256", text = local.bundle_sha256 },
    { type = "plain_text", name = "PKARR_RELAY", text = var.pkarr_relay },
    { type = "plain_text", name = "LOG_LEVEL", text = var.log_level },
  ]

  migrations = local.migrations

  # The API fills in any observability attribute left out, and the provider
  # then reports it as drift, so every attribute is spelled out.
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

  lifecycle {
    # After the first apply the configuration stops sending the migration
    # (above); without this, every later plan would show it being removed.
    ignore_changes = [migrations]
  }
}
