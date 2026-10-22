# The module's inputs (deploy/tofu/variables.tf), set from *.auto.tfvars and
# TF_VAR_*, plus the state passphrase this root module uses.

variable "account_id" {
  description = "Cloudflare account id that owns the Worker script."
  type        = string

  validation {
    condition     = can(regex("^[0-9a-f]{32}$", var.account_id))
    error_message = "account_id must be a 32-char lowercase hex Cloudflare id."
  }
}

variable "zone_id" {
  description = "Zone id of zone_name."
  type        = string

  validation {
    condition     = can(regex("^[0-9a-f]{32}$", var.zone_id))
    error_message = "zone_id must be a 32-char lowercase hex Cloudflare id."
  }
}

variable "zone_name" {
  description = "The zone apex, e.g. example.com. Endpoints live at https://<E>.<hostname_prefix>.<zone_name>/."
  type        = string
}

variable "hostname_prefix" {
  description = "Subdomain of zone_name that holds the endpoint hostnames. A single DNS label."
  type        = string
  default     = "iroh-do"

  validation {
    condition     = can(regex("^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$", var.hostname_prefix))
    error_message = "hostname_prefix must be a single lowercase DNS label."
  }
}

variable "script_name" {
  description = "Worker script name. Durable Object namespaces are named <script_name>_endpoint and <script_name>_registry. Renaming it creates new namespaces: every existing endpoint identity is lost."
  type        = string
  default     = "iroh-do"
}

variable "admin_token" {
  description = "Bearer token for POST https://admin.<hostname_prefix>.<zone_name>/mint. It authorizes minting only. `just up` generates one."
  type        = string
  sensitive   = true

  validation {
    condition     = length(var.admin_token) >= 32
    error_message = "admin_token must be at least 32 characters (just up uses openssl rand -hex 32)."
  }
}

variable "compatibility_date" {
  description = "Workers compatibility date. Kept equal to the newest date the local workerd (wrangler 4.130.0) accepts, so local and production behave the same."
  type        = string
  default     = "2026-09-15"
}

variable "pkarr_relay" {
  description = "pkarr relay each object publishes its record to, at mint time and every 24 h. Empty disables publishing."
  type        = string
  default     = "https://dns.iroh.link/pkarr"
}

variable "log_level" {
  description = "LOG_LEVEL binding: tracing filter directives for the Worker's JSON logs (docs/design-notes/010-structured-logs.md)."
  type        = string
  default     = "info,iroh=warn,noq=warn,netwatch=error,iroh_do=info"
}

variable "certificate_authority" {
  description = "CA for the Advanced Certificate Manager pack covering *.<hostname_prefix>.<zone_name>."
  type        = string
  # Let's Encrypt limits duplicate certificates for the same hostnames per
  # week; a few `just down` / `just up` cycles in a day exhaust it and leave the
  # pack in pending_validation until the limit expires.
  default = "google"

  validation {
    condition     = contains(["lets_encrypt", "google", "ssl_com"], var.certificate_authority)
    error_message = "certificate_authority must be lets_encrypt, google or ssl_com."
  }
}

variable "certificate_validity_days" {
  description = "Validity of the ACM certificate (Let's Encrypt supports 90 only). Cloudflare renews automatically."
  type        = number
  default     = 90
}

variable "state_passphrase" {
  description = "Passphrase for OpenTofu state/plan encryption (encryption.tf). Supply via TF_VAR_state_passphrase; `just up` keeps it in .demo/state_passphrase. Losing it makes the state unreadable."
  type        = string
  sensitive   = true

  validation {
    condition     = length(var.state_passphrase) >= 16
    error_message = "state_passphrase must be at least 16 characters (pbkdf2 key provider minimum)."
  }
}
