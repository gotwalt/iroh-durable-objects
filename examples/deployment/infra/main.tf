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
# admin token. The manifest is written by `just build-ts` (deploy/BUILD.md).
# Change the class or binding name here to match the app in the bundle;
# registry_class_name is null because step 1 has no registry.
module "iroh_ts" {
  source = "../../../deploy/tofu"

  account_id      = var.account_id
  zone_id         = var.zone_id
  zone_name       = var.zone_name
  hostname_prefix = "iroh-ts"
  script_name     = "iroh-ts"
  worker_manifest = "../../../build/echo-ts/manifest.json"
  admin_token     = var.admin_token_ts

  endpoint_class_name   = "Echo"
  endpoint_binding_name = "ENDPOINTS"
  registry_class_name   = null
}
