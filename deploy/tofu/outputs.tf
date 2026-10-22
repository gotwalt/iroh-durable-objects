output "admin_url" {
  description = "Admin URL: POST /mint with the admin token."
  value       = "https://admin.${local.zone_suffix}"
}

output "endpoint_url_template" {
  description = "Relay URL of endpoint {E}."
  value       = "https://{E}.${local.zone_suffix}/"
}

output "zone_suffix" {
  description = "<hostname_prefix>.<zone_name>."
  value       = local.zone_suffix
}

output "script_name" {
  description = "Worker script name."
  value       = cloudflare_workers_script.iroh_do.script_name
}
