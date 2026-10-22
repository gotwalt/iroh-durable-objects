# Set in <name>.auto.tfvars (terraform.tfvars.example).
variable "account_id" {
  description = "Cloudflare account id that owns the Worker script."
  type        = string
}

variable "zone_id" {
  description = "Zone id of zone_name."
  type        = string
}

variable "zone_name" {
  description = "The zone apex. Endpoints live at https://<E>.iroh-do.<zone_name>/."
  type        = string
}

# Set by the justfile from .demo/, never in a file OpenTofu reads.
variable "admin_token" {
  description = "Bearer token for POST /mint (TF_VAR_admin_token; `just up` generates it)."
  type        = string
  sensitive   = true
}

variable "state_passphrase" {
  description = "Passphrase of the state and plan encryption (encryption.tf; TF_VAR_state_passphrase). Losing it makes the state unreadable."
  type        = string
  sensitive   = true

  validation {
    condition     = length(var.state_passphrase) >= 16
    error_message = "state_passphrase must be at least 16 characters (pbkdf2 key provider minimum)."
  }
}
