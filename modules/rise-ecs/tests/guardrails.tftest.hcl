# Each of these configurations is one the backend rejects at startup, or one
# that produces an install that cannot work. They must fail in `terraform plan`
# instead.

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
  target = data.aws_caller_identity.current
  values = { account_id = "123456789012" }
}

override_data {
  target = data.aws_region.current
  values = { id = "eu-central-1", region = "eu-central-1" }
}

override_data {
  target = data.aws_partition.current
  values = { partition = "aws" }
}

override_data {
  target = module.network.data.aws_availability_zones.available
  values = { names = ["eu-central-1a", "eu-central-1b", "eu-central-1c"] }
}

variables {
  name                = "rise"
  ingress_domain      = "rise.example.com"
  admin_email         = "ops@example.com"
  rise_image_tag      = "0.23.0"
  acme_email          = "ops@example.com"
  controller_role_arn = "arn:aws:iam::123456789012:role/rise"
  execution_role_arn  = "arn:aws:iam::123456789012:role/rise-ecs-execution"
  ecr_push_role_arn   = "arn:aws:iam::123456789012:role/rise-ecr-push"
  oidc_issuer         = "https://id.example.com"
  oidc_client_secret  = "s3cret"
}

# ECS re-authenticates at every task start and cannot refresh a scoped token, so
# the backend refuses these two outright.
run "rejects_gitlab_and_jfrog_registries" {
  command = plan

  variables {
    registry_type = "gitlab"
  }

  expect_failures = [var.registry_type]
}

run "rejects_an_image_with_both_tag_and_digest" {
  command = plan
  variables {
    rise_image_ref = "ghcr.io/rise-deploy/rise@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  }
  expect_failures = [var.rise_image_ref]
}

run "rejects_an_image_without_tag_or_digest" {
  command = plan
  variables {
    rise_image_tag = null
  }
  expect_failures = [var.rise_image_ref]
}

# The backend normalises common spellings, but the module should not be the
# thing that hands it something to normalise.
run "rejects_a_non_fargate_cpu_architecture" {
  command = plan

  variables {
    cpu_architecture = "riscv64"
  }

  expect_failures = [var.cpu_architecture]
}

# "rise" would produce repositories named "risemyapp": the prefix is
# concatenated onto the project name literally.
run "rejects_a_repo_prefix_without_a_trailing_slash" {
  command = plan

  variables {
    ecr_repo_prefix = "rise"
  }

  expect_failures = [var.ecr_repo_prefix]
}

run "rejects_noncanonical_idp_group_sync_prefixes" {
  command = plan

  variables {
    idp_group_sync_prefixes = ["Rise_"]
  }

  expect_failures = [var.idp_group_sync_prefixes]
}

run "rejects_an_additional_secret_that_overrides_a_builtin" {
  command = plan

  variables {
    control_plane_secret_environment = {
      DATABASE_URL = "arn:aws:secretsmanager:eu-central-1:123456789012:secret:rise/database-url-abc123"
    }
  }

  expect_failures = [var.control_plane_secret_environment]
}

run "rejects_an_additional_secret_without_a_full_arn" {
  command = plan

  variables {
    control_plane_secret_environment = {
      SNOWFLAKE_OAUTH_PRIVATE_KEY = "rise/snowflake-oauth"
    }
  }

  expect_failures = [var.control_plane_secret_environment]
}

# VPC endpoints reach AWS services only. Traefik's HTTP-01 challenge needs
# Let's Encrypt, so the certificate would silently never arrive.
run "rejects_acme_without_internet_egress" {
  command = plan

  variables {
    nat_gateway_mode     = "none"
    enable_vpc_endpoints = true
  }

  expect_failures = [var.nat_gateway_mode]
}

run "rejects_ecr_without_a_push_role" {
  command = plan

  variables {
    ecr_push_role_arn = null
  }

  expect_failures = [var.ecr_push_role_arn]
}

run "rejects_external_traefik_mode_without_an_arn" {
  command = plan

  variables {
    create_traefik_task_role = false
    traefik_task_role_arn    = null
  }

  expect_failures = [var.create_traefik_task_role]
}

run "rejects_module_owned_traefik_mode_with_an_external_arn" {
  command = plan

  variables {
    create_traefik_task_role = true
    traefik_task_role_arn    = "arn:aws:iam::123456789012:role/rise-traefik"
  }

  expect_failures = [var.create_traefik_task_role]
}

run "rejects_an_ecr_registry_host_with_a_scheme" {
  command = plan

  variables {
    ecr_registry_host = "https://123456789012.dkr.ecr.eu-central-1.amazonaws.com"
  }

  expect_failures = [var.ecr_registry_host]
}

run "rejects_an_install_with_no_identity_provider" {
  command = plan

  variables {
    oidc_issuer = null
    deploy_dex  = false
  }

  expect_failures = [var.oidc_issuer]
}

run "a_real_idp_install_must_supply_its_own_oidc_client_secret" {
  command = plan

  variables {
    deploy_dex         = false
    oidc_client_secret = null
  }

  # Without this the module would write the repo-published `rise-backend-secret`
  # constant as the client secret; that default is only for the bundled Dex demo.
  expect_failures = [var.oidc_client_secret]
}

run "deploy_dex_uses_a_browser_reachable_issuer" {
  command = plan

  variables {
    deploy_dex                = true
    oidc_issuer               = null
    dex_admin_password_bcrypt = "$2y$10$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  }

  # A browser performs the authorization-code redirect, so the issuer cannot be
  # the Cloud Map name the e2e harness uses -- that resolves only inside the VPC.
  assert {
    condition     = local.rise_environment["DEX_ISSUER"] == "https://dex.rise.example.com/dex"
    error_message = "the demo issuer must be publicly reachable, not the Cloud Map address"
  }
}

run "rejects_an_alb_without_a_certificate" {
  command = plan
  variables { edge_mode = "alb-acm" }
  expect_failures = [var.edge_mode]
}

run "rejects_acme_without_an_email" {
  command = plan
  variables { acme_email = null }
  expect_failures = [var.acme_email]
}

run "rejects_a_vpc_without_public_subnets" {
  command = plan
  variables {
    vpc = { id = "vpc-0123456789abcdef0", private_subnet_ids = ["subnet-a"] }
  }
  override_data {
    target = module.network.data.aws_subnet.brought["subnet-a"]
    values = { vpc_id = "vpc-0123456789abcdef0" }
  }
  expect_failures = [var.vpc]
}
