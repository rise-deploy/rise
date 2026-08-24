provider "aws" {
  region = var.region
}

data "aws_caller_identity" "current" {}
data "aws_partition" "current" {}

data "aws_availability_zones" "available" {
  state = "available"

  filter {
    name   = "opt-in-status"
    values = ["opt-in-not-required"]
  }
}

locals {
  account_id = data.aws_caller_identity.current.account_id
  partition  = data.aws_partition.current.partition
  azs        = slice(data.aws_availability_zones.available.names, 0, var.availability_zone_count)

  tags = merge({
    "rise.dev/managed-by" = "terraform"
    "rise.dev/purpose"    = "e2e"
  }, var.tags)

  namespace_name = "${var.name}.internal"

  # The one contract with the per-run apply. It reads these from remote state.
  state_bucket = "${var.name}-tfstate-${local.account_id}"
}

# -----------------------------------------------------------------------------
# IAM for Rise itself, from the reusable module
# -----------------------------------------------------------------------------

module "rise_aws" {
  source = "../../../modules/rise-aws"

  name       = var.name
  enable_ecr = true
  enable_ecs = true
  enable_kms = false

  ecs_cluster_name     = var.name
  ssm_parameter_prefix = var.name
  # rise-aws derives its ECR repo prefix from `name` ("<name>/"), so the
  # per-run apply must pass the same prefix to Rise.

  # A name wildcard, not explicit ARNs. Secrets Manager appends a random suffix,
  # so a per-run secret would never match a list fixed at bootstrap time -- and
  # updating that list every run is exactly the IAM write the harness must not
  # make.
  ecs_secret_arns = ["arn:${local.partition}:secretsmanager:${var.region}:${local.account_id}:secret:${var.name}/*"]

  tags = local.tags
}

# -----------------------------------------------------------------------------
# Traefik's task role
#
# Pre-created here rather than by modules/rise-ecs, because the per-run identity
# cannot write IAM. It grants cluster reads and nothing else -- deliberately not
# the Rise controller role, since Traefik's API is unauthenticated.
# -----------------------------------------------------------------------------

data "aws_iam_policy_document" "traefik_assume" {
  statement {
    effect = "Allow"
    principals {
      type        = "Service"
      identifiers = ["ecs-tasks.amazonaws.com"]
    }
    actions = ["sts:AssumeRole"]
    condition {
      test     = "StringEquals"
      variable = "aws:SourceAccount"
      values   = [local.account_id]
    }
  }
}

resource "aws_iam_role" "traefik" {
  name               = "${var.name}-traefik"
  description        = "Traefik ECS provider discovery for the Rise e2e environment"
  assume_role_policy = data.aws_iam_policy_document.traefik_assume.json
  tags               = local.tags
}

resource "aws_iam_role_policy" "traefik" {
  name = "discovery"
  role = aws_iam_role.traefik.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = [
        "ecs:ListClusters",
        "ecs:DescribeClusters",
        "ecs:ListTasks",
        "ecs:DescribeTasks",
        "ecs:DescribeContainerInstances",
        "ecs:DescribeTaskDefinition",
      ]
      Resource = "*"
    }]
  })
}

# -----------------------------------------------------------------------------
# Terraform state for the per-run apply
# -----------------------------------------------------------------------------

resource "aws_s3_bucket" "state" {
  bucket = local.state_bucket
  tags   = local.tags

  lifecycle {
    # This bucket holds this workspace's own state. Without the guard a
    # `terraform destroy` would delete the state file it is reading from,
    # halfway through, leaving every other resource orphaned and unmanageable.
    # Emptying and removing it is a deliberate manual act -- see the README.
    prevent_destroy = true
  }
}

resource "aws_s3_bucket_versioning" "state" {
  bucket = aws_s3_bucket.state.id

  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "state" {
  bucket = aws_s3_bucket.state.id

  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_public_access_block" "state" {
  bucket                  = aws_s3_bucket.state.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

# -----------------------------------------------------------------------------
# DNS
#
# Fargate cannot hold an Elastic IP, and Traefik is per-run, so its address is
# new every run. The zone makes the *name* stable instead: each run UPSERTs its
# own scope, `<scope>.<zone>` and `*.<scope>.<zone>`, at the Traefik it just
# created, and deletes both at teardown. Scoping is what lets runs overlap --
# one shared apex would have them fighting over the same record.
#
# The wildcard is required: projects are served at `<project>.<scope>.<zone>`.
# One wildcard label is enough because groups and environments join the project
# with a dash rather than a dot.
# -----------------------------------------------------------------------------

resource "aws_route53_zone" "this" {
  name    = var.dns_zone_name
  comment = "Rise ECS e2e environment"
  tags    = local.tags
}
