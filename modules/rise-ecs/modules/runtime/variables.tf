variable "cluster" {
  description = "Existing ECS cluster hosting both services."
  type = object({
    name = string
    arn  = string
  })
}

variable "discovery" {
  description = "Existing Cloud Map namespace for internal service addresses."
  type = object({
    namespace_id   = string
    namespace_name = string
  })
}

variable "network" {
  description = "Task networking. The caller permits Traefik to reach Rise on 3000-3001 and Rise to reach Traefik on 8080."
  type = object({
    control_plane = object({
      subnet_ids         = list(string)
      security_group_ids = list(string)
      assign_public_ip   = optional(bool, false)
    })
    traefik = object({
      subnet_ids         = list(string)
      security_group_ids = list(string)
      assign_public_ip   = optional(bool, false)
    })
  })

  validation {
    condition = alltrue([
      for network in [var.network.control_plane, var.network.traefik] :
      length(network.subnet_ids) > 0 && length(network.subnet_ids) <= 16 &&
      length(network.security_group_ids) > 0 && length(network.security_group_ids) <= 5
    ])
    error_message = "Each service requires 1-16 subnets and 1-5 security groups for awsvpc networking."
  }
}

variable "roles" {
  description = "Existing task execution, control-plane, and Traefik IAM roles."
  type = object({
    execution     = string
    control_plane = string
    traefik       = string
  })
}

variable "logging" {
  description = "Existing CloudWatch log group and AWS region."
  type = object({
    group_name = string
    region     = string
  })
}

variable "control_plane" {
  description = "Rise service identity, image, resources, labels, and health-check timing."
  type = object({
    name                              = string
    discovery_name                    = string
    image                             = string
    cpu                               = optional(string, "1024")
    memory                            = optional(string, "2048")
    desired_count                     = optional(number, 1)
    enable_execute_command            = optional(bool, false)
    log_stream_prefix                 = optional(string, "rise")
    health_check_grace_period_seconds = optional(number, 1800)
    deployment_rollback               = optional(bool, true)
    labels                            = map(string)
    health_check = optional(object({
      interval     = optional(number, 30)
      timeout      = optional(number, 5)
      retries      = optional(number, 3)
      start_period = optional(number, 60)
    }), {})
  })

  validation {
    condition = (
      var.control_plane.health_check_grace_period_seconds >= 0 &&
      var.control_plane.health_check_grace_period_seconds <= 2147483647 &&
      floor(var.control_plane.health_check_grace_period_seconds) == var.control_plane.health_check_grace_period_seconds
    )
    error_message = "Control-plane health-check grace period must be an integer from 0 to 2147483647 seconds."
  }
}

variable "environment" {
  description = "Rise environment, composed with control-plane-env. Can contain credentials for ephemeral deployments."
  type        = map(string)
}

variable "secret_environment" {
  description = "ECS secret valueFrom references, keyed by environment-variable name."
  type        = map(string)
  default     = {}
}

variable "traefik" {
  description = "Traefik service identity and ECS discovery settings. HTTP routing and catalog isolation are managed by this module."
  type = object({
    name              = string
    discovery_name    = string
    image             = string
    refresh_seconds   = optional(number, 15)
    constraints       = optional(string, "")
    log_stream_prefix = optional(string, "traefik")
    # Secrets Manager ARN of the bearer token the Rise backend requires on its
    # HTTP-provider endpoint. When set, the task resolves it through the
    # container `secrets` block and presents it as the provider's
    # `Authorization` header; the same secret must reach Rise as
    # RISE_TRAEFIK_CONFIG_SECRET. Null sends no header.
    config_secret_arn = optional(string)
  })
}

variable "acme" {
  description = "Optional ACME certificate store. Keep enabled known during planning even when EFS identifiers are unknown."
  type = object({
    enabled         = optional(bool, false)
    email           = optional(string)
    file_system_id  = optional(string)
    access_point_id = optional(string)
  })
  default = {}

  validation {
    condition = !var.acme.enabled || (
      var.acme.email != null && var.acme.file_system_id != null && var.acme.access_point_id != null
    )
    error_message = "ACME requires a registration email, EFS filesystem, and access point."
  }
}

variable "load_balancer_targets" {
  description = "Target group ARNs keyed by container port. Port keys must be known during planning."
  type        = map(string)
  default     = {}

  validation {
    condition     = alltrue([for port in keys(var.load_balancer_targets) : contains(["80", "443"], port)])
    error_message = "Traefik load-balancer targets must use port 80 or 443."
  }
}

variable "client_ip" {
  description = <<-EOT
    How the real client address reaches Traefik from the load balancer in front
    of it, and which peers may be believed.

    - "none": Traefik trusts no one. It discards inbound X-Forwarded-* headers
      and records its direct peer -- the load balancer node -- as the client.
    - "forwarded-headers": an L7 load balancer (ALB) adds X-Forwarded-For and
      X-Forwarded-Proto. Traefik's `web` entrypoint keeps them when the peer is
      in `trusted_cidrs`.
    - "proxy-protocol": an L4 load balancer (NLB with IP targets and TCP, which
      does not preserve the client address) prepends a PROXY protocol v2 header.
      The `web`, `websecure` and `ping` entrypoints read it from peers in
      `trusted_cidrs`; `ping` carries the load balancer's health check, which
      the load balancer prefixes with the header too.

    `trusted_cidrs` is the load balancer's address range, never the whole
    internet: a trusted peer can claim any client address.

    This must match the ingress module's mode: an NLB there enables PROXY
    protocol v2 on its target groups and requires "proxy-protocol" here, or
    Traefik fails every connection (health checks included). `rise-ecs` couples
    them; callers composing the submodules directly must do so themselves.
  EOT
  type = object({
    source        = optional(string, "none")
    trusted_cidrs = optional(list(string), [])
  })
  default = {}

  validation {
    condition     = contains(["none", "forwarded-headers", "proxy-protocol"], var.client_ip.source)
    error_message = "client_ip.source must be \"none\", \"forwarded-headers\" or \"proxy-protocol\"."
  }

  validation {
    condition     = var.client_ip.source == "none" || length(var.client_ip.trusted_cidrs) > 0
    error_message = "client_ip.trusted_cidrs must name the load balancer's address ranges when client_ip.source is not \"none\"."
  }

  validation {
    condition     = alltrue([for cidr in var.client_ip.trusted_cidrs : can(cidrhost(cidr, 0)) && !contains(["0.0.0.0/0", "::/0"], cidr)])
    error_message = "client_ip.trusted_cidrs must be valid CIDR blocks and must not be 0.0.0.0/0 or ::/0."
  }
}

variable "cpu_architecture" {
  description = "Fargate task CPU architecture."
  type        = string
  default     = "X86_64"

  validation {
    condition     = contains(["X86_64", "ARM64"], var.cpu_architecture)
    error_message = "cpu_architecture must be X86_64 or ARM64."
  }
}

variable "tags" {
  description = "Tags for both services, their task definitions, and discovery services."
  type        = map(string)
  default     = {}
}
