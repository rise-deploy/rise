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
    Public hosted zone for the environment. It must already exist and the domain
    must be delegated to it -- the bootstrap reads the zone rather than creating
    one, so a second zone for the same name cannot appear. The harness UPSERTs a
    per-run scope beneath it (`*.<scope>.<zone>`) at every run start, so runs
    never collide over one name.
  EOT
  type        = string
  default     = "rise-deploy.click"
}

variable "vpc_cidr" {
  type    = string
  default = "10.44.0.0/16"
}

variable "availability_zone_count" {
  type    = number
  default = 2
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

variable "enable_ci_bootstrap_role" {
  description = <<-EOT
    Create a second OIDC role that can apply this workspace from CI.

    Off by default because it can write IAM, and a principal that can create
    roles and attach policies can escalate to whatever the account allows. Turn
    it on only where that is an acceptable trade -- and narrow
    `ci_bootstrap_subjects` when you do.
  EOT
  type        = bool
  default     = false
}

variable "ci_bootstrap_subjects" {
  description = <<-EOT
    GitHub OIDC subjects allowed to assume the bootstrap-apply role. Empty
    defaults to the repository's develop branch.

    Unlike the run role this is not `repo:<repo>:*`: a branch anyone can push,
    or a pull request, must not reach an identity that can write IAM. A GitHub
    Environment with required reviewers -- `repo:<repo>:environment:<name>` --
    puts a human in the loop.
  EOT
  type        = list(string)
  default     = []
}
