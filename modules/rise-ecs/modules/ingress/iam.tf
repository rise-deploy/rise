# Traefik's own IAM. Deliberately not the Rise controller role: Traefik needs to
# read the cluster's tasks and nothing else, and giving it the identity that can
# create services and pass roles would put the whole control-plane surface
# behind an unauthenticated dashboard.

data "aws_iam_policy_document" "traefik_assume" {
  count = var.traefik_role.create ? 1 : 0

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
      values   = [var.traefik_role.account_id]
    }
  }
}

resource "aws_iam_role" "traefik" {
  count = var.traefik_role.create ? 1 : 0

  name               = "${var.name}-traefik"
  description        = "Traefik ECS provider discovery"
  assume_role_policy = data.aws_iam_policy_document.traefik_assume[0].json
  tags               = var.tags
}

locals {
  # The action list Traefik's ECS provider actually calls, as published by its
  # own documentation. It calls ec2:DescribeInstances and
  # ssm:DescribeInstanceInformation even on Fargate, where neither has anything
  # to return; denied either one, discovery yields nothing at all and Traefik
  # 404s every host while looking perfectly healthy.
  traefik_discovery_actions = [
    "ecs:ListClusters",
    "ecs:DescribeClusters",
    "ecs:ListTasks",
    "ecs:DescribeTasks",
    "ecs:DescribeContainerInstances",
    "ecs:DescribeTaskDefinition",
    "ec2:DescribeInstances",
    "ssm:DescribeInstanceInformation",
  ]
}

data "aws_iam_policy_document" "traefik" {
  count = var.traefik_role.create ? 1 : 0

  statement {
    sid       = "DiscoverTasks"
    effect    = "Allow"
    actions   = local.traefik_discovery_actions
    resources = ["*"]
  }

  dynamic "statement" {
    for_each = local.acme_enabled ? [1] : []
    content {
      sid    = "MountCertificateStore"
      effect = "Allow"
      actions = [
        "elasticfilesystem:ClientMount",
        "elasticfilesystem:ClientWrite",
      ]
      resources = [aws_efs_file_system.acme[0].arn]
    }
  }
}

resource "aws_iam_role_policy" "traefik" {
  count = var.traefik_role.create ? 1 : 0

  name   = "discovery"
  role   = aws_iam_role.traefik[0].id
  policy = data.aws_iam_policy_document.traefik[0].json
}
