provider "aws" {
  region                      = "eu-central-1"
  access_key                  = "AKIAIOSFODNN7EXAMPLE"
  secret_key                  = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
  skip_credentials_validation = true
  skip_requesting_account_id  = true
  skip_metadata_api_check     = true
  skip_region_validation      = true
}

run "accepts_an_external_traefik_role_that_is_unknown_during_planning" {
  command = plan

  module {
    source = "./tests/fixtures/external-traefik-role"
  }

  override_data {
    target = module.rise.data.aws_caller_identity.current
    values = { account_id = "123456789012" }
  }

  override_data {
    target = module.rise.data.aws_region.current
    values = { id = "eu-central-1", region = "eu-central-1" }
  }

  override_data {
    target = module.rise.data.aws_partition.current
    values = { partition = "aws" }
  }

  override_data {
    target = module.rise.data.aws_availability_zones.available
    values = { names = ["eu-central-1a", "eu-central-1b", "eu-central-1c"] }
  }
}
