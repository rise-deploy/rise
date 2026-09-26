variable "name" {
  description = "Installation name used in AWS resource names."
  type        = string
}

variable "tags" {
  description = "Tags applied to managed resources."
  type        = map(string)
}

variable "network" {
  description = "VPC and public subnets for the load balancer; private subnets keyed by stable identifiers for EFS mounts."
  type = object({
    vpc_id                 = string
    public_subnet_ids      = list(string)
    private_subnets_by_key = map(string)
  })
}

variable "security_groups" {
  description = "Load balancer and certificate storage security group IDs."
  type = object({
    edge = string
    efs  = string
  })
}

variable "edge" {
  description = "TLS topology, optional ALB certificate and load balancer deletion protection."
  type = object({
    mode                = string
    acm_certificate_arn = string
    deletion_protection = bool
  })
}

variable "dns" {
  description = <<-EOT
    Ingress domain and optional Route 53 zone for apex and wildcard records.
    `create` decides the record count on its own, because a zone created in
    the same apply has no ID until then.
  EOT
  type = object({
    create  = bool
    zone_id = string
    domain  = string
  })
}

variable "traefik_role" {
  description = "Traefik task role ownership, optional supplied ARN and trusted AWS account."
  type = object({
    create     = bool
    arn        = string
    account_id = string
  })
}
