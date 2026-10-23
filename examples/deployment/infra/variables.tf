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

variable "state_passphrase" {
  type      = string
  sensitive = true
}
