variable "name" {
  description = "Installation name used in AWS resource names."
  type        = string
}

variable "tags" {
  description = "Tags applied to managed resources."
  type        = map(string)
}

variable "vpc" {
  description = "Existing VPC and subnets, or null to create the network."
  type = object({
    id                  = string
    public_subnet_ids   = list(string)
    private_subnet_ids  = list(string)
    database_subnet_ids = list(string)
  })
}

variable "topology" {
  description = "CIDR, availability zones, egress and interface endpoint settings for a managed VPC."
  type = object({
    cidr                    = string
    availability_zone_count = number
    nat_gateway_mode        = string
    enable_vpc_endpoints    = bool
  })
}

variable "region" {
  description = "AWS region for endpoint service names."
  type        = string
}

variable "endpoint_security_group_id" {
  description = "Interface endpoint security group supplied by the security module."
  type        = string
}
