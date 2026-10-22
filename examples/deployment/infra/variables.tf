variable "account_id" {
  type = string
}

variable "zone_id" {
  type = string
}

variable "zone_name" {
  type = string
}

variable "admin_token" {
  type      = string
  sensitive = true
}

variable "admin_token_ts" {
  description = "Admin token of module.iroh_ts; just supplies it as TF_VAR_admin_token_ts from .demo/admin_token_ts."
  type        = string
  sensitive   = true
}

variable "state_passphrase" {
  type      = string
  sensitive = true
}
