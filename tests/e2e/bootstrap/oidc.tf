# The identity GitHub Actions assumes to run the ECS e2e.
#
# Deliberately not in modules/rise-aws: that module is the reusable "deploy Rise
# on AWS" one, and this is specific to this repository's CI.
#
# The shape of the grant follows one rule -- **no IAM writes**. Every role this
# environment needs already exists (created above), so the per-run apply only
# ever creates ECS, CloudWatch and Cloud Map resources. The two IAM-adjacent
# actions it does need are called out below.

resource "aws_iam_openid_connect_provider" "github" {
  count = var.create_github_oidc_provider ? 1 : 0

  url             = "https://token.actions.githubusercontent.com"
  client_id_list  = ["sts.amazonaws.com"]
  thumbprint_list = ["6938fd4d98bab03faadb97b34396831e3780aea1"]

  tags = local.tags
}

locals {
  # Interpolated rather than referenced: the ARN is deterministic, and building
  # it keeps both trust policies known at `terraform plan`. Who may assume a
  # role that can write IAM should be readable in a diff, not "(known after
  # apply)".
  github_oidc_provider_arn = "arn:${local.partition}:iam::${local.account_id}:oidc-provider/token.actions.githubusercontent.com"
}

data "aws_iam_policy_document" "ci_assume" {
  statement {
    effect = "Allow"
    principals {
      type        = "Federated"
      identifiers = [local.github_oidc_provider_arn]
    }
    actions = ["sts:AssumeRoleWithWebIdentity"]

    condition {
      test     = "StringEquals"
      variable = "token.actions.githubusercontent.com:aud"
      values   = ["sts.amazonaws.com"]
    }

    # Any ref of this repository. Narrower would be better, but the e2e runs on
    # a label from pull requests as well as on a schedule from develop.
    condition {
      test     = "StringLike"
      variable = "token.actions.githubusercontent.com:sub"
      values   = ["repo:${var.github_repository}:*"]
    }
  }
}

resource "aws_iam_role" "ci" {
  name                 = "${var.name}-ci"
  description          = "GitHub Actions identity for the Rise ECS e2e"
  assume_role_policy   = data.aws_iam_policy_document.ci_assume.json
  max_session_duration = 3600 * 2
  tags                 = local.tags
}

