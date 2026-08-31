# Traefik's own IAM. Deliberately not the Rise controller role: Traefik needs to
# read the cluster's tasks and nothing else, and giving it the identity that can
# create services and pass roles would put the whole control-plane surface
# behind an unauthenticated dashboard.

data "aws_iam_policy_document" "traefik_assume" {
  count = local.create_traefik_task_role ? 1 : 0

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
  count = local.create_traefik_task_role ? 1 : 0

  name               = "${local.name}-traefik"
  description        = "Traefik ECS provider discovery"
  assume_role_policy = data.aws_iam_policy_document.traefik_assume[0].json
  tags               = local.tags
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
  count = local.create_traefik_task_role ? 1 : 0

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
  count = local.create_traefik_task_role ? 1 : 0

  name   = "discovery"
  role   = aws_iam_role.traefik[0].id
  policy = data.aws_iam_policy_document.traefik[0].json
}

locals {
  traefik_command = concat(
    [
      "--providers.ecs=true",
      "--providers.ecs.clusters=${local.cluster_name}",
      "--providers.ecs.region=${local.region}",
      # Defaults to *true*, which would give every task in the cluster a router
      # -- including ones Rise did not create.
      "--providers.ecs.exposedByDefault=false",
      "--providers.ecs.refreshSeconds=${var.traefik_refresh_seconds}",
      # Traefik's own per-server health check is the readiness signal: the
      # renderer emits loadbalancer.healthcheck.* for every container with an
      # effective health path, and Rise reads the resulting serverStatus.
      #
      # `healthyTasksOnly=true` would gate that on ECS's task health instead,
      # which only exists when the task definition carries a container
      # healthCheck -- a command run *inside* the container, so it would mean
      # requiring curl or wget in every user image. Tasks without one report
      # UNKNOWN, never HEALTHY, and Traefik would drop them: nothing would ever
      # be routed. Docker's provider has no equivalent gate, so this also keeps
      # the two Traefik-fronted backends on the same readiness semantics.
      "--providers.ecs.healthyTasksOnly=false",
    ],
    # Confine discovery to one Rise install. Without it a cluster shared by two
    # installs gives each Traefik the other's containers -- both would answer for
    # the same hosts. Rise stamps its controller class into `dockerLabels`
    # precisely so this can match; `traefik.*` is reserved by Traefik and cannot
    # be used as a constraint key.
    var.traefik_constraints == "" ? [] : [
      "--providers.ecs.constraints=${var.traefik_constraints}",
    ],
    [
      "--entrypoints.web.address=:80",
      "--entrypoints.websecure.address=:443",
      # A dedicated entrypoint for the load balancer's health check, so it does
      # not depend on a router existing.
      "--entrypoints.ping.address=:8082",
      "--ping=true",
      "--ping.entrypoint=ping",
      # Unauthenticated, and contained by the security group alone: only the
      # control-plane group reaches 8080. Rise reads serverStatus here, which is
      # the sole readiness signal for a project with a health_check.
      "--api=true",
      "--api.insecure=true",
      "--log.level=INFO",
    ],
    local.acme_enabled ? [
      "--certificatesresolvers.letsencrypt.acme.email=${var.acme_email}",
      "--certificatesresolvers.letsencrypt.acme.storage=/acme/acme.json",
      "--certificatesresolvers.letsencrypt.acme.httpchallenge=true",
      "--certificatesresolvers.letsencrypt.acme.httpchallenge.entrypoint=web",
    ] : []
  )
}

resource "aws_ecs_task_definition" "traefik" {
  family                   = "${local.name}-traefik"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = "512"
  memory                   = "1024"
  execution_role_arn       = var.execution_role_arn
  task_role_arn            = local.traefik_task_role_arn

  runtime_platform {
    cpu_architecture        = var.cpu_architecture
    operating_system_family = "LINUX"
  }

  dynamic "volume" {
    for_each = local.acme_enabled ? [1] : []
    content {
      name = "acme"

      efs_volume_configuration {
        file_system_id     = aws_efs_file_system.acme[0].id
        transit_encryption = "ENABLED"

        authorization_config {
          access_point_id = aws_efs_access_point.acme[0].id
          iam             = "ENABLED"
        }
      }
    }
  }

  container_definitions = jsonencode([
    {
      name      = "traefik"
      image     = var.traefik_image
      essential = true
      command   = local.traefik_command

      portMappings = [
        { containerPort = 80 },
        { containerPort = 443 },
        { containerPort = 8080 },
        { containerPort = 8082 },
      ]

      mountPoints = local.acme_enabled ? [
        { sourceVolume = "acme", containerPath = "/acme", readOnly = false }
      ] : []

      healthCheck = {
        command = [
          "CMD",
          "traefik",
          "healthcheck",
          "--ping=true",
          "--entrypoints.ping.address=:8082",
          "--ping.entrypoint=ping",
        ]
        interval    = 30
        timeout     = 5
        retries     = 3
        startPeriod = 10
      }

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.this.name
          "awslogs-region"        = local.region
          "awslogs-stream-prefix" = "traefik"
        }
      }
    }
  ])

  tags = local.tags
}

resource "aws_ecs_service" "traefik" {
  name            = "${local.name}-traefik"
  cluster         = local.cluster_arn
  task_definition = aws_ecs_task_definition.traefik.arn
  launch_type     = "FARGATE"

  # One replica, and not a variable. Traefik's ACME file store is not
  # multi-writer safe: two tasks sharing acme.json race each other's
  # certificate orders and can corrupt the file.
  desired_count = 1

  # The default 100/200 would briefly run a second Traefik task during a
  # redeploy -- which is the same race, just narrower. Stop the old task first.
  deployment_minimum_healthy_percent = local.acme_enabled ? 0 : 100
  deployment_maximum_percent         = local.acme_enabled ? 100 : 200

  network_configuration {
    subnets          = local.private_subnet_ids
    security_groups  = [aws_security_group.traefik.id]
    assign_public_ip = false
  }

  dynamic "load_balancer" {
    for_each = aws_lb_target_group.traefik
    content {
      target_group_arn = load_balancer.value.arn
      container_name   = "traefik"
      container_port   = tonumber(load_balancer.key)
    }
  }

  service_registries {
    registry_arn = aws_service_discovery_service.traefik.arn
  }

  propagate_tags = "SERVICE"
  tags           = local.tags

  lifecycle {
    # Replacing the discovery service removes its instances even when its ARN
    # remains stable. A fresh ECS service registers its tasks again.
    replace_triggered_by = [aws_service_discovery_service.traefik]
  }

  depends_on = [
    aws_lb_listener.http,
    aws_efs_mount_target.acme,
  ]
}
