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
  target = data.aws_availability_zones.available
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

# VPC endpoints reach AWS services only. Traefik's HTTP-01 challenge needs
# Let's Encrypt, so the certificate would silently never arrive.
run "rejects_acme_without_internet_egress" {
  command = plan

  variables {
    nat_gateway_mode     = "none"
    enable_vpc_endpoints = true
  }

  expect_failures = [aws_lb.this]
}

run "rejects_ecr_without_a_push_role" {
  command = plan

  variables {
    ecr_push_role_arn = null
  }

  expect_failures = [aws_lb.this]
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

run "rejects_an_install_with_no_identity_provider" {
  command = plan

  variables {
    oidc_issuer = null
    deploy_dex  = false
  }

  expect_failures = [aws_lb.this]
}

run "a_real_idp_install_must_supply_its_own_oidc_client_secret" {
  command = plan

  variables {
    deploy_dex         = false
    oidc_client_secret = null
  }

  # Without this the module would write the repo-published `rise-backend-secret`
  # constant as the client secret; that default is only for the bundled Dex demo.
  expect_failures = [aws_secretsmanager_secret_version.oidc_client_secret]
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
