locals {
  zone_suffix = "${var.hostname_prefix}.${var.zone_name}"

  # Two ways to describe the bundle. worker_bundle_dir is the layout of
  # deploy/build-worker.sh. worker_manifest is the manifest of a wrangler
  # dry-run outdir, whose wasm part is named by its content hash, so the names
  # cannot be fixed here.
  manifest     = var.worker_manifest == null ? null : jsondecode(file(var.worker_manifest))
  manifest_dir = var.worker_manifest == null ? null : dirname(var.worker_manifest)

  main_module = local.manifest == null ? "index.js" : local.manifest.main_module
  bundle_js   = local.manifest == null ? "${var.worker_bundle_dir}/index.js" : "${local.manifest_dir}/${local.manifest.main_module}"

  # Part name => file. The name is the import specifier in the main module.
  bundle_files = local.manifest == null ? {
    "index_bg.wasm" = {
      content_file = "${var.worker_bundle_dir}/index_bg.wasm"
      content_type = "application/wasm"
    }
    } : {
    for m in local.manifest.modules : m.name => {
      content_file = "${local.manifest_dir}/${m.path}"
      content_type = m.content_type
    }
  }

  # Bound as BUNDLE_SHA256 and reported in each endpoint's status. It also makes
  # a wasm-only rebuild show in the plan, which files[*].content_sha256 does not.
  # With a manifest, the build computes it.
  bundle_sha256 = local.manifest == null ? sha256("${filesha256(local.bundle_js)}${filesha256(local.bundle_files["index_bg.wasm"].content_file)}") : local.manifest.bundle_sha256

  do_bindings = concat(
    [{ type = "durable_object_namespace", name = var.endpoint_binding_name, class_name = var.endpoint_class_name }],
    [for c in compact([var.registry_class_name]) : { type = "durable_object_namespace", name = var.registry_binding_name, class_name = c }],
  )
  do_classes = compact([var.endpoint_class_name, var.registry_class_name])

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

  main_module    = local.main_module
  content_file   = local.bundle_js
  content_sha256 = filesha256(local.bundle_js)
  files          = local.bundle_files

  compatibility_date  = var.compatibility_date
  compatibility_flags = []

  bindings = concat(local.do_bindings, [
    { type = "secret_text", name = "ADMIN_TOKEN", text = var.admin_token },
    { type = "plain_text", name = "ZONE_SUFFIX", text = local.zone_suffix },
    { type = "plain_text", name = "BUNDLE_SHA256", text = local.bundle_sha256 },
    { type = "plain_text", name = "PKARR_RELAY", text = var.pkarr_relay },
    { type = "plain_text", name = "LOG_LEVEL", text = var.log_level },
  ])

  migrations = local.applied_tag == local.migration_tag ? null : {
    new_tag            = local.migration_tag
    new_sqlite_classes = local.do_classes
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

    # The provider does not check part names against imports; a mismatch would
    # fail only at apply. wrangler writes the import as "./<part name>".
    precondition {
      condition = local.manifest == null || alltrue([
        for name, _ in local.bundle_files : strcontains(file(local.bundle_js), "\"./${name}\"")
      ])
      error_message = "A module named in the manifest is not imported by the main module as \"./<name>\"."
    }
  }
}
