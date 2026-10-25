terraform {
  required_version = ">= 1.5.0"

  # Partially configured, unlike the bootstrap's: the bucket comes from that
  # workspace's output and the key is per-scope, so the harness supplies both
  # with `terraform init -backend-config`.
  #
  # The block itself is not optional. Without it Terraform does not error on
  # those flags -- it warns "-backend-config was used without a backend block",
  # ignores them, and keeps state locally, where it dies with the runner. A
  # cancelled job would then leak its Traefik, Dex, security groups and Cloud
  # Map entries with no state left to destroy them from.
  backend "s3" {
    encrypt      = true
    use_lockfile = true
  }

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 6.0"
    }
  }
}
