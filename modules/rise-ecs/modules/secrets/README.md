# `secrets`

Owns persistent signing and encryption keys and their Secrets Manager secrets,
plus the supplied OIDC client credential. `environment` exposes ECS secret
references and waits for populated secret versions. Secret versions use
write-only values; generated key resources retain their values in Terraform state.

This child is composed by [`rise-ecs`](../..). It declares provider requirements;
provider configuration and state belong to the caller.
