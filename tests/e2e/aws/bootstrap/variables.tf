variable "name" {
  description = "Prefix for every resource, and the ECS cluster name."
  type        = string
  default     = "rise-e2e"
}

variable "region" {
  description = "AWS region hosting the environment."
  type        = string
  default     = "eu-central-1"
}

variable "github_repository" {
  description = "owner/repo whose workflows may assume the CI role."
  type        = string
  default     = "rise-deploy/rise"

  validation {
    condition     = can(regex("^[^/]+/[^/]+$", var.github_repository))
    error_message = "github_repository must be owner/repo."
  }
}

variable "dns_zone_name" {
  description = <<-EOT
    Public hosted zone for the environment, e.g. e2e.example.com. Delegate it
    once from the parent domain; the harness UPSERTs the apex and wildcard
    records at every run start.
  EOT
  type        = string
}

variable "vpc_cidr" {
  type    = string
  default = "10.44.0.0/16"
}

variable "availability_zone_count" {
  type    = number
  default = 2
}

variable "traefik_image" {
  type    = string
  default = "traefik:v3.7.10"
}

variable "dex_image" {
  type    = string
  default = "dexidp/dex:v2.45.1"
}

variable "log_retention_days" {
  type    = number
  default = 14
}

variable "tags" {
  type    = map(string)
  default = {}
}

variable "create_github_oidc_provider" {
  description = <<-EOT
    Create the GitHub OIDC provider. An account may only have one, so set this
    false if something else in the account already registered it.
  EOT
  type        = bool
  default     = true
}
