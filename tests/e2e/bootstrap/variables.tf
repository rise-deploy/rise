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

variable "dns_zone_id" {
  description = <<-EOT
    The public hosted zone to serve this environment from. It must already
    exist, and the domain must be delegated to it.

    Taken as an id rather than created here, or looked up by name, for two
    reasons. Creating one invites a *second* zone for a domain that already has
    one -- registering through Route 53 Domains creates a zone and delegates to
    it, so a `resource` here yields two valid zones with different nameservers,
    only one of which the domain points at; records written to the other
    resolve for nobody. Looking one up by name reintroduces that ambiguity and
    costs an API call at plan time, which the credential-free plan tests cannot
    make.
  EOT
  type        = string

  validation {
    condition     = can(regex("^Z[A-Z0-9]+$", var.dns_zone_id))
    error_message = "dns_zone_id must be a Route 53 hosted zone id, e.g. Z09798271JB9GIJW4BB9X."
  }
}

variable "dns_zone_name" {
  description = <<-EOT
    The domain `dns_zone_id` serves, without a trailing dot. The harness UPSERTs
    a per-run scope beneath it (`*.<scope>.<zone>`) at every run start, so runs
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
