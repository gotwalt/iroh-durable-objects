output "endpoint_url_template" {
  description = "Relay URL of endpoint E (z-base-32). Also published to pkarr at mint time."
  value       = module.iroh_do.endpoint_url_template
}

output "admin_url" {
  description = "Admin surface; POST /mint with Authorization: Bearer <admin_token>."
  value       = module.iroh_do.admin_url
}

output "zone_suffix" {
  description = "Value of the ZONE_SUFFIX binding."
  value       = module.iroh_do.zone_suffix
}

output "script_name" {
  description = "Worker script name. Deleting the script deletes every endpoint identity."
  value       = module.iroh_do.script_name
}

output "certificate_status" {
  description = "ACM certificate pack status; HTTPS to <E>.<zone_suffix> fails until it is \"active\"."
  value       = module.iroh_do.certificate_status
}
