module "iroh_do" {
  source = "../../../deploy/tofu"

  account_id        = var.account_id
  zone_id           = var.zone_id
  zone_name         = var.zone_name
  worker_bundle_dir = "../../../build/worker"
  admin_token       = var.admin_token
}
