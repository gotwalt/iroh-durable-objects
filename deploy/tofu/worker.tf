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

  # Part name => file. The name is the import specifier in the main module. The
  # manifest lists the main module among its modules; it is uploaded as the
  # script's content, not as a part.
  bundle_files = local.manifest == null ? {
    "index_bg.wasm" = {
      content_file = "${var.worker_bundle_dir}/index_bg.wasm"
      content_type = "application/wasm"
    }
    } : {
    for m in local.manifest.modules : m.name => {
      content_file = "${local.manifest_dir}/${m.path}"
      content_type = m.content_type
    } if m.name != local.manifest.main_module
  }

  # Bound as BUNDLE_SHA256 and reported in each endpoint's status. It also makes
  # a wasm-only rebuild show in the plan, which files[*].content_sha256 does not.
  # With a manifest, the build computes it.
  bundle_sha256 = local.manifest == null ? sha256("${filesha256(local.bundle_js)}${filesha256(local.bundle_files["index_bg.wasm"].content_file)}") : local.manifest.bundle_sha256

  do_bindings = concat(
    [{ type = "durable_object_namespace", name = var.endpoint_binding_name, class_name = var.endpoint_class_name }],
    [for c in var.extra_endpoint_classes : { type = "durable_object_namespace", name = c.binding_name, class_name = c.class_name }],
    [for c in compact([var.registry_class_name]) : { type = "durable_object_namespace", name = var.registry_binding_name, class_name = c }],
  )
  do_classes = concat(
    [var.endpoint_class_name],
    [for c in var.extra_endpoint_classes : c.class_name],
    compact([var.registry_class_name]),
  )

  # The migrations that create the Durable Object classes are sent only while
  # the live script lacks them, as wrangler does: the provider sends
  # `migrations` from the configuration on every update, and Cloudflare rejects
  # a migration whose old_tag is not the script's current tag. So the script's
  # applied tag is read from the account, and only the steps after it are sent.
  migrations     = var.migrations != null ? var.migrations : [{ tag = "v1", new_sqlite_classes = local.do_classes }]
  migration_tags = [for m in local.migrations : m.tag]
  applied_tag = one([
    for s in data.cloudflare_workers_scripts.account.result : s.migration_tag
    if s.id == var.script_name
  ])
  # -1 for a script that does not exist yet or has no migration; null for a
  # tag this configuration does not know (a precondition fails the plan).
  applied_index = local.applied_tag == null ? -1 : try(index(local.migration_tags, local.applied_tag), null)
  pending       = local.applied_index == null ? [] : slice(local.migrations, local.applied_index + 1, length(local.migrations))
  # One pending step is sent in the form step 1 used; several (a new script)
  # as `steps`, which the API applies in order.
  migration = length(local.pending) == 0 ? null : {
    old_tag            = local.applied_tag
    new_tag            = local.pending[length(local.pending) - 1].tag
    new_sqlite_classes = length(local.pending) == 1 ? local.pending[0].new_sqlite_classes : null
    steps              = length(local.pending) == 1 ? null : [for m in local.pending : { new_sqlite_classes = m.new_sqlite_classes }]
  }
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

  migrations = local.migration

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
    # No ignore_changes on `migrations`. The provider (5.28.1, resource.go,
    # Create and Update) sends the configuration's value and stores it in state,
    # and OpenTofu (1.13.1) checks the stored value against the plan. With
    # ignore_changes the plan keeps the old value, so an update that sends a new
    # migration, or none, fails that check after the API call has succeeded.
    # The cost: after an apply that sent a migration, the next plan shows
    # `migrations` going to null, and one more apply (an upload with no
    # migration) clears it; `just up` makes that apply. It is a full script upload,
    # so the script's live Durable Objects restart (Cloudflare's documented
    # behaviour for a new deployment; not tested here). A script whose state
    # already holds null, such as module.iroh_do's, plans with no change.

    precondition {
      condition     = local.applied_index != null
      error_message = "The live script's migration tag is not in this configuration's migrations."
    }

    precondition {
      condition     = alltrue([for c in local.do_classes : contains(flatten([for m in local.migrations : m.new_sqlite_classes]), c)])
      error_message = "A bound Durable Object class is not created by any migration."
    }

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
