locals {
  name        = var.name
  repo_prefix = "${var.name}/"

  # KMS key alias name - defaults to just the name, but can be overridden for backwards compatibility
  kms_key_alias = var.kms_key_alias != null ? var.kms_key_alias : var.name

  default_tags = {
    "rise:managed-by" = "terraform"
    "rise:component"  = "backend"
  }

  tags = merge(local.default_tags, var.tags)

  # Default lifecycle policy to limit image count
  lifecycle_policy = jsonencode({
    rules = [{
      rulePriority = 1
      description  = "Keep only the last ${var.max_image_count} images"
      selection = {
        tagStatus   = "any"
        countType   = "imageCountMoreThan"
        countNumber = var.max_image_count
      }
      action = {
        type = "expire"
      }
    }]
  })
}

data "aws_caller_identity" "current" {}
data "aws_region" "current" {}

locals {
  region           = data.aws_region.current.id
  account_id       = data.aws_caller_identity.current.account_id
  s3_bucket_prefix = var.s3_bucket_prefix != null ? var.s3_bucket_prefix : var.name
}

# -----------------------------------------------------------------------------
# KMS Key (for encryption)
# -----------------------------------------------------------------------------

resource "aws_kms_key" "ecr" {
  count = var.enable_ecr && var.enable_kms ? 1 : 0

  description             = "KMS key for Rise encryption"
  deletion_window_in_days = 30
  enable_key_rotation     = true
  tags                    = local.tags
}

resource "aws_kms_alias" "ecr" {
  count = var.enable_ecr && var.enable_kms ? 1 : 0

  name          = "alias/${local.kms_key_alias}"
  target_key_id = aws_kms_key.ecr[0].key_id
}

# -----------------------------------------------------------------------------
# IAM Policy for ECR Controller
# -----------------------------------------------------------------------------

