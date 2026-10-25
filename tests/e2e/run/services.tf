resource "aws_service_discovery_service" "postgres" {
  name = "postgres-${var.scope}"

  dns_config {
    namespace_id   = local.env.cloud_map_namespace_id
    routing_policy = "MULTIVALUE"

    dns_records {
      type = "A"
      ttl  = 10
    }
  }

  force_destroy = true
  tags          = local.tags
}


# -----------------------------------------------------------------------------
# Postgres
#
# A task, not RDS: it starts in seconds rather than minutes, and a fresh
# database per run is the point -- it exercises org and controller-class
# bootstrap, which a long-lived database would only ever test once.
# -----------------------------------------------------------------------------

resource "aws_ecs_task_definition" "postgres" {
  family                   = "${var.name}-${var.scope}-postgres"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = "512"
  memory                   = "1024"
  execution_role_arn       = local.env.execution_role_arn

  container_definitions = jsonencode([
    {
      name      = "postgres"
      image     = var.postgres_image
      essential = true

      environment = [
        { name = "POSTGRES_USER", value = "rise" },
        { name = "POSTGRES_PASSWORD", value = local.postgres_password },
        { name = "POSTGRES_DB", value = "rise" },
      ]

      portMappings = [{ containerPort = 5432 }]

      healthCheck = {
        command     = ["CMD-SHELL", "pg_isready -U rise"]
        interval    = 10
        timeout     = 5
        retries     = 5
        startPeriod = 30
      }

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = local.env.log_group_name
          "awslogs-region"        = var.region
          "awslogs-stream-prefix" = "postgres-${var.scope}"
        }
      }
    }
  ])

  tags = local.tags
}

resource "aws_ecs_service" "postgres" {
  name            = "${var.name}-${var.scope}-postgres"
  cluster         = local.env.cluster_arn
  task_definition = aws_ecs_task_definition.postgres.arn
  launch_type     = "FARGATE"
  desired_count   = 1

  network_configuration {
    subnets          = local.env.subnet_ids
    security_groups  = [aws_security_group.internal.id]
    assign_public_ip = true
  }

  service_registries {
    registry_arn = aws_service_discovery_service.postgres.arn
  }

  propagate_tags = "SERVICE"
  tags           = local.tags

  lifecycle {
    replace_triggered_by = [aws_service_discovery_service.postgres]
  }
}

# -----------------------------------------------------------------------------
# Dex
#
# Only Rise talks to it for discovery and JWKS, over private DNS -- which is why
# the issuer is a Cloud Map address and never needs to resolve publicly. The
# harness reaches the token endpoint through Traefik to mint user tokens with
# the password grant.
# -----------------------------------------------------------------------------

resource "aws_service_discovery_service" "dex" {
  name = "dex-${var.scope}"

  dns_config {
    namespace_id   = local.env.cloud_map_namespace_id
    routing_policy = "MULTIVALUE"

    dns_records {
      type = "A"
      ttl  = 10
    }
  }

  force_destroy = true
  tags          = local.tags
}

resource "aws_ecs_task_definition" "dex" {
  family                   = "${var.name}-${var.scope}-dex"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = "256"
  memory                   = "512"
  execution_role_arn       = local.env.execution_role_arn

  runtime_platform {
    cpu_architecture        = var.cpu_architecture
    operating_system_family = "LINUX"
  }

  container_definitions = jsonencode([
    {
      name      = "dex"
      image     = var.dex_image
      essential = true

      # Fargate cannot bind-mount a file, so the config arrives base64-encoded
      # and is written at start.
      entryPoint = ["/bin/sh", "-c"]
      command = [
        "echo \"$DEX_CONFIG_B64\" | base64 -d > /tmp/dex.yaml && exec /usr/local/bin/dex serve /tmp/dex.yaml"
      ]

      environment = [
        { name = "DEX_CONFIG_B64", value = base64encode(local.dex_config) }
      ]

      portMappings = [{ containerPort = 5556 }]

      dockerLabels = {
        "traefik.enable"                                     = "true"
        "traefik.http.routers.dex.rule"                      = "Host(`dex.${local.domain}`)"
        "traefik.http.routers.dex.entrypoints"               = "web"
        "traefik.http.routers.dex.service"                   = "dex"
        "traefik.http.services.dex.loadbalancer.server.port" = "5556"
        # Without this the run's own Traefik filters Dex out: the constraint
        # applies to every container it considers, Rise-created or not.
        "rise.dev/controller-class" = local.controller_class
      }

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = local.env.log_group_name
          "awslogs-region"        = var.region
          "awslogs-stream-prefix" = "dex-${var.scope}"
        }
      }
    }
  ])

  tags = local.tags
}

resource "aws_ecs_service" "dex" {
  name            = "${var.name}-${var.scope}-dex"
  cluster         = local.env.cluster_arn
  task_definition = aws_ecs_task_definition.dex.arn
  desired_count   = 1
  launch_type     = "FARGATE"

  network_configuration {
    subnets          = local.env.subnet_ids
    security_groups  = [aws_security_group.internal.id]
    assign_public_ip = true
  }

  service_registries {
    registry_arn = aws_service_discovery_service.dex.arn
  }

  tags = local.tags

  lifecycle {
    replace_triggered_by = [aws_service_discovery_service.dex]
  }
}
