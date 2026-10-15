locals {
  traefik_command = concat(
    [
      "--providers.ecs=true",
      "--providers.ecs.clusters=${var.cluster.name}",
      "--providers.ecs.region=${var.logging.region}",
      # Defaults to *true*, which would give every task in the cluster a router
      # -- including ones Rise did not create.
      "--providers.ecs.exposedByDefault=false",
      "--providers.ecs.refreshSeconds=${var.traefik.refresh_seconds}",
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
      "--providers.http.endpoint=http://${var.control_plane.discovery_name}.${var.discovery.namespace_name}:3001/internal/traefik/config",
      "--providers.http.pollInterval=5s",
    ],
    # Confine discovery to one Rise install. Without it a cluster shared by two
    # installs gives each Traefik the other's containers -- both would answer for
    # the same hosts. Rise stamps its controller class into `dockerLabels`
    # precisely so this can match; `traefik.*` is reserved by Traefik and cannot
    # be used as a constraint key.
    var.traefik.constraints == "" ? [] : [
      "--providers.ecs.constraints=${var.traefik.constraints}",
    ],
    [
      "--entrypoints.web.address=:80",
      "--entrypoints.websecure.address=:443",
      "--entrypoints.rise-catalog.address=127.0.0.1:8083",
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
    var.acme.enabled ? [
      "--certificatesresolvers.letsencrypt.acme.email=${var.acme.email}",
      "--certificatesresolvers.letsencrypt.acme.storage=/acme/acme.json",
      "--certificatesresolvers.letsencrypt.acme.httpchallenge=true",
      "--certificatesresolvers.letsencrypt.acme.httpchallenge.entrypoint=web",
    ] : []
  )
}

resource "aws_ecs_task_definition" "traefik" {
  family                   = var.traefik.name
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = "512"
  memory                   = "1024"
  execution_role_arn       = var.roles.execution
  task_role_arn            = var.roles.traefik

  runtime_platform {
    cpu_architecture        = var.cpu_architecture
    operating_system_family = "LINUX"
  }

  dynamic "volume" {
    for_each = var.acme.enabled ? [1] : []
    content {
      name = "acme"

      efs_volume_configuration {
        file_system_id     = var.acme.file_system_id
        transit_encryption = "ENABLED"

        authorization_config {
          access_point_id = var.acme.access_point_id
          iam             = "ENABLED"
        }
      }
    }
  }

  container_definitions = jsonencode([
    {
      name      = "traefik"
      image     = var.traefik.image
      essential = true
      command   = local.traefik_command

      portMappings = [
        { containerPort = 80 },
        { containerPort = 443 },
        { containerPort = 8080 },
        { containerPort = 8082 },
      ]

      mountPoints = var.acme.enabled ? [
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
          "awslogs-group"         = var.logging.group_name
          "awslogs-region"        = var.logging.region
          "awslogs-stream-prefix" = var.traefik.log_stream_prefix
        }
      }
    }
  ])

  tags = var.tags
}

resource "aws_ecs_service" "traefik" {
  name            = var.traefik.name
  cluster         = var.cluster.arn
  task_definition = aws_ecs_task_definition.traefik.arn
  launch_type     = "FARGATE"

  # One replica, and not a variable. Traefik's ACME file store is not
  # multi-writer safe: two tasks sharing acme.json race each other's
  # certificate orders and can corrupt the file.
  desired_count = 1

  # The default 100/200 would briefly run a second Traefik task during a
  # redeploy -- which is the same race, just narrower. Stop the old task first.
  deployment_minimum_healthy_percent = var.acme.enabled ? 0 : 100
  deployment_maximum_percent         = var.acme.enabled ? 100 : 200

  network_configuration {
    subnets          = var.network.traefik.subnet_ids
    security_groups  = var.network.traefik.security_group_ids
    assign_public_ip = var.network.traefik.assign_public_ip
  }

  dynamic "load_balancer" {
    for_each = var.load_balancer_targets
    content {
      target_group_arn = load_balancer.value
      container_name   = "traefik"
      container_port   = tonumber(load_balancer.key)
    }
  }

  service_registries {
    registry_arn = aws_service_discovery_service.traefik.arn
  }

  propagate_tags = "SERVICE"
  tags           = var.tags

  lifecycle {
    # Replacing the discovery service removes its instances even when its ARN
    # remains stable. A fresh ECS service registers its tasks again.
    replace_triggered_by = [aws_service_discovery_service.traefik]
  }
}
