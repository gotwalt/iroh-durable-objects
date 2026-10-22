# Serving https://<E>.<hostname_prefix>.<zone_name>/ and the reserved
# admin.<hostname_prefix>.<zone_name> from the script.

# Makes the wildcard names resolve to Cloudflare's edge. The route serves every
# request, so nothing is sent to 100::.
resource "cloudflare_dns_record" "wildcard" {
  zone_id = var.zone_id
  name    = "*.${local.zone_suffix}"
  type    = "AAAA"
  content = "100::"
  proxied = true
  ttl     = 1
  comment = "iroh-do example: Worker route target for <E>.${local.zone_suffix} (managed by OpenTofu)"
}

resource "cloudflare_workers_route" "wildcard" {
  zone_id = var.zone_id
  pattern = "*.${local.zone_suffix}/*"
  script  = cloudflare_workers_script.iroh_do.script_name

  lifecycle {
    # Deleting a script deletes its routes, so a replaced script needs a new
    # route, not an update of one that no longer exists (design note 011,
    # phase 5).
    replace_triggered_by = [cloudflare_workers_script.iroh_do.id]
  }
}

# Universal SSL covers only one level below the apex, and *.<hostname_prefix>.
# <zone_name> is two levels deep, so it needs an Advanced Certificate Manager
# pack. The API requires the apex in every pack's host list. The zone's ACM
# subscription is not managed here (README.md).
resource "cloudflare_certificate_pack" "wildcard" {
  zone_id               = var.zone_id
  type                  = "advanced"
  hosts                 = [var.zone_name, local.zone_suffix, "*.${local.zone_suffix}"]
  validation_method     = "txt"
  validity_days         = var.certificate_validity_days
  certificate_authority = var.certificate_authority
  cloudflare_branding   = false
}
