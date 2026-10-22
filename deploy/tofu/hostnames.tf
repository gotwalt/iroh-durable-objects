# Proxied, so the wildcard names resolve to Cloudflare; the route answers every
# request, and nothing reaches 100::.
resource "cloudflare_dns_record" "wildcard" {
  zone_id = var.zone_id
  name    = "*.${local.zone_suffix}"
  type    = "AAAA"
  content = "100::"
  proxied = true
  ttl     = 1
  comment = "Worker route target for iroh-do endpoints"
}

resource "cloudflare_workers_route" "wildcard" {
  zone_id = var.zone_id
  pattern = "*.${local.zone_suffix}/*"
  script  = cloudflare_workers_script.iroh_do.script_name

  lifecycle {
    # Deleting a script deletes its routes.
    replace_triggered_by = [cloudflare_workers_script.iroh_do.id]
  }
}

# Universal SSL does not cover a name two levels below the apex. The API
# requires the apex in every pack.
resource "cloudflare_certificate_pack" "wildcard" {
  zone_id               = var.zone_id
  type                  = "advanced"
  hosts                 = [var.zone_name, local.zone_suffix, "*.${local.zone_suffix}"]
  validation_method     = "txt"
  validity_days         = var.certificate_validity_days
  certificate_authority = var.certificate_authority
  cloudflare_branding   = false
}
