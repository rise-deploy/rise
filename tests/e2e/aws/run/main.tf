# The per-run half of the ECS e2e environment, applied and destroyed by the
# harness around each suite.
#
# Deliberately NOT modules/rise-ecs. That module builds ADR-0004 D15's
# production topology -- NLB, private subnets, RDS, ACME -- and bending it to
# this shape would take a set of test-only knobs that would leave it with two
# personalities while still not exercising what operators run. The one contract
# that genuinely must not drift is shared instead, as
# modules/rise-ecs/modules/control-plane-env.
#
# Postgres and Rise are per-run rather than persistent so every run starts on a
# fresh database (which exercises bootstrap, as the Docker harness also relies
# on) and runs the image under test.

provider "aws" {
  region = var.region
}

data "terraform_remote_state" "bootstrap" {
  backend = "s3"

  config = {
    bucket = var.state_bucket
    key    = "bootstrap/terraform.tfstate"
    region = var.region
  }
}

locals {
  env = data.terraform_remote_state.bootstrap.outputs

  tags = {
    "rise.dev/managed-by" = "terraform"
    "rise.dev/purpose"    = "e2e"
    "rise.dev/scope"      = "per-run"
  }

  postgres_password = "rise123"
  database_url      = "postgres://rise:${local.postgres_password}@postgres.${local.env.cloud_map_namespace_name}:5432/rise"

  auth_backend_url = "http://rise.${local.env.cloud_map_namespace_name}:3000"
  traefik_api_url  = "http://traefik.${local.env.cloud_map_namespace_name}:8080"
}

module "control_plane_env" {
  source = "../../../../modules/rise-ecs/modules/control-plane-env"

  ingress_domain = var.ingress_domain
  # Plain HTTP. There is no load balancer to terminate at and no ACME, and the
  # harness drives the API with explicit headers rather than a browser, so
  # nothing here depends on transport security.
  ingress_scheme = "http"
  region         = var.region
  admin_email    = var.admin_email

  cluster_name = local.env.cluster_name
  subnet_ids   = local.env.subnet_ids
  # Deployed workloads share the environment's internal group; a test
  # environment does not need the per-role segmentation the production module
  # builds.
  security_group_ids = [local.env.internal_security_group_id]
  # No NAT here, so a task without a public IP cannot reach ECR and would fail
  # to start.
  assign_public_ip = true

  execution_role_arn     = local.env.execution_role_arn
  workload_task_role_arn = local.env.controller_role_arn
  log_group_name         = local.env.log_group_name

  auth_backend_url   = local.auth_backend_url
  traefik_api_url    = local.traefik_api_url
  traefik_entrypoint = "web"

  oidc_issuer    = local.env.dex_issuer
  oidc_client_id = "rise-backend"
  # The issuer is a Cloud Map address over http, so discovery is an in-VPC
  # plaintext fetch that the SSRF defaults would otherwise refuse.
  allow_private_ssrf = true

  cpu_architecture = var.cpu_architecture
  # Faster than production's 30s: a suite waiting on reconciliation is waiting
  # on this.
  reconcile_interval_secs = 10
  resource_prefix         = var.name
  ssm_parameter_prefix    = var.name

  registry = {
    type          = "ecr"
    account_id    = data.aws_caller_identity.current.account_id
    push_role_arn = local.env.ecr_push_role_arn
    repo_prefix   = local.env.ecr_repo_prefix
    # Project deletion at teardown then removes the repository, which is both
    # the cleanup path and a free test of it.
    auto_remove = true
  }
}

data "aws_caller_identity" "current" {}