data "aws_iam_policy_document" "backend" {
  # ECR permissions (if enabled)
  dynamic "statement" {
    for_each = var.enable_ecr ? [1] : []
    content {
      sid    = "GetAuthorizationToken"
      effect = "Allow"
      actions = [
        "ecr:GetAuthorizationToken"
      ]
      resources = ["*"]
    }
  }

  dynamic "statement" {
    for_each = var.enable_ecr ? [1] : []
    content {
      sid    = "DescribeRepositories"
      effect = "Allow"
      actions = [
        "ecr:DescribeRepositories",
        "ecr:ListTagsForResource"
      ]
      resources = ["*"]
    }
  }

  dynamic "statement" {
    for_each = var.enable_ecr ? [1] : []
    content {
      sid    = "CreateRepository"
      effect = "Allow"
      actions = [
        "ecr:CreateRepository",
        "ecr:TagResource",
        "ecr:PutImageScanningConfiguration",
        "ecr:PutImageTagMutability",
        "ecr:PutLifecyclePolicy"
      ]
      resources = [
        "arn:aws:ecr:${local.region}:${local.account_id}:repository/${local.repo_prefix}*"
      ]
    }
  }

  dynamic "statement" {
    for_each = var.enable_ecr ? [1] : []
    content {
      sid    = "DeleteRepository"
      effect = "Allow"
      actions = [
        "ecr:DeleteRepository",
        "ecr:BatchDeleteImage"
      ]
      resources = [
        "arn:aws:ecr:${local.region}:${local.account_id}:repository/${local.repo_prefix}*"
      ]
    }
  }

  dynamic "statement" {
    for_each = var.enable_ecr ? [1] : []
    content {
      sid    = "TagRepository"
      effect = "Allow"
      actions = [
        "ecr:TagResource",
        "ecr:UntagResource"
      ]
      resources = [
        "arn:aws:ecr:${local.region}:${local.account_id}:repository/${local.repo_prefix}*"
      ]
    }
  }

  # KMS permissions if using KMS encryption
  dynamic "statement" {
    for_each = var.enable_ecr && var.enable_kms ? [1] : []
    content {
      sid    = "KMSEncryption"
      effect = "Allow"
      actions = [
        "kms:Encrypt",
        "kms:Decrypt",
        "kms:GenerateDataKey*",
        "kms:DescribeKey"
      ]
      resources = [aws_kms_key.ecr[0].arn]
    }
  }

  # S3 bucket lifecycle management (if enabled)
  dynamic "statement" {
    for_each = var.enable_s3 ? [1] : []
    content {
      sid    = "ManageS3Buckets"
      effect = "Allow"
      actions = [
        "s3:CreateBucket",
        "s3:DeleteBucket",
        "s3:GetBucketLocation",
        "s3:ListBucket",
        "s3:ListBucketVersions",
        "s3:PutBucketTagging",
        "s3:GetBucketTagging",
        "s3:PutBucketPublicAccessBlock",
        "s3:GetBucketPublicAccessBlock",
        "s3:DeleteObject",
        "s3:DeleteObjectVersion",
      ]
      resources = [
        "arn:aws:s3:::${local.s3_bucket_prefix}-*",
        "arn:aws:s3:::${local.s3_bucket_prefix}-*/*",
      ]
    }
  }

  # IAM user creation for S3 — requires the permissions boundary to be attached
  dynamic "statement" {
    for_each = var.enable_s3 ? [1] : []
    content {
      sid    = "CreateS3IamUsers"
      effect = "Allow"
      actions = [
        "iam:CreateUser",
      ]
      resources = ["arn:aws:iam::${local.account_id}:user/${local.s3_bucket_prefix}-s3-*"]
      condition {
        test     = "StringEquals"
        variable = "iam:PermissionsBoundary"
        values   = [aws_iam_policy.s3_user_boundary[0].arn]
      }
    }
  }

  # IAM user management for S3 (get, tag, delete — no boundary condition needed)
  dynamic "statement" {
    for_each = var.enable_s3 ? [1] : []
    content {
      sid    = "ManageS3IamUsers"
      effect = "Allow"
      actions = [
        "iam:GetUser",
        "iam:DeleteUser",
        "iam:TagUser",
        "iam:UntagUser",
      ]
      resources = ["arn:aws:iam::${local.account_id}:user/${local.s3_bucket_prefix}-s3-*"]
    }
  }

  # IAM access key and inline policy management (scoped to Rise S3 users only)
  dynamic "statement" {
    for_each = var.enable_s3 ? [1] : []
    content {
      sid    = "ManageS3IamUserCredentials"
      effect = "Allow"
      actions = [
        "iam:CreateAccessKey",
        "iam:DeleteAccessKey",
        "iam:ListAccessKeys",
        "iam:PutUserPolicy",
        "iam:DeleteUserPolicy",
        "iam:GetUserPolicy",
      ]
      resources = ["arn:aws:iam::${local.account_id}:user/${local.s3_bucket_prefix}-s3-*"]
    }
  }

  # Deny removing the permissions boundary (prevents privilege escalation)
  dynamic "statement" {
    for_each = var.enable_s3 ? [1] : []
    content {
      sid       = "DenyRemoveS3UserBoundary"
      effect    = "Deny"
      actions   = ["iam:DeleteUserPermissionsBoundary"]
      resources = ["arn:aws:iam::${local.account_id}:user/${local.s3_bucket_prefix}-s3-*"]
    }
  }

  # RDS permissions for managing database instances (if enabled)
  dynamic "statement" {
    for_each = var.enable_rds ? [1] : []
    content {
      sid    = "ManageRDSInstances"
      effect = "Allow"
      actions = [
        "rds:CreateDBInstance",
        "rds:DeleteDBInstance",
        "rds:DescribeDBInstances",
        "rds:ModifyDBInstance",
        "rds:ListTagsForResource",
        "rds:AddTagsToResource",
        "rds:RemoveTagsFromResource"
      ]
      resources = [
        "arn:aws:rds:${local.region}:${local.account_id}:db:${var.name}-*",
        "arn:aws:rds:${local.region}:${local.account_id}:subgrp:${var.name}-*"
      ]
    }
  }

  # RDS subnet groups (needed for VPC placement)
  dynamic "statement" {
    for_each = var.enable_rds ? [1] : []
    content {
      sid    = "ManageRDSSubnetGroups"
      effect = "Allow"
      actions = [
        "rds:CreateDBSubnetGroup",
        "rds:DeleteDBSubnetGroup",
        "rds:DescribeDBSubnetGroups"
      ]
      resources = [
        "arn:aws:rds:${local.region}:${local.account_id}:subgrp:${var.name}-*"
      ]
    }
  }
}

