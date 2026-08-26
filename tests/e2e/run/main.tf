# The per-run half of the ECS e2e environment, applied and destroyed by the
# harness around each suite.
#
# Deliberately NOT modules/rise-ecs. That module builds ADR-0005 D15's
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
    "rise.dev/scope"      = var.scope
  }

  # Everything this run answers for lives under its own label, so concurrent
  # runs never contend for a name.
  domain = "${var.scope}.${var.dns_zone_name}"

  # The controller class is what keeps runs from destroying each other: the
  # orphan collector only considers services carrying its own class, and
  # Traefik is constrained to the same value below.
  controller_class = var.scope

  # The bootstrap's prefix is shared by every run; Rise's repositories go under
  # this run's own segment of it so the bring-up sweep can enumerate its own
  # without reaching a concurrent run's. The trailing slash is what makes it a
  # prefix rather than a repository name.
  scoped_ecr_repo_prefix = "${local.env.ecr_repo_prefix}${var.scope}/"

  # Cloud Map is shared, so per-run services need distinct names within it.
  postgres_host = "postgres-${var.scope}.${local.env.cloud_map_namespace_name}"
  rise_host     = "rise-${var.scope}.${local.env.cloud_map_namespace_name}"
  traefik_host  = "traefik-${var.scope}.${local.env.cloud_map_namespace_name}"
  dex_host      = "dex-${var.scope}.${local.env.cloud_map_namespace_name}"

  postgres_password = "rise123"
  database_url      = "postgres://rise:${local.postgres_password}@${local.postgres_host}:5432/rise"

  auth_backend_url = "http://${local.rise_host}:3000"
  traefik_api_url  = "http://${local.traefik_host}:8080"

  # In-VPC and never publicly resolvable, which is fine: Rise fetches discovery
  # and JWKS over private DNS, and the harness uses the password grant, so no
  # browser redirect is involved.
  dex_issuer = "http://${local.dex_host}:5556/dex"

  dex_config = replace(
    file("${path.module}/../../../dev/dex/config.yaml"),
    "/(?m)^issuer:.*$/",
    "issuer: ${local.dex_issuer}"
  )
}

module "control_plane_env" {
  source = "../../../modules/rise-ecs/modules/control-plane-env"

  ingress_domain = local.domain
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
  security_group_ids = [aws_security_group.internal.id]
  # No NAT here, so a task without a public IP cannot reach ECR and would fail
  # to start.
  assign_public_ip = true

  execution_role_arn     = local.env.execution_role_arn
  workload_task_role_arn = local.env.controller_role_arn
  log_group_name         = local.env.log_group_name

  auth_backend_url   = local.auth_backend_url
  traefik_api_url    = local.traefik_api_url
  traefik_entrypoint = "web"

  oidc_issuer    = local.dex_issuer
  oidc_client_id = "rise-backend"
  # The issuer is a Cloud Map address over http, so discovery is an in-VPC
  # plaintext fetch that the SSRF defaults would otherwise refuse.
  allow_private_ssrf = true

  cpu_architecture = var.cpu_architecture
  # Faster than production's 30s: a suite waiting on reconciliation is waiting
  # on this.
  reconcile_interval_secs = 10
  # Everything Rise itself creates is scoped too, not just the stack around it.
  # ECS service names are cluster-unique, and the ECR repositories and SSM
  # parameters are what the next run's bring-up sweep enumerates by prefix -- an
  # unscoped prefix makes that sweep reach into a concurrent run's live images
  # and secrets.
  resource_prefix      = "${var.name}-${var.scope}"
  ssm_parameter_prefix = "${var.name}/${var.scope}"

  # See `local.controller_class`: this is what isolates one run from another.
  controller_class_name = local.controller_class

  registry = {
    type          = "ecr"
    account_id    = data.aws_caller_identity.current.account_id
    push_role_arn = local.env.ecr_push_role_arn
    repo_prefix   = local.scoped_ecr_repo_prefix
    # Project deletion at teardown then removes the repository, which is both
    # the cleanup path and a free test of it.
    auto_remove = true
  }
}

data "aws_caller_identity" "current" {}
