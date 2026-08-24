terraform {
  required_version = ">= 1.5.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 6.0"
    }
  }

  # Partially configured: the bucket name depends on the account, and this
  # workspace creates the bucket itself. `terraform init -backend-config=...`
  # supplies it -- see the README's first-apply sequence, which is the one time
  # the chicken-and-egg has to be worked around by hand.
  backend "s3" {
    key = "bootstrap/terraform.tfstate"
  }
}
