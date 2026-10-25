terraform {
  # 1.11 is the floor for write-only resource arguments. Secret values must not
  # be returned by the provider during refresh or retained on secret-version
  # resources in state.
  required_version = ">= 1.11.0"

  required_providers {
    aws = {
      source = "hashicorp/aws"
      # 6.50 includes consistent handling for unknown write-only values during
      # planning. Database endpoints and generated credentials are unknown on a
      # first plan, so an older provider cannot safely represent this module.
      version = ">= 6.50"
    }
    random = {
      source = "hashicorp/random"
      # random_bytes, for the signing and encryption keys.
      version = ">= 3.6"
    }
  }
}
