# This repository's deployment of the chat and echo examples, through the
# iroh-do module in deploy/tofu. `just build-worker` builds the bundle from
# examples/deployment into build/worker at the repository root.
module "iroh_do" {
  source = "../../../deploy/tofu"

  account_id        = var.account_id
  zone_id           = var.zone_id
  zone_name         = var.zone_name
  worker_bundle_dir = "../../../build/worker"
  admin_token       = var.admin_token
}

# The resources were declared in infra/, this directory's predecessor, before
# they moved into the module (design note 012).
moved {
  from = cloudflare_workers_script.iroh_do
  to   = module.iroh_do.cloudflare_workers_script.iroh_do
}

moved {
  from = cloudflare_dns_record.wildcard
  to   = module.iroh_do.cloudflare_dns_record.wildcard
}

moved {
  from = cloudflare_workers_route.wildcard
  to   = module.iroh_do.cloudflare_workers_route.wildcard
}

moved {
  from = cloudflare_certificate_pack.wildcard
  to   = module.iroh_do.cloudflare_certificate_pack.wildcard
}
