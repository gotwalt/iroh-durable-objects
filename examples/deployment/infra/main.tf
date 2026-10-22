module "iroh_do" {
  source = "../../../deploy/tofu"

  account_id        = var.account_id
  zone_id           = var.zone_id
  zone_name         = var.zone_name
  worker_bundle_dir = "../../../build/worker"
  admin_token       = var.admin_token
}

# The TypeScript host (design note 013), beside the Rust deployment above until
# the Rust one is retired. Its own script, hostname prefix, certificate and
# admin token. The manifest is written by `just build-ts` from
# examples/ts-deployment (deploy/BUILD.md). The classes and bindings here must
# match examples/ts-deployment/wrangler.toml.
module "iroh_ts" {
  source = "../../../deploy/tofu"

  account_id      = var.account_id
  zone_id         = var.zone_id
  zone_name       = var.zone_name
  hostname_prefix = "iroh-ts"
  script_name     = "iroh-ts"
  worker_manifest = "../../../build/iroh-ts/manifest.json"
  admin_token     = var.admin_token_ts

  endpoint_class_name   = "Echo"
  endpoint_binding_name = "ENDPOINTS"
  extra_endpoint_classes = [
    { class_name = "Chat", binding_name = "CHAT" },
  ]
  registry_class_name   = "Registry"
  registry_binding_name = "REGISTRY"

  # v1 is what step 1 applied to the live script. v2 adds the chat and the
  # registry; only the steps after the script's current tag are sent.
  migrations = [
    { tag = "v1", new_sqlite_classes = ["Echo"] },
    { tag = "v2", new_sqlite_classes = ["Chat", "Registry"] },
  ]
}
