terraform {
  required_version = ">= 1.11.0"
  required_providers {
    aws    = { source = "hashicorp/aws", version = ">= 6.50" }
    random = { source = "hashicorp/random", version = ">= 3.6" }
  }
}
