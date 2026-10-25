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
    name                   = string
    discovery_name         = string
    image                  = string
    cpu                    = optional(string, "1024")
    memory                 = optional(string, "2048")
    desired_count          = optional(number, 1)
    enable_execute_command = optional(bool, false)
    log_stream_prefix      = optional(string, "rise")
    labels                 = map(string)
    health_check = optional(object({
      interval     = optional(number, 30)
      timeout      = optional(number, 5)
      retries      = optional(number, 3)
      start_period = optional(number, 60)
    }), {})
  })
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
