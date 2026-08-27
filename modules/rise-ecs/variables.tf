# -----------------------------------------------------------------------------
# Identity
# -----------------------------------------------------------------------------

variable "name" {
  description = "Name prefix for every resource. Must match the `name` given to modules/rise-aws."
  type        = string
  default     = "rise"
}

variable "tags" {
  description = "Tags applied to every resource this module creates."
  type        = map(string)
  default     = {}
}

# -----------------------------------------------------------------------------
# Composition with modules/rise-aws
#
# This module consumes IAM; it creates none of the identities Rise itself uses.
# Apply modules/rise-aws with enable_ecs = true and wire its outputs in.
# -----------------------------------------------------------------------------

variable "controller_role_arn" {
  description = "modules/rise-aws `role_arn` — the Rise control plane's task role."
  type        = string
}

variable "execution_role_arn" {
  description = "modules/rise-aws `ecs_execution_role_arn` — the role ECS assumes to pull images and resolve secrets."
  type        = string
}

variable "workload_task_role_arn" {
  description = <<-EOT
    Task role for deployed workloads (deployment_controller.task_role_arn).
    Defaults to controller_role_arn, which is what modules/rise-aws's
    ecs_task_role_arn returns: per-project task roles are not implemented.
  EOT
  type        = string
  default     = null
}

# -----------------------------------------------------------------------------
# Registry
# -----------------------------------------------------------------------------

variable "traefik_task_role_arn" {
  description = <<-EOT
    Pre-created task role for Traefik. Leave null and the module creates one.

    Set it where the applying identity has no IAM-write -- the e2e harness runs
    that way, with IAM pre-created in a bootstrap apply.
  EOT
  type        = string
  default     = null
}

variable "registry_type" {
  description = <<-EOT
    Container registry Rise deploys from.

    `gitlab` and `jfrog` are absent deliberately, not by omission: both issue
    short-lived scoped pull tokens that Rise refreshes on the puller's behalf,
    and ECS re-authenticates at every task start with no hook to refresh
    anything. The backend refuses to start with either, so this module will not
    emit one.
  EOT
  type        = string
  default     = "ecr"

  validation {
    condition     = contains(["ecr", "oci-client-auth"], var.registry_type)
    error_message = "registry_type must be \"ecr\" or \"oci-client-auth\". The ECS backend rejects gitlab and jfrog at startup."
  }
}

variable "ecr_push_role_arn" {
  description = "modules/rise-aws `push_role_arn`. Required when registry_type is \"ecr\"."
  type        = string
  default     = null
}

variable "ecr_repo_prefix" {
  description = "ECR repository prefix. The trailing slash matters — \"rise\" would produce repositories named \"risemyapp\"."
  type        = string
  default     = "rise/"

  validation {
    condition     = endswith(var.ecr_repo_prefix, "/")
    error_message = "ecr_repo_prefix must end in \"/\"; it is concatenated onto the project name literally."
  }
}

variable "ecr_auto_remove" {
  description = "Delete a project's ECR repository when the project is deleted."
  type        = bool
  default     = false
}

variable "oci_registry_url" {
  description = "Registry host for registry_type = \"oci-client-auth\". Must be reachable from the task subnets."
  type        = string
  default     = null
}

variable "oci_registry_namespace" {
  description = "Namespace under the OCI registry."
  type        = string
  default     = "rise-apps"
}

variable "repository_credentials_secret_arn" {
  description = <<-EOT
    Secrets Manager secret holding {"username", "password"} for a private
    non-ECR registry. ECS re-authenticates at every task start and cannot take
    inline credentials, so a static-credential registry needs them parked where
    ECS can read them itself.
  EOT
  type        = string
  default     = null
}

# -----------------------------------------------------------------------------
# Networking — create, or bring your own
# -----------------------------------------------------------------------------

variable "vpc" {
  description = <<-EOT
    Existing VPC to deploy into. Leave null to have the module create one with
    public, private and database subnets across `availability_zone_count` AZs.

    When set, private_subnet_ids must be non-empty; database_subnet_ids falls
    back to the private subnets, and public_subnet_ids is required unless the
    edge is disabled.
  EOT
  type = object({
    id                  = string
    public_subnet_ids   = optional(list(string), [])
    private_subnet_ids  = list(string)
    database_subnet_ids = optional(list(string), [])
  })
  default = null

  validation {
    condition     = var.vpc == null || length(coalesce(try(var.vpc.private_subnet_ids, null), [])) > 0
    error_message = "vpc.private_subnet_ids must name at least one subnet."
  }

  # The backend rejects more than 16 subnets on an awsvpc network configuration.
  validation {
    condition     = var.vpc == null || length(coalesce(try(var.vpc.private_subnet_ids, null), [])) <= 16
    error_message = "At most 16 private subnets: an awsvpc network configuration accepts no more, and the backend rejects the config at startup."
  }
}

