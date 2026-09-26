terraform {
  # rise-ecs's floor: write-only secret arguments.
  required_version = ">= 1.11.0"

  # Local state on purpose. The whole install lives and dies inside one CI job
  # against an emulator that forgets everything when its container stops, so
  # there is nothing a remote backend would protect.

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 6.50"
    }
    random = {
      source  = "hashicorp/random"
      version = ">= 3.6"
    }
  }
}
