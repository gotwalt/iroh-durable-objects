variable "account_id" {
  description = "Cloudflare account that owns the Worker."
  type        = string

  validation {
    condition     = can(regex("^[0-9a-f]{32}$", var.account_id))
    error_message = "account_id must be a 32-character lowercase hex id."
  }
}

variable "zone_id" {
  description = "Id of zone_name."
  type        = string

  validation {
    condition     = can(regex("^[0-9a-f]{32}$", var.zone_id))
    error_message = "zone_id must be a 32-character lowercase hex id."
  }
}

variable "zone_name" {
  description = "Zone apex. Endpoints are served at https://<E>.<hostname_prefix>.<zone_name>/."
  type        = string
}

variable "hostname_prefix" {
  description = "Subdomain of zone_name holding the endpoints: one DNS label."
  type        = string
  default     = "iroh-do"

  validation {
    condition     = can(regex("^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$", var.hostname_prefix))
    error_message = "hostname_prefix must be one lowercase DNS label."
  }
}

variable "script_name" {
  description = "Worker script name. Changing it deletes every endpoint and its key."
  type        = string
  default     = "iroh-do"
}

variable "worker_bundle_dir" {
  description = "Directory holding index.js and index_bg.wasm, as deploy/build-worker.sh writes them."
  type        = string
}

variable "admin_token" {
  description = "Bearer token for POST https://admin.<hostname_prefix>.<zone_name>/mint."
  type        = string
  sensitive   = true

  validation {
    condition     = length(var.admin_token) >= 32
    error_message = "admin_token must be at least 32 characters."
  }
}

variable "compatibility_date" {
  description = "Workers compatibility date; the same as local workerd's."
  type        = string
  default     = "2026-09-15"
}

variable "pkarr_relay" {
  description = "pkarr relay each endpoint publishes its record to. Empty disables publishing."
  type        = string
  default     = "https://dns.iroh.link/pkarr"
}

variable "log_level" {
  description = "tracing filter for the Worker's logs (design note 010)."
  type        = string
  default     = "info,iroh=warn,noq=warn,netwatch=error,iroh_do=info,iroh_relay_ingress=info"
}

variable "certificate_authority" {
  description = "CA of the certificate for *.<hostname_prefix>.<zone_name>. Let's Encrypt's duplicate-certificate limit is easy to hit when redeploying."
  type        = string
  default     = "google"

  validation {
    condition     = contains(["lets_encrypt", "google", "ssl_com"], var.certificate_authority)
    error_message = "certificate_authority must be lets_encrypt, google or ssl_com."
  }
}

variable "certificate_validity_days" {
  description = "Certificate validity; Cloudflare renews it."
  type        = number
  default     = 90
}