variable "vpc_cidr" {
  description = "CIDR for the created VPC. Ignored when `vpc` is set."
  type        = string
  default     = "10.42.0.0/16"
}

variable "availability_zone_count" {
  description = "AZs to spread across. Two is the floor for a Multi-AZ database."
  type        = number
  default     = 2

  validation {
    condition     = var.availability_zone_count >= 2 && var.availability_zone_count <= 4
    error_message = "availability_zone_count must be between 2 and 4."
  }
}

variable "nat_gateway_mode" {
  description = <<-EOT
    Egress for the private subnets.

    - "single": one NAT gateway. The default; cheapest working option.
    - "per_az": one per AZ, so an AZ failure does not take egress with it.
    - "none": no NAT. Requires enable_vpc_endpoints, and rules out ACME —
      endpoints reach AWS services only, and HTTP-01 needs Let's Encrypt.
  EOT
  type        = string
  default     = "single"

  validation {
    condition     = contains(["single", "per_az", "none"], var.nat_gateway_mode)
    error_message = "nat_gateway_mode must be one of: single, per_az, none."
  }
}

variable "enable_vpc_endpoints" {
  description = <<-EOT
    Create interface endpoints for ECR, Logs, SSM, Secrets Manager, STS and KMS.

    Not a cost saving: eight endpoints across two AZs runs well above a single
    NAT gateway. Choose it for a no-egress posture. The S3 *gateway* endpoint is
    created in every mode regardless — it is free, and ECR layer blobs come from
    S3, so its absence fails the image pull rather than the API call.
  EOT
  type        = bool
  default     = false
}

variable "ingress_cidr_blocks" {
  description = "CIDRs allowed to reach the edge on 80/443."
  type        = list(string)
  default     = ["0.0.0.0/0"]
}

# -----------------------------------------------------------------------------
# Cluster — create, or bring your own
# -----------------------------------------------------------------------------

variable "cluster" {
  description = "Existing ECS cluster to deploy into. Leave null to create one named `name`."
  type = object({
    name = string
  })
  default = null
}

variable "enable_container_insights" {
  description = "Container Insights on the created cluster. Ignored when bringing your own — the module does not mutate a cluster it does not own."
  type        = bool
  default     = false
}

variable "cloud_map_namespace_id" {
  description = "Existing Cloud Map private DNS namespace to register into. Leave null to create one."
  type        = string
  default     = null
}

variable "cloud_map_namespace_name" {
  description = "Name for the created namespace. Internal only; never resolvable from outside the VPC."
  type        = string
  default     = null
}

variable "log_retention_days" {
  description = "Retention for the CloudWatch log group carrying control-plane and workload logs."
  type        = number
  default     = 30
}

variable "log_group_name" {
  description = "CloudWatch log group for the control plane, rise-traefik and deployed applications. Defaults to /<name>."
  type        = string
  default     = null
}

# -----------------------------------------------------------------------------
# Ingress and edge
# -----------------------------------------------------------------------------

variable "ingress_domain" {
  description = <<-EOT
    Domain apps are served under, e.g. "rise.example.com". Projects reach
    <project>.<domain>, so a wildcard DNS record pointing at the load balancer
    is required — see the `dns_records_required` output.
  EOT
  type        = string

  validation {
    condition     = can(regex("^[a-z0-9.-]+$", var.ingress_domain))
    error_message = "ingress_domain must be a bare hostname: no scheme, port or path."
  }
}

variable "control_plane_host" {
  description = <<-EOT
    Hostname Rise itself answers on. Null gives `rise.<ingress_domain>`.

    Set it to the same value as `ingress_domain` to serve Rise at the apex, with
    each project on a label below it — `apps.example.com` for Rise and
    `<project>.apps.example.com` for projects. No extra DNS or certificate work:
    the apex and the wildcard both already point at the load balancer.
  EOT
  type        = string
  default     = null

  validation {
    # Both operands must tolerate null: a validation's `||` evaluates each side
    # rather than short-circuiting, so a null guard on the left does not spare
    # the right. Defaulting to `ingress_domain` makes the unset case pass by the
    # same rule the apex does.
    condition = (
      coalesce(var.control_plane_host, var.ingress_domain) == var.ingress_domain ||
      endswith(coalesce(var.control_plane_host, var.ingress_domain), ".${var.ingress_domain}")
    )
    error_message = "control_plane_host must be ingress_domain itself or a name under it; anything else is covered by neither the DNS records nor the wildcard certificate this module provisions."
  }
}

