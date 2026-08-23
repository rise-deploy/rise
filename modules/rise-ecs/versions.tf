terraform {
  # 1.5 for `check` blocks and object-type `optional()` defaults, both of which
  # the create-or-bring inputs rely on.
  required_version = ">= 1.5.0"

  required_providers {
    aws = {
      source = "hashicorp/aws"
      # 5.15 introduced security groups on network load balancers. Without one,
      # NLB client-IP preservation would force the Traefik security group to
      # admit 0.0.0.0/0 directly.
      version = ">= 5.15"
    }
    random = {
      source = "hashicorp/random"
      # random_bytes, for the signing and encryption keys.
      version = ">= 3.6"
    }
  }
}
