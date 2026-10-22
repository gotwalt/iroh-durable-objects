# Exact pins; no other providers.
terraform {
  required_version = "= 1.13.1"

  required_providers {
    cloudflare = {
      source  = "cloudflare/cloudflare"
      version = "= 5.28.1"
    }
  }

  # Local backend by default; backend-r2.tf.example configures R2.
  # State is encrypted either way (encryption.tf).
}

# Reads CLOUDFLARE_API_TOKEN from the environment, so the token is never in a
# file OpenTofu reads.
provider "cloudflare" {}