variable "edge_mode" {
  description = <<-EOT
    - "nlb-traefik-acme": NLB in TCP passthrough, Traefik terminates TLS with
      ACME HTTP-01. Custom domains work by CNAME with no further steps.
    - "alb-acm": ALB terminates with an ACM certificate and forwards to Traefik.
      Buys L7 access logs and WAF; costs custom domains their automatic TLS,
      since ACM has no HTTP-01 and each domain gains a DNS validation step.
  EOT
  type        = string
  default     = "nlb-traefik-acme"

  validation {
    condition     = contains(["nlb-traefik-acme", "alb-acm"], var.edge_mode)
    error_message = "edge_mode must be \"nlb-traefik-acme\" or \"alb-acm\"."
  }
}

variable "acme_email" {
  description = "Registration address for Let's Encrypt. Required when edge_mode is \"nlb-traefik-acme\"."
  type        = string
  default     = null
}

variable "acm_certificate_arn" {
  description = "ACM certificate for the ALB listener. Required when edge_mode is \"alb-acm\"."
  type        = string
  default     = null
}

variable "traefik_image" {
  description = "Traefik image. Pinned by default; Traefik's ECS provider and label vocabulary are version-sensitive."
  type        = string
  default     = "traefik:v3.7.10"
}

variable "traefik_refresh_seconds" {
  description = "How often Traefik's ECS provider polls. Cutover reacts one poll slower than this."
  type        = number
  default     = 15
}

variable "route53_zone_id" {
  description = "Route 53 zone to create the apex and wildcard alias records in. Leave null to manage DNS yourself."
  type        = string
  default     = null
}

# -----------------------------------------------------------------------------
# Database
# -----------------------------------------------------------------------------

variable "database_url_secret_arn" {
  description = <<-EOT
    Existing Secrets Manager secret holding the full postgres:// URL. Set it to
    skip database creation entirely and keep every credential out of Terraform
    state — the recommended production path.
  EOT
  type        = string
  default     = null
}

variable "db_instance_class" {
  description = "RDS instance class for the control-plane database."
  type        = string
  default     = "db.t4g.micro"
}

variable "db_engine_version" {
  description = "PostgreSQL major version. Minor upgrades apply automatically."
  type        = string
  default     = "17"
}

variable "db_allocated_storage" {
  description = "Allocated storage (GiB)."
  type        = number
  default     = 20
}

variable "db_multi_az" {
  description = "Multi-AZ database. ADR-0005 D15's reference topology; roughly doubles the instance cost."
  type        = bool
  default     = false
}

variable "db_backup_retention_days" {
  description = "Automated backup retention."
  type        = number
  default     = 7
}

# -----------------------------------------------------------------------------
# Control plane
# -----------------------------------------------------------------------------

variable "rise_image" {
  description = "Rise control-plane image repository."
  type        = string
  default     = "ghcr.io/rise-deploy/rise"
}

variable "rise_image_tag" {
  description = "Image tag. Set this or rise_image_ref, but not both."
  type        = string
  default     = null
}

variable "rise_image_ref" {
  description = "Complete immutable control-plane image reference, such as ghcr.io/rise-deploy/rise@sha256:…. Set this or rise_image_tag, but not both."
  type        = string
  default     = null

  validation {
    condition     = var.rise_image_ref == null || strcontains(var.rise_image_ref, "@sha256:")
    error_message = "rise_image_ref must be an OCI digest reference containing @sha256:."
  }
}

variable "rise_cpu" {
  description = "Control-plane task CPU units."
  type        = string
  default     = "1024"
}

variable "rise_memory" {
  description = "Control-plane task memory (MiB)."
  type        = string
  default     = "2048"
}

variable "rise_desired_count" {
  description = "Control-plane replicas. Safe above 1: the reconcile loop is leader-elected through rise-runtime-sync."
  type        = number
  default     = 1
}

variable "cpu_architecture" {
  description = "Fargate CPU architecture for the control plane and for deployed workloads."
  type        = string
  default     = "X86_64"

  validation {
    condition     = contains(["X86_64", "ARM64"], var.cpu_architecture)
    error_message = "cpu_architecture must be X86_64 or ARM64."
  }
}

variable "reconcile_interval_secs" {
  description = "Reconcile loop period. ECS is a throttled API; the Docker backend's 5s is far too fast here."
  type        = number
  default     = 30
}