data "aws_iam_policy_document" "ci" {
  # The per-run stack: task definitions and services, plus the sweep of whatever
  # a previous run's Rise left behind.
  statement {
    sid    = "ManageRunResources"
    effect = "Allow"
    actions = [
      "ecs:CreateService",
      "ecs:UpdateService",
      "ecs:DeleteService",
      "ecs:DescribeServices",
      "ecs:ListServices",
      "ecs:DescribeTasks",
      "ecs:ListTasks",
      "ecs:StopTask",
      "ecs:RunTask",
      "ecs:RegisterTaskDefinition",
      "ecs:DeregisterTaskDefinition",
      "ecs:DescribeTaskDefinition",
      "ecs:ListTaskDefinitions",
      "ecs:DescribeClusters",
      "ecs:TagResource",
      "ecs:UntagResource",
      "ecs:ListTagsForResource",
    ]
    resources = ["*"]
  }

  # Reading task addresses, and managing the run's own security groups. The
  # groups are per-run -- created with the stack and destroyed with it -- so this
  # needs create and delete, not just the rule actions.
  statement {
    sid    = "ReadAddressesAndManageRunSecurityGroups"
    effect = "Allow"
    actions = [
      "ec2:DescribeNetworkInterfaces",
      "ec2:DescribeSecurityGroups",
      "ec2:DescribeSecurityGroupRules",
      "ec2:DescribeVpcs",
      "ec2:DescribeTags",
      "ec2:CreateSecurityGroup",
      "ec2:DeleteSecurityGroup",
      "ec2:AuthorizeSecurityGroupIngress",
      "ec2:RevokeSecurityGroupIngress",
      "ec2:AuthorizeSecurityGroupEgress",
      "ec2:RevokeSecurityGroupEgress",
    ]
    resources = ["*"]
  }

  # Terraform tags a security group in the same call that creates it, which EC2
  # authorizes as a separate CreateTags. Confined to that moment: this does not
  # allow retagging anything that already exists.
  statement {
    sid       = "TagSecurityGroupsAtCreation"
    effect    = "Allow"
    actions   = ["ec2:CreateTags"]
    resources = ["*"]

    condition {
      test     = "StringEquals"
      variable = "ec2:CreateAction"
      values   = ["CreateSecurityGroup"]
    }
  }

  # Registering a task definition that names a role requires passing it. This is
  # an iam: action but not an IAM *write* -- and a blanket iam:* deny would break
  # the per-run apply in a way that reads like an ECS fault, so it is explicit.
  statement {
    sid     = "PassPreCreatedRoles"
    effect  = "Allow"
    actions = ["iam:PassRole"]
    # Interpolated from names rather than referenced, so the whole policy is
    # known at `terraform plan` -- the escalation-prone statement should be
    # readable in a diff, not "(known after apply)".
    resources = [
      "arn:${local.partition}:iam::${local.account_id}:role/${var.name}",
      "arn:${local.partition}:iam::${local.account_id}:role/${var.name}-ecs-execution",
      "arn:${local.partition}:iam::${local.account_id}:role/${var.name}-traefik",
    ]
    condition {
      test     = "StringEquals"
      variable = "iam:PassedToService"
      values   = ["ecs-tasks.amazonaws.com"]
    }
  }

  # Pointing the stable domain at Traefik's current address at run start.
  statement {
    sid    = "UpdateEnvironmentDNS"
    effect = "Allow"
    actions = [
      "route53:ChangeResourceRecordSets",
      "route53:GetChange",
      "route53:ListResourceRecordSets",
    ]
    # Hosted-zone IDs are assigned by AWS, so scoping to this one would leave
    # the policy unknown until apply. The account is a dedicated scratch account
    # holding exactly this zone; readability of the whole document is worth more
    # than the distinction.
    resources = [
      "arn:${local.partition}:route53:::hostedzone/*",
      "arn:${local.partition}:route53:::change/*",
    ]
  }

  # Cloud Map entries are per-run too -- `traefik-<scope>`, `dex-<scope>`,
  # `postgres-<scope>`, `rise-<scope>` -- so the run creates and deletes them.
  # The namespace itself belongs to the bootstrap and is not writable here.
  statement {
    sid    = "ReadCloudMapAndManageRunServices"
    effect = "Allow"
    actions = [
      "servicediscovery:Get*",
      "servicediscovery:List*",
      "servicediscovery:CreateService",
      "servicediscovery:UpdateService",
      "servicediscovery:DeleteService",
      # `force_destroy` on those services only needs this when a drain ended
      # abnormally and left instances registered -- which is exactly the
      # teardown that must not then fail on AccessDenied.
      "servicediscovery:DeregisterInstance",
      "servicediscovery:TagResource",
      "servicediscovery:UntagResource",
    ]
    resources = ["*"]
  }

  # Sweeping repositories a previous run's projects left behind, and letting the
  # CLI push during registry-build-push-pull.
  statement {
    sid    = "SweepAndUseECR"
    effect = "Allow"
    actions = [
      "ecr:DescribeRepositories",
      "ecr:ListImages",
      "ecr:BatchDeleteImage",
      "ecr:DeleteRepository",
      "ecr:GetAuthorizationToken",
    ]
    resources = ["*"]
  }

  # Secret env vars a previous run's deployments wrote.
  statement {
    sid    = "SweepSecretParameters"
    effect = "Allow"
    actions = [
      # GetParameter is how the harness bootstraps itself: one read of
      # /<name>/e2e/bootstrap tells it everything about the environment.
      "ssm:GetParameter",
      "ssm:GetParameters",
      "ssm:DescribeParameters",
      "ssm:GetParametersByPath",
      "ssm:DeleteParameter",
      "ssm:DeleteParameters",
    ]
    resources = ["arn:${local.partition}:ssm:${var.region}:${local.account_id}:parameter/${var.name}/*"]
  }

  statement {
    sid       = "ReadLogs"
    effect    = "Allow"
    actions   = ["logs:DescribeLogStreams", "logs:GetLogEvents", "logs:FilterLogEvents"]
    resources = ["arn:${local.partition}:logs:${var.region}:${local.account_id}:log-group:/${var.name}*"]
  }

  statement {
    sid    = "PerRunTerraformState"
    effect = "Allow"
    actions = [
      "s3:GetObject",
      "s3:PutObject",
      "s3:DeleteObject",
      "s3:ListBucket",
    ]
    resources = [
      "arn:${local.partition}:s3:::${local.state_bucket}",
      "arn:${local.partition}:s3:::${local.state_bucket}/*",
    ]
  }
}

resource "aws_iam_role_policy" "ci" {
  name   = "e2e"
  role   = aws_iam_role.ci.id
  policy = data.aws_iam_policy_document.ci.json
}

# -----------------------------------------------------------------------------
# The identity that manages *this* workspace from CI
#
# Separate from the run role, and deliberately harder to reach.
#
# **This role can write IAM, and a principal that can create roles and attach
# policies can grant itself anything.** In an account that holds nothing else
# that is an acceptable trade for not applying the bootstrap by hand forever; in
# any account that matters it is not. Two things narrow it:
#
#   - it is opt-in (`enable_ci_bootstrap_role`, default off), so nobody acquires
#     it by applying this module, and
#   - its trust names specific refs (`ci_bootstrap_subjects`, default the
#     develop branch) rather than the run role's `repo:<repo>:*`, so a pull
#     request from a fork -- or from a branch anyone can push -- cannot assume
#     it.
#
# Point it at a GitHub Environment with required reviewers
# (`repo:<repo>:environment:<name>`) if you want a human in the loop.
# -----------------------------------------------------------------------------