resource "aws_iam_policy" "backend" {
  name        = local.name
  description = "IAM policy for Rise backend to manage ECR repositories, RDS instances, and S3 buckets"
  policy      = data.aws_iam_policy_document.backend.json
  tags        = local.tags
}

# -----------------------------------------------------------------------------
# IAM Role (for AWS deployments)
# -----------------------------------------------------------------------------

# Default assume role policy - allows account to assume
data "aws_iam_policy_document" "assume_role_default" {
  statement {
    effect = "Allow"
    principals {
      type        = "AWS"
      identifiers = ["arn:aws:iam::${local.account_id}:root"]
    }
    actions = ["sts:AssumeRole"]
  }

  # Service principals, for runtimes that hand the role to a workload directly
  # rather than through a federated identity. ECS is the case that needs it: the
  # control plane takes its AWS credentials from the task role named on its own
  # task definition, so without ecs-tasks.amazonaws.com here the role cannot be
  # used as a taskRoleArn at all.
  dynamic "statement" {
    for_each = length(var.assume_role_services) > 0 ? [1] : []
    content {
      effect = "Allow"
      principals {
        type        = "Service"
        identifiers = var.assume_role_services
      }
      actions = ["sts:AssumeRole"]
      # Confused-deputy guard: the service may assume this role only on behalf
      # of this account, never for a resource someone else owns.
      condition {
        test     = "StringEquals"
        variable = "aws:SourceAccount"
        values   = [local.account_id]
      }
    }
  }
}

# IRSA assume role policy - for EKS service accounts
data "aws_iam_policy_document" "assume_role_irsa" {
  count = var.irsa_oidc_provider_arn != null ? 1 : 0

  statement {
    effect = "Allow"
    principals {
      type        = "Federated"
      identifiers = [var.irsa_oidc_provider_arn]
    }
    actions = ["sts:AssumeRoleWithWebIdentity"]
    condition {
      test     = "StringEquals"
      variable = "${replace(var.irsa_oidc_provider_arn, "/^arn:aws:iam::[0-9]+:oidc-provider\\//", "")}:sub"
      values   = ["system:serviceaccount:${var.irsa_namespace}:${var.irsa_service_account}"]
    }
    condition {
      test     = "StringEquals"
      variable = "${replace(var.irsa_oidc_provider_arn, "/^arn:aws:iam::[0-9]+:oidc-provider\\//", "")}:aud"
      values   = ["sts.amazonaws.com"]
    }
  }
}

locals {
  # Use IRSA policy if OIDC provider is configured, otherwise use default
  assume_role_policy = var.irsa_oidc_provider_arn != null ? data.aws_iam_policy_document.assume_role_irsa[0].json : data.aws_iam_policy_document.assume_role_default.json
}

resource "aws_iam_role" "backend" {
  name               = local.name
  description        = "IAM role for Rise backend"
  assume_role_policy = local.assume_role_policy
  tags               = local.tags
}

resource "aws_iam_role_policy_attachment" "backend" {
  role       = aws_iam_role.backend.name
  policy_arn = aws_iam_policy.backend.arn
}

# -----------------------------------------------------------------------------
# IAM User (for non-AWS deployments)
# -----------------------------------------------------------------------------

resource "aws_iam_user" "backend" {
  count = var.create_iam_user ? 1 : 0

  name = local.name
  tags = local.tags
}

