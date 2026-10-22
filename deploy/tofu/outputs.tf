output "endpoint_url_template" {
  description = "Relay URL of endpoint E (z-base-32). Also published to pkarr at mint time."
  value       = "https://{E}.${local.zone_suffix}/"
}

output "admin_url" {
  description = "Admin surface; POST /mint with Authorization: Bearer <admin_token>."
  value       = "https://admin.${local.zone_suffix}"
}

output "zone_suffix" {
  description = "Value of the ZONE_SUFFIX binding."
  value       = local.zone_suffix
}

output "script_name" {
  description = "Worker script name. Deleting the script deletes every endpoint identity."
  value       = cloudflare_workers_script.iroh_do.script_name
}

output "bundle_sha256" {
  description = "Content address of the deployed bundle (BUNDLE_SHA256 binding)."
  value       = local.bundle_sha256
}
