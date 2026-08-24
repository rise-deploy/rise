terraform {
  required_version = ">= 1.0"

  required_providers {
    aws = {
      source = "hashicorp/aws"
      # 6.0 is the floor because it is the major this module is tested against;
      # nothing validates it on 5.x, and provider majors change resource schemas.
      # The features it actually needs are older --
      # aws_vpc_security_group_ingress_rule and its egress counterpart, used by
      # the RDS section, arrived in 5.0 -- so the floor is about what is
      # verified, not about what is required.
      version = ">= 6.0"
    }
  }
}