resource "aws_iam_user_policy_attachment" "backend" {
  count = var.create_iam_user ? 1 : 0

  user       = aws_iam_user.backend[0].name
  policy_arn = aws_iam_policy.backend.arn
}

resource "aws_iam_access_key" "backend" {
  count = var.create_iam_user ? 1 : 0

  user = aws_iam_user.backend[0].name
}

# -----------------------------------------------------------------------------
# Push Role - for scoped image push credentials (if ECR enabled)
# -----------------------------------------------------------------------------
# This role is assumed by the Rise backend to generate temporary credentials
# for clients to push images. The credentials are scoped per-repository using
# an inline session policy during AssumeRole.

data "aws_iam_policy_document" "push_role" {
  count = var.enable_ecr ? 1 : 0

  # Allow getting authorization tokens (required for docker login)
  statement {
    sid    = "GetAuthorizationToken"
    effect = "Allow"
    actions = [
      "ecr:GetAuthorizationToken"
    ]
    resources = ["*"]
  }

  # Allow all image push operations on repositories with our prefix
  # The actual scoping to specific repos happens via session policy during AssumeRole
  statement {
    sid    = "PushImages"
    effect = "Allow"
    actions = [
      "ecr:BatchCheckLayerAvailability",
      "ecr:InitiateLayerUpload",
      "ecr:UploadLayerPart",
      "ecr:CompleteLayerUpload",
      "ecr:PutImage",
      "ecr:BatchGetImage",
      "ecr:GetDownloadUrlForLayer"
    ]
    resources = [
      "arn:aws:ecr:${local.region}:${local.account_id}:repository/${local.repo_prefix}*"
    ]
  }
}

resource "aws_iam_policy" "push_role" {
  count = var.enable_ecr ? 1 : 0

  name        = "${var.name}-ecr-push"
  description = "IAM policy for Rise ECR push operations"
  policy      = data.aws_iam_policy_document.push_role[0].json
  tags        = local.tags
}

# The push role can be assumed by the controller role or user
data "aws_iam_policy_document" "push_role_assume" {
  count = var.enable_ecr ? 1 : 0

  # Allow the controller role to assume the push role
  statement {
    effect = "Allow"
    principals {
      type        = "AWS"
      identifiers = [aws_iam_role.backend.arn]
    }
    actions = ["sts:AssumeRole"]
  }

  # Allow the controller user to assume the push role
  dynamic "statement" {
    for_each = var.create_iam_user ? [1] : []
    content {
      effect = "Allow"
      principals {
        type        = "AWS"
        identifiers = [aws_iam_user.backend[0].arn]
      }
      actions = ["sts:AssumeRole"]
    }
  }
}

resource "aws_iam_role" "push_role" {
  count = var.enable_ecr ? 1 : 0

  name               = "${var.name}-ecr-push"
  description        = "IAM role for Rise ECR push operations (assumed to generate scoped credentials)"
  assume_role_policy = data.aws_iam_policy_document.push_role_assume[0].json
  tags               = local.tags
}

resource "aws_iam_role_policy_attachment" "push_role" {
  count = var.enable_ecr ? 1 : 0

  role       = aws_iam_role.push_role[0].name
  policy_arn = aws_iam_policy.push_role[0].arn
}

# The controller also needs permission to assume the push role
data "aws_iam_policy_document" "assume_push_role" {
  count = var.enable_ecr ? 1 : 0

  statement {
    sid    = "AssumePushRole"
    effect = "Allow"
    actions = [
      "sts:AssumeRole"
    ]
    resources = [aws_iam_role.push_role[0].arn]
  }
}

resource "aws_iam_policy" "assume_push_role" {
  count = var.enable_ecr ? 1 : 0

  name        = "${var.name}-ecr-assume-push"
  description = "Allow assuming the ECR push role"
  policy      = data.aws_iam_policy_document.assume_push_role[0].json
  tags        = local.tags
}

