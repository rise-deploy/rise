# Plans the per-run stack against a mocked bootstrap state.

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
  target = data.terraform_remote_state.bootstrap
  values = {
    outputs = {
      region                     = "eu-central-1"
      cluster_name               = "rise-e2e"
      cluster_arn                = "arn:aws:ecs:eu-central-1:123456789012:cluster/rise-e2e"
      subnet_ids                 = ["subnet-aaa", "subnet-bbb"]
      internal_security_group_id = "sg-internal"
      edge_security_group_id     = "sg-edge"
      cloud_map_namespace_id     = "ns-abc"
      cloud_map_namespace_name   = "rise-e2e.internal"
      log_group_name             = "/rise-e2e"
      traefik_service_name       = "rise-e2e-traefik"
      dex_issuer                 = "http://dex.rise-e2e.internal:5556/dex"
      controller_role_arn        = "arn:aws:iam::123456789012:role/rise-e2e"
      execution_role_arn         = "arn:aws:iam::123456789012:role/rise-e2e-ecs-execution"
      ecr_push_role_arn          = "arn:aws:iam::123456789012:role/rise-e2e-ecr-push"
      ecr_repo_prefix            = "rise-e2e/"
      dns_zone_id                = "Z123"
      dns_zone_name              = "e2e.example.com"
      state_bucket               = "rise-e2e-tfstate-123456789012"
      ci_role_arn                = "arn:aws:iam::123456789012:role/rise-e2e-ci"
    }
  }
}

variables {
  name               = "rise-e2e"
  state_bucket       = "rise-e2e-tfstate-123456789012"
  rise_image_tag     = "0.23.0"
  ingress_domain     = "e2e.example.com"
  jwt_signing_secret = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE9"
  encryption_key     = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE9"
}

run "per_run_stack_plans" {
  command = plan

  # Plain HTTP: no load balancer to terminate at, no ACME. A Secure cookie is
  # never sent over HTTP, so this must follow the scheme.
  assert {
    condition     = module.control_plane_env.environment["RISE_COOKIE_SECURE"] == "false"
    error_message = "cookie security must follow the scheme, or login breaks over http"
  }

  assert {
    condition     = module.control_plane_env.public_url == "http://rise.e2e.example.com"
    error_message = "public_url must match what the CI bearer's iss will be"
  }

  # The issuer is an in-VPC plaintext address, which the SSRF defaults refuse.
  assert {
    condition     = module.control_plane_env.environment["RISE_SSRF_ALLOW_HTTP"] == "true"
    error_message = "Rise would refuse OIDC discovery against the Cloud Map issuer"
  }

  assert {
    condition     = module.control_plane_env.environment["DEX_ISSUER"] == "http://dex.rise-e2e.internal:5556/dex"
    error_message = "the issuer must be the Cloud Map address the password grant is served from"
  }

  # No NAT in this topology, so a task without a public IP cannot reach ECR.
  assert {
    condition     = module.control_plane_env.environment["RISE_ECS_ASSIGN_PUBLIC_IP"] == "true"
    error_message = "workloads need public IPs here; there is no NAT for them to egress through"
  }

  # Two installs sharing a cluster with the same controller class delete each
  # other's services.
  assert {
    condition     = module.control_plane_env.environment["RISE_CONTROLLER_CLASS_NAME"] != ""
    error_message = "controller class must be set"
  }

  assert {
    condition     = module.control_plane_env.environment["RISE_ECR_ACCOUNT_ID"] == "123456789012"
    error_message = "ECR account must be the caller's own; the backend asserts it at startup"
  }
}
