variable "name" {
  description = "Must match the bootstrap's `name`."
  type        = string
  default     = "rise-e2e"
}

variable "region" {
  type    = string
  default = "eu-central-1"
}

variable "state_bucket" {
  description = "Bootstrap's S3 state bucket, from its `state_bucket` output."
  type        = string
}

variable "rise_image" {
  description = <<-EOT
    Pulled anonymously: Fargate's execution role has no registry credentials for
    it, and the runner's own GHCR login does not reach the task. The package
    must therefore be public. A private one needs a Secrets Manager secret and
    `repositoryCredentials`, which is what the production module does.
  EOT
  type        = string
  default     = "ghcr.io/rise-deploy/rise"
}

variable "rise_image_tag" {
  description = "The image under test. No default: the run must be explicit about what it exercises."
  type        = string
}

variable "scope" {
  description = <<-EOT
    Identifier isolating this run from every other one sharing the cluster, e.g.
    `pr-457`. It is one token doing three jobs:

      * the DNS subtree, `*.<scope>.<dns_zone_name>`;
      * Rise's `deployment_controller_class`, which scopes its orphan collector
        so one run never deletes another's services;
      * the Traefik discovery constraint, so this run's Traefik routes only
        containers carrying that class.

    Must be a single DNS label.
  EOT
  type        = string

  validation {
    condition     = can(regex("^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$", var.scope))
    error_message = "scope must be a single lowercase DNS label."
  }
}

variable "dns_zone_name" {
  description = "Bootstrap's zone. This run is served under `<scope>.<dns_zone_name>`."
  type        = string
}

variable "jwt_signing_secret" {
  description = <<-EOT
    Base64 HS256 key, generated fresh by the harness each run and used to mint
    the admin bearer.

    Not the shipped default: this environment is persistent and publicly
    addressed, and that default is a constant in a repository, so anyone able to
    reach the API could mint an admin token offline.
  EOT
  type        = string
  sensitive   = true
}

variable "encryption_key" {
  description = "Base64 32-byte AES-GCM-256 key. Per run; nothing outlives the run that would need it again."
  type        = string
  sensitive   = true
}

variable "admin_email" {
  type    = string
  default = "admin@example.com"
}

variable "postgres_image" {
  type    = string
  default = "public.ecr.aws/docker/library/postgres:18-alpine"
}

variable "cpu_architecture" {
  type    = string
  default = "X86_64"

  validation {
    condition     = contains(["X86_64", "ARM64"], var.cpu_architecture)
    error_message = "cpu_architecture must be X86_64 or ARM64."
  }
}

variable "traefik_image" {
  type    = string
  default = "traefik:v3.7.10"
}

variable "dex_image" {
  type    = string
  default = "dexidp/dex:v2.45.1"
}

variable "authorized_cidrs" {
  description = <<-EOT
    Addresses allowed to reach Traefik on port 80 for the length of this run --
    in practice the single address the harness is driven from. Empty leaves the
    edge closed, which is the correct default: nothing but the run itself has
    any business reaching it.
  EOT
  type        = list(string)
  default     = []
}
