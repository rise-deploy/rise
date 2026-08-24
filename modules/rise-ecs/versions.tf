terraform {
  # 1.5 for `check` blocks and object-type `optional()` defaults, both of which
  # the create-or-bring inputs rely on.
  required_version = ">= 1.5.0"

  required_providers {
    aws = {
      source = "hashicorp/aws"
      # 6.0 is the floor because it is the major this module is tested against.
      # The oldest feature it actually needs is older: security groups on network
      # load balancers (5.15), without which NLB client-IP preservation would
      # force the Traefik security group to admit 0.0.0.0/0 directly.
      version = ">= 6.0"
    }
    random = {
      source = "hashicorp/random"
      # random_bytes, for the signing and encryption keys.
      version = ">= 3.6"
    }
  }
}