variable "resource_prefix" {
  description = "Prefix for the ECS resources Rise creates for deployments."
  type        = string
  default     = "rise"
}

variable "controller_class_name" {
  description = "Controller identity stamped onto every managed service and used to isolate multiple Rise installations sharing one cluster."
  type        = string
  default     = "default"
}

variable "ssm_parameter_prefix" {
  description = "SSM path for secret environment variables. Must match the value given to modules/rise-aws."
  type        = string
  default     = "rise"
}

variable "ssm_kms_key_arn" {
  description = "CMK for the SSM SecureStrings. Null uses the AWS-managed alias/aws/ssm key."
  type        = string
  default     = null
}

variable "max_replicas" {
  description = "Ceiling on replicas per deployment."
  type        = number
  default     = 10
}

variable "admin_email" {
  description = "Bootstrap admin user (auth.admin_users)."
  type        = string
}

variable "oidc_group_claim" {
  description = "ID-token claim containing identity-provider group names."
  type        = string
  default     = "groups"
}

variable "admin_idp_group" {
  description = "Identity-provider group whose members are Rise administrators. Null grants no group administrative access."
  type        = string
  default     = null
}

variable "platform_access_policy" {
  description = "Who may use the Rise platform: allow_all or restrictive."
  type        = string
  default     = "allow_all"

  validation {
    condition     = contains(["allow_all", "restrictive"], var.platform_access_policy)
    error_message = "platform_access_policy must be allow_all or restrictive."
  }
}

variable "platform_allowed_idp_group" {
  description = "Identity-provider group allowed to use Rise when platform_access_policy is restrictive."
  type        = string
  default     = null

  validation {
    condition     = var.platform_access_policy != "restrictive" || try(trimspace(var.platform_allowed_idp_group), "") != ""
    error_message = "restrictive platform access requires platform_allowed_idp_group."
  }
}

# -----------------------------------------------------------------------------
# Authentication
# -----------------------------------------------------------------------------

variable "oidc_issuer" {
  description = "OIDC issuer URL. Required unless deploy_dex is true."
  type        = string
  default     = null
}

variable "oidc_client_id" {
  description = "OIDC client id for the Rise backend."
  type        = string
  default     = "rise-backend"
}

variable "oidc_client_secret" {
  description = "OIDC client secret. Stored in Secrets Manager and injected, never placed in a plain environment variable."
  type        = string
  default     = null
  sensitive   = true
}

variable "deploy_dex" {
  description = <<-EOT
    Run Dex as an ECS service so the install is usable end to end without an
    existing IdP. A demo, not a production identity provider: storage is
    in-memory, so sessions and refresh tokens are lost whenever the task is
    replaced, and there is no HA.
  EOT
  type        = bool
  default     = false
}

variable "dex_admin_email" {
  description = "Static user for the demo Dex."
  type        = string
  default     = null
}

variable "dex_admin_password_bcrypt" {
  description = <<-EOT
    Bcrypt hash of the demo Dex user's password. Terraform has no bcrypt
    function, so supply it: htpasswd -bnBC 10 "" 'password' | tr -d ':\n'
  EOT
  type        = string
  default     = null
  sensitive   = true
}

variable "dex_image" {
  description = "Dex image for the demo identity provider."
  type        = string
  default     = "dexidp/dex:v2.45.1"
}

# -----------------------------------------------------------------------------
# Operations
# -----------------------------------------------------------------------------

variable "deletion_protection" {
  description = "Deletion protection on the database and load balancer. Leave on outside throwaway installs."
  type        = bool
  default     = true
}

variable "secret_recovery_window_days" {
  description = <<-EOT
    Secrets Manager recovery window. Zero deletes immediately, which is what a
    test install wants: at the default, a destroy followed by an apply collides
    on the still-scheduled secret name.
  EOT
  type        = number
  default     = 7
}

variable "enable_execute_command" {
  description = "Allow `aws ecs execute-command` into the control-plane task. A shell inside Rise; off by default."
  type        = bool
  default     = false
}

variable "traefik_constraints" {
  description = <<-EOT
    Traefik discovery constraint, confining this Traefik to one Rise install's
    containers. Empty (the default) discovers every Rise-labelled container in
    the cluster, which is correct when the cluster hosts a single install.

    Set it when several installs share a cluster -- each must then also run a
    distinct `deployment_controller_class`, both so Traefik can tell the
    containers apart and so one install's orphan collector does not delete
    another's services:

        traefik_constraints = "Label(`rise.dev/controller-class`, `my-install`)"
  EOT
  type        = string
  default     = ""
}
