variable "name" {
  description = "Installation name used in AWS resource names."
  type        = string
}

variable "tags" {
  description = "Tags applied to managed resources."
  type        = map(string)
}

variable "network" {
  description = "VPC, endpoint creation flag and NAT addresses for demo OIDC discovery."
  type = object({
    vpc_id           = string
    create_endpoints = bool
    nat_public_ips   = map(string)
  })
}

variable "ingress_cidr_blocks" {
  description = "IPv4 CIDRs allowed to reach the public load balancer."
  type        = list(string)
}

variable "acme_enabled" {
  description = "Create the certificate storage security group and NFS rule."
  type        = bool
}

variable "deploy_dex" {
  description = "Create demo identity-provider rules, including access through the public edge."
  type        = bool
}
