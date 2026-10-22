# OpenTofu client-side encryption of state and plan files (checked against
# 1.13.1, which evaluates variables in this block early).
#
# The admin token is a secret_text binding and is stored in state, so
# `enforced = true` refuses to write unencrypted state or plans.
#
# Losing the passphrase loses the state file, not the endpoint identities,
# which live only in Durable Object storage. Recover with `tofu import`.
terraform {
  encryption {
    key_provider "pbkdf2" "state" {
      passphrase = var.state_passphrase
    }

    method "aes_gcm" "state" {
      keys = key_provider.pbkdf2.state
    }

    state {
      method   = method.aes_gcm.state
      enforced = true
    }

    plan {
      method   = method.aes_gcm.state
      enforced = true
    }
  }
}
