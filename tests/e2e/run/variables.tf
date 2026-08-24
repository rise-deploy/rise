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
  type    = string
  default = "ghcr.io/rise-deploy/rise"
}

variable "rise_image_tag" {
  description = "The image under test. No default: the run must be explicit about what it exercises."
  type        = string
}

variable "ingress_domain" {
  description = "Bootstrap's zone name. Records are pointed at Traefik's current address by the harness before this applies."
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