resource "aws_iam_role_policy_attachment" "controller_assume_push" {
  count = var.enable_ecr ? 1 : 0

  role       = aws_iam_role.backend.name
  policy_arn = aws_iam_policy.assume_push_role[0].arn
}

resource "aws_iam_user_policy_attachment" "controller_assume_push" {
  count = var.create_iam_user && var.enable_ecr ? 1 : 0

  user       = aws_iam_user.backend[0].name
  policy_arn = aws_iam_policy.assume_push_role[0].arn
}

# -----------------------------------------------------------------------------
# S3 User Permissions Boundary
# -----------------------------------------------------------------------------
# This policy is attached as a permissions boundary to every IAM user created by the
# S3 extension, capping their effective permissions to S3 operations on the Rise bucket
# prefix. Even if the inline policy were broader, the boundary prevents privilege escalation.

resource "aws_iam_policy" "s3_user_boundary" {
  count = var.enable_s3 ? 1 : 0

  name        = "${var.name}-s3-user-boundary"
  description = "Permissions boundary for Rise-managed S3 IAM users. Caps maximum permissions to S3 access on the Rise bucket prefix."
  tags        = local.tags

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid    = "S3BucketAccess"
        Effect = "Allow"
        Action = ["s3:*"]
        Resource = [
          "arn:aws:s3:::${local.s3_bucket_prefix}-*",
          "arn:aws:s3:::${local.s3_bucket_prefix}-*/*",
        ]
      }
    ]
  })
}

# -----------------------------------------------------------------------------
# RDS DB Subnet Group
# -----------------------------------------------------------------------------
# Subnet group defines which subnets RDS instances can be placed in

resource "aws_db_subnet_group" "rds" {
  count = var.enable_rds && var.rds_vpc_id != null && length(var.rds_subnet_ids) > 0 ? 1 : 0

  name_prefix = "${var.name}-rds-"
  description = "DB subnet group for Rise RDS instances"
  subnet_ids  = var.rds_subnet_ids

  tags = merge(local.tags, {
    Name = "${var.name}-rds"
  })
}

# -----------------------------------------------------------------------------
# RDS Security Group
# -----------------------------------------------------------------------------
# Security group for RDS instances, allowing access from specified security groups

resource "aws_security_group" "rds" {
  count = var.enable_rds && var.rds_vpc_id != null ? 1 : 0

  name_prefix = "${var.name}-rds-"
  description = "Security group for Rise RDS instances"
  vpc_id      = var.rds_vpc_id

  tags = merge(local.tags, {
    Name = "${var.name}-rds"
  })
}

# Allow ingress from specified security groups on PostgreSQL port
resource "aws_vpc_security_group_ingress_rule" "rds_from_allowed_sgs" {
  count = var.enable_rds && var.rds_vpc_id != null ? length(var.rds_allowed_security_groups) : 0

  security_group_id            = aws_security_group.rds[0].id
  referenced_security_group_id = var.rds_allowed_security_groups[count.index]
  from_port                    = 5432
  to_port                      = 5432
  ip_protocol                  = "tcp"
  description                  = "PostgreSQL access from allowed security group"
}

# Allow ingress from specified CIDR blocks on PostgreSQL port
resource "aws_vpc_security_group_ingress_rule" "rds_from_allowed_cidrs" {
  count = var.enable_rds && var.rds_vpc_id != null ? length(var.rds_allowed_cidr_blocks) : 0

  security_group_id = aws_security_group.rds[0].id
  cidr_ipv4         = var.rds_allowed_cidr_blocks[count.index]
  from_port         = 5432
  to_port           = 5432
  ip_protocol       = "tcp"
  description       = "PostgreSQL access from allowed CIDR block"
}

# Allow all egress (RDS instances need to reach out for updates, etc.)
resource "aws_vpc_security_group_egress_rule" "rds_egress" {
  count = var.enable_rds && var.rds_vpc_id != null ? 1 : 0

  security_group_id = aws_security_group.rds[0].id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
  description       = "Allow all outbound traffic"
}
