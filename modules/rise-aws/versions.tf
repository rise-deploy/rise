terraform {
  required_version = ">= 1.0"

  required_providers {
    aws = {
      source = "hashicorp/aws"
      # 5.0 is the floor, not a preference: aws_vpc_security_group_ingress_rule
      # and its egress counterpart (used by the RDS section) were introduced in
      # that major version.
      version = ">= 5.0"
    }
  }
}
