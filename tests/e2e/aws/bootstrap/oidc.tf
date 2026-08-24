# The identity GitHub Actions assumes to run the ECS e2e.
#
# Deliberately not in modules/rise-aws: that module is the reusable "deploy Rise
# on AWS" one, and this is specific to this repository's CI.
#
# The shape of the grant follows one rule -- **no IAM writes**. Every role this
# environment needs already exists (created above), so the per-run apply only
# ever creates ECS, CloudWatch and Cloud Map resources. The two IAM-adjacent
# actions it does need are called out below.

data "aws_iam_openid_connect_provider" "github" {
  count = var.create_github_oidc_provider ? 0 : 1
  url   = "https://token.actions.githubusercontent.com"
}

resource "aws_iam_openid_connect_provider" "github" {
  count = var.create_github_oidc_provider ? 1 : 0

  url             = "https://token.actions.githubusercontent.com"
  client_id_list  = ["sts.amazonaws.com"]
  thumbprint_list = ["6938fd4d98bab03faadb97b34396831e3780aea1"]

  tags = local.tags
}

locals {
  github_oidc_provider_arn = var.create_github_oidc_provider ? (
    aws_iam_openid_connect_provider.github[0].arn
  ) : data.aws_iam_openid_connect_provider.github[0].arn
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

  # Reading task addresses, and reaping the ingress rule a crashed run leaked.
  statement {
    sid    = "ReadAddressesAndManageEdgeIngress"
    effect = "Allow"
    actions = [
      "ec2:DescribeNetworkInterfaces",
      "ec2:DescribeSecurityGroups",
      "ec2:DescribeSecurityGroupRules",
      "ec2:AuthorizeSecurityGroupIngress",
      "ec2:RevokeSecurityGroupIngress",
    ]
    resources = ["*"]
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

  statement {
    sid       = "ReadCloudMap"
    effect    = "Allow"
    actions   = ["servicediscovery:Get*", "servicediscovery:List*"]
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