data "aws_iam_policy_document" "ci_bootstrap_assume" {
  count = var.enable_ci_bootstrap_role ? 1 : 0

  statement {
    effect = "Allow"
    principals {
      type        = "Federated"
      identifiers = [local.github_oidc_provider_arn]
    }
    actions = ["sts:AssumeRoleWithWebIdentity"]

    condition {
      test     = "StringEquals"
      variable = "token.actions.githubusercontent.com:aud"
      values   = ["sts.amazonaws.com"]
    }

    condition {
      test     = "StringLike"
      variable = "token.actions.githubusercontent.com:sub"
      values = length(var.ci_bootstrap_subjects) > 0 ? var.ci_bootstrap_subjects : [
        "repo:${var.github_repository}:ref:refs/heads/develop"
      ]
    }
  }
}

resource "aws_iam_role" "ci_bootstrap" {
  count = var.enable_ci_bootstrap_role ? 1 : 0

  name                 = "${var.name}-ci-bootstrap"
  description          = "GitHub Actions identity that applies the Rise e2e bootstrap workspace"
  assume_role_policy   = data.aws_iam_policy_document.ci_bootstrap_assume[0].json
  max_session_duration = 3600
  tags                 = local.tags
}

data "aws_iam_policy_document" "ci_bootstrap" {
  count = var.enable_ci_bootstrap_role ? 1 : 0

  # Everything this workspace creates. Scoped by resource where the service
  # supports it and the names are ours to predict; `*` where it does not.
  statement {
    sid    = "ManageEnvironmentInfrastructure"
    effect = "Allow"
    actions = [
      "ecs:*",
      "ec2:*",
      "logs:*",
      "servicediscovery:*",
      "route53:*",
      "ecr:*",
      "ssm:*",
      "kms:DescribeKey",
      "kms:CreateGrant",
      "sts:GetCallerIdentity",
    ]
    resources = ["*"]
  }

  # The escalation-capable part, confined to the role and policy names this
  # workspace owns. It cannot touch an unrelated role in the account.
  statement {
    sid    = "ManageEnvironmentIAM"
    effect = "Allow"
    actions = [
      "iam:CreateRole",
      "iam:DeleteRole",
      "iam:GetRole",
      "iam:UpdateRole",
      "iam:UpdateAssumeRolePolicy",
      "iam:PutRolePolicy",
      "iam:DeleteRolePolicy",
      "iam:GetRolePolicy",
      "iam:ListRolePolicies",
      "iam:AttachRolePolicy",
      "iam:DetachRolePolicy",
      "iam:ListAttachedRolePolicies",
      "iam:TagRole",
      "iam:UntagRole",
      "iam:ListRoleTags",
      "iam:PassRole",
    ]
    resources = [
      "arn:${local.partition}:iam::${local.account_id}:role/${var.name}",
      "arn:${local.partition}:iam::${local.account_id}:role/${var.name}-*",
    ]
  }

  statement {
    sid    = "ManageEnvironmentPolicies"
    effect = "Allow"
    actions = [
      "iam:CreatePolicy",
      "iam:DeletePolicy",
      "iam:GetPolicy",
      "iam:GetPolicyVersion",
      "iam:ListPolicyVersions",
      "iam:CreatePolicyVersion",
      "iam:DeletePolicyVersion",
      "iam:TagPolicy",
      "iam:ListEntitiesForPolicy",
    ]
    resources = [
      "arn:${local.partition}:iam::${local.account_id}:policy/${var.name}",
      "arn:${local.partition}:iam::${local.account_id}:policy/${var.name}-*",
    ]
  }

  # The OIDC provider is account-global and shared, so it is readable but not
  # writable: recreating it would break every other workflow that trusts it.
  statement {
    sid       = "ReadOIDCProvider"
    effect    = "Allow"
    actions   = ["iam:GetOpenIDConnectProvider", "iam:ListOpenIDConnectProviders"]
    resources = ["*"]
  }

  # Its own state, and the run workspace's alongside it.
  statement {
    sid    = "ManageWorkspaceState"
    effect = "Allow"
    actions = [
      "s3:GetObject",
      "s3:PutObject",
      "s3:DeleteObject",
      "s3:ListBucket",
      "s3:GetBucketVersioning",
      "s3:GetBucketPublicAccessBlock",
      "s3:GetEncryptionConfiguration",
      "s3:GetBucketTagging",
      "s3:GetBucketPolicy",
      "s3:GetBucketAcl",
      "s3:GetBucketLocation",
      "s3:GetLifecycleConfiguration",
      "s3:GetReplicationConfiguration",
      "s3:GetAccelerateConfiguration",
      "s3:GetBucketRequestPayment",
      "s3:GetBucketLogging",
      "s3:GetBucketWebsite",
      "s3:GetBucketCORS",
      "s3:GetBucketObjectLockConfiguration",
    ]
    resources = [
      "arn:${local.partition}:s3:::${local.state_bucket}",
      "arn:${local.partition}:s3:::${local.state_bucket}/*",
    ]
  }
}

resource "aws_iam_role_policy" "ci_bootstrap" {
  count = var.enable_ci_bootstrap_role ? 1 : 0

  name   = "bootstrap"
  role   = aws_iam_role.ci_bootstrap[0].id
  policy = data.aws_iam_policy_document.ci_bootstrap[0].json
}
