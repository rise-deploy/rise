# The one contract that must not drift between the production module and the
# e2e test root: what the Rise control plane's environment and Traefik routing
# labels look like on ECS. Both consume this; everything else about the two
# topologies is allowed to differ.
#
# Values fill the ${...} interpolations in the config/ecs.yaml that ships in the
# image, selected with RISE_CONFIG_RUN_MODE=ecs.

variable "ingress_domain" {
  description = "Domain apps are served under. Projects reach <project>.<domain>."
  type        = string
}

variable "control_plane_host" {
  description = <<-EOT
    Hostname Rise itself answers on. Null keeps `rise.<ingress_domain>`.

    Set it to `ingress_domain` to serve Rise at the apex, with projects on the
    labels below it. Anything at or under `ingress_domain` is reachable without
    further DNS or certificate work: the module already points both the apex and
    the wildcard at the load balancer.
  EOT
  type        = string
  default     = null
}

variable "ingress_scheme" {
  description = "http or https. Drives PUBLIC_URL, cookie security and the URLs Rise hands out."
  type        = string

  validation {
    condition     = contains(["http", "https"], var.ingress_scheme)
    error_message = "ingress_scheme must be http or https."
  }
}

variable "region" {
  type = string
}

variable "admin_email" {
  type = string
}

variable "cluster_name" {
  type = string
}

variable "subnet_ids" {
  description = "Subnets deployed workloads run in."
  type        = list(string)
}

variable "security_group_ids" {
  description = "Security groups deployed workloads run in."
  type        = list(string)
}

variable "assign_public_ip" {
  description = "Whether deployed workloads get public IPs. True only where there is no NAT."
  type        = bool
  default     = false
}

variable "execution_role_arn" {
  type = string
}

variable "workload_task_role_arn" {
  type = string
}

variable "log_group_name" {
  type = string
}

variable "log_retention_days" {
  type = number
}

variable "auth_backend_url" {
  description = "Reachable from inside the cluster. Traefik calls it for every forwardAuth subrequest; the backend refuses to start when empty."
  type        = string
}

variable "traefik_api_url" {
  description = "Traefik's API. Readiness comes from serverStatus with no fallback."
  type        = string
}

variable "traefik_entrypoint" {
  type = string
}

variable "traefik_certresolver" {
  description = "Traefik ACME resolver name, or null for no TLS termination at Traefik."
  type        = string
  default     = null
}

variable "oidc_issuer" {
  type = string
}

variable "oidc_client_id" {
  type = string
}

variable "oidc_group_claim" {
  type    = string
  default = "groups"
}

variable "admin_idp_group" {
  type    = string
  default = null
}

variable "platform_access_policy" {
  type    = string
  default = "allow_all"
}

variable "platform_allowed_idp_group" {
  type    = string
  default = null
}

variable "allow_private_ssrf" {
  description = <<-EOT
    Let Rise make outbound requests to private addresses and over plain HTTP.
    Required when the OIDC issuer is an in-cluster address such as a Cloud Map
    name; must stay false when the issuer is public.
  EOT
  type        = bool
  default     = false
}

variable "controller_class_name" {
  description = <<-EOT
    Which controller class this install owns. Two installs sharing one cluster
    must differ: ServiceTags::is_managed keys on it, so same-class installs each
    treat the other's services as their own orphans and delete them.
  EOT
  type        = string
  default     = "default"
}

variable "label_namespace" {
  description = <<-EOT
    Prefix for the Rise-owned labels and tags, matching the backend's
    `deployment.label_namespace`. Only change it in step with that setting: a
    Traefik constraint written against the old prefix stops matching.
  EOT
  type        = string
  default     = "rise.dev"
}

variable "resource_prefix" {
  type    = string
  default = "rise"
}

variable "ssm_parameter_prefix" {
  type    = string
  default = "rise"
}

variable "ssm_kms_key_arn" {
  type    = string
  default = null
}

variable "cpu_architecture" {
  type    = string
  default = "X86_64"

  validation {
    condition     = contains(["X86_64", "ARM64"], var.cpu_architecture)
    error_message = "cpu_architecture must be X86_64 or ARM64."
  }
}

variable "reconcile_interval_secs" {
  type    = number
  default = 30
}

variable "max_replicas" {
  type    = number
  default = 10
}

variable "registry" {
  description = <<-EOT
    Registry configuration. `type` is ecr or oci-client-auth; gitlab and jfrog
    are refused by the backend on ECS, since both issue short-lived scoped pull
    tokens and ECS re-authenticates at every task start with no refresh hook.

    For ecr, account_id must be the ECS credentials' own account: Rise writes no
    ECR repository policy, so cross-account pulls cannot work and the backend
    asserts the match at startup.
  EOT
  type = object({
    type          = string
    account_id    = optional(string)
    push_role_arn = optional(string)
    repo_prefix   = optional(string, "rise/")
    auto_remove   = optional(bool, false)
    registry_url  = optional(string)
    namespace     = optional(string, "rise-apps")
  })

  validation {
    condition     = contains(["ecr", "oci-client-auth"], var.registry.type)
    error_message = "registry.type must be \"ecr\" or \"oci-client-auth\"; the ECS backend rejects gitlab and jfrog at startup."
  }

  validation {
    condition     = var.registry.type != "ecr" || try(endswith(var.registry.repo_prefix, "/"), false)
    error_message = "registry.repo_prefix must end in \"/\"; it is concatenated onto the project name literally."
  }
}

variable "repository_credentials_secret_arn" {
  description = "Secrets Manager secret for a private non-ECR registry."
  type        = string
  default     = null
}
