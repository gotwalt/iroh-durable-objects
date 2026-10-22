locals {
  zone_suffix = "${var.hostname_prefix}.${var.zone_name}"

  bundle_js   = "${var.worker_bundle_dir}/index.js"
  bundle_wasm = "${var.worker_bundle_dir}/index_bg.wasm"

  # Bound as BUNDLE_SHA256 and reported in each endpoint's status. It also makes
  # a wasm-only rebuild show in the plan, which files[*].content_sha256 does not.
  bundle_sha256 = sha256("${filesha256(local.bundle_js)}${filesha256(local.bundle_wasm)}")

  # The migration that creates the Durable Object classes is sent only while
  # the live script lacks it, as wrangler does: the provider sends `migrations`
  # on every update, and Cloudflare rejects one the script has already applied.
  migration_tag = "v1"
  applied_tag = one([
    for s in data.cloudflare_workers_scripts.account.result : s.migration_tag
    if s.id == var.script_name
  ])
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

  migrations = local.applied_tag == local.migration_tag ? null : {
    new_tag            = local.migration_tag
    new_sqlite_classes = ["endpoint", "registry"]
  }

  # Every attribute is set: the API fills in omitted ones, which then show as drift.
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
    # Once applied, the migration is no longer sent (above); this keeps that
    # from showing as a change.
    ignore_changes = [migrations]
  }
}
