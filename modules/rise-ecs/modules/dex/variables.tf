variable "name" {
  description = "Installation name used in AWS resource names."
  type        = string
}

variable "tags" {
  description = "Tags applied to managed resources."
  type        = map(string)
}

variable "enabled" {
  description = "Deploy the demo identity provider and its service discovery registration."
  type        = bool
}

variable "cluster_arn" {
  description = "ECS cluster hosting the demo identity provider."
  type        = string
}

variable "discovery" {
  description = "Private DNS namespace ID and install-scoped service name."
  type = object({
    namespace_id = string
    name         = string
  })
}

variable "network" {
  description = "Private task subnets and Dex security group."
  type = object({
    subnet_ids        = list(string)
    security_group_id = string
  })
}

variable "logging" {
  description = "CloudWatch log group and AWS region."
  type = object({
    group_name = string
    region     = string
  })
}

variable "task" {
  description = "Container image, task execution role and CPU architecture."
  type = object({
    image              = string
    execution_role_arn = string
    cpu_architecture   = string
  })
}

variable "identity" {
  sensitive   = true
  description = "Public OIDC issuer, client registration and static demo administrator."
  type = object({
    issuer                = string
    client_id             = string
    client_secret         = string
    public_url            = string
    admin_email           = string
    admin_password_bcrypt = string
  })
}

variable "ingress" {
  description = "Public routing domain, Traefik entrypoint and certificate resolver selection."
  type = object({
    domain       = string
    entrypoint   = string
    acme_enabled = bool
  })
}

variable "runtime_services" {
  description = "Rise and Traefik service IDs whose creation precedes the Dex service."
  type        = list(string)
}
