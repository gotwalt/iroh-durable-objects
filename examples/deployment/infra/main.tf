# This repository's deployment of the chat and echo examples, through the
# iroh-do module in deploy/tofu. The bundle is built from examples/deployment
# by `just build-worker`.
module "iroh_do" {
  source = "../../../deploy/tofu"

  account_id                = var.account_id
  zone_id                   = var.zone_id
  zone_name                 = var.zone_name
  hostname_prefix           = var.hostname_prefix
  script_name               = var.script_name
  worker_bundle_dir         = "${path.root}/../../../build/worker"
  admin_token               = var.admin_token
  compatibility_date        = var.compatibility_date
  pkarr_relay               = var.pkarr_relay
  log_level                 = var.log_level
  certificate_authority     = var.certificate_authority
  certificate_validity_days = var.certificate_validity_days
}

# The resources were declared in this directory's predecessor, infra/, before
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
