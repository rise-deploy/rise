# The documented production install -- modules/rise-aws for IAM, then
# modules/rise-ecs for everything else -- applied and destroyed against Floci,
# a local AWS emulator, by the `aws-install` suite. See ../README.md ("AWS
# Install Suite") for what this proves and what it cannot.

provider "aws" {
  region = var.region

  # Floci accepts any credentials; these only have to be present.
  access_key = "test"
  secret_key = "test"

  skip_credentials_validation = true
  skip_metadata_api_check     = true
  skip_region_validation      = true

  # Virtual-hosted buckets would need `<bucket>.localhost` to resolve.
  s3_use_path_style = true

  endpoints {
    acm              = var.endpoint
    cloudwatchlogs   = var.endpoint
    ec2              = var.endpoint
    ecr              = var.endpoint
    ecs              = var.endpoint
    efs              = var.endpoint
    elbv2            = var.endpoint
    iam              = var.endpoint
    kms              = var.endpoint
    rds              = var.endpoint
    route53          = var.endpoint
    s3               = var.endpoint
    secretsmanager   = var.endpoint
    servicediscovery = var.endpoint
    ssm              = var.endpoint
    sts              = var.endpoint
  }
}

locals {
  tags = {
    "rise.dev/managed-by" = "terraform"
    "rise.dev/purpose"    = "e2e-floci"
  }

  # Both modules derive policy ARNs and resource names from these, so they
  # must agree -- which is the contract this workspace exists to exercise.
  cluster_name  = var.name
  log_group     = "/${var.name}"
  ssm_prefix    = var.name
  ecr_prefix    = "${var.name}/"
  secret_prefix = "arn:aws:secretsmanager:${var.region}:${data.aws_caller_identity.current.account_id}:secret:${var.name}/*"
}

data "aws_caller_identity" "current" {}

module "rise_aws" {
  source = "../../../modules/rise-aws"

  name       = var.name
  enable_ecr = true
  enable_ecs = true
  enable_kms = true
  enable_s3  = true

  ecs_cluster_name     = local.cluster_name
  ecs_log_group_name   = local.log_group
  ssm_parameter_prefix = local.ssm_prefix
  ecs_secret_arns      = [local.secret_prefix]

  tags = local.tags
}

resource "aws_route53_zone" "this" {
  name          = var.ingress_domain
  force_destroy = true
  tags          = local.tags
}

module "rise_ecs" {
  source = "../../../modules/rise-ecs"

  name           = var.name
  ingress_domain = var.ingress_domain
  log_group_name = local.log_group
  rise_image_tag = var.rise_image_tag
  admin_email    = "admin@example.com"
  acme_email     = "admin@example.com"

  controller_role_arn      = module.rise_aws.role_arn
  execution_role_arn       = module.rise_aws.ecs_execution_role_arn
  workload_task_role_arn   = module.rise_aws.ecs_task_role_arn
  traefik_task_role_arn    = module.rise_aws.ecs_traefik_role_arn
  create_traefik_task_role = false
  ecr_push_role_arn        = module.rise_aws.push_role_arn
  ecr_repo_prefix          = local.ecr_prefix
  ssm_parameter_prefix     = local.ssm_prefix

  # Created in this same apply, so its ID is unknown at plan time; the flag
  # keeps the record count known.
  route53_zone_id    = aws_route53_zone.this.zone_id
  create_dns_records = true

  # Self-contained identity, so the install needs nothing outside Floci.
  deploy_dex                = true
  dex_admin_password_bcrypt = "$2a$10$2b2cU8CPhOTaGrs1HRQuAueS7JTT5ZHsHSzYiFPm1leZck7Mc8T4W" # "password"

  # A throwaway install: both of these would otherwise make `destroy` fail or
  # leave names that collide with the next apply.
  deletion_protection         = false
  secret_recovery_window_days = 0

  tags = local.tags
}
