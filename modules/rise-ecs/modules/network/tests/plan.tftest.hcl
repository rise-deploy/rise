provider "aws" {
  region                      = "eu-central-1"
  access_key                  = "AKIAIOSFODNN7EXAMPLE"
  secret_key                  = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
  skip_credentials_validation = true
  skip_requesting_account_id  = true
  skip_metadata_api_check     = true
  skip_region_validation      = true
}
override_data {
  target = data.aws_availability_zones.available
  values = { names = ["eu-central-1a", "eu-central-1b"] }
}

variables {
  name                       = "rise"
  tags                       = {}
  vpc                        = null
  region                     = "eu-central-1"
  topology                   = { cidr = "10.42.0.0/16", availability_zone_count = 2, nat_gateway_mode = "single", enable_vpc_endpoints = false }
  endpoint_security_group_id = null
}

run "database_subnets_clear_the_private_range_at_four_azs" {
  command = plan

  override_data {
    target = data.aws_availability_zones.available
    values = { names = ["eu-central-1a", "eu-central-1b", "eu-central-1c", "eu-central-1d"] }
  }

  variables {
    topology = { cidr = "10.42.0.0/16", availability_zone_count = 4, nat_gateway_mode = "single", enable_vpc_endpoints = false }
  }

  # The fourth private /20 determines the end of the private address range.
  assert {
    condition     = aws_subnet.private["eu-central-1d"].cidr_block == "10.42.64.0/20"
    error_message = "the fourth private subnet moved; re-check the database offset"
  }
  # Database /24s at netnum 128+, well above the private range's /24-netnum 79.
  assert {
    condition     = aws_subnet.database["eu-central-1a"].cidr_block == "10.42.128.0/24"
    error_message = "database subnet must start clear of the private /20 range"
  }
  assert {
    condition     = aws_subnet.database["eu-central-1d"].cidr_block == "10.42.131.0/24"
    error_message = "database subnet must start clear of the private /20 range"
  }
}