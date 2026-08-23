resource "aws_service_discovery_service" "postgres" {
  name = "postgres"

  dns_config {
    namespace_id   = local.env.cloud_map_namespace_id
    routing_policy = "MULTIVALUE"

    dns_records {
      type = "A"
      ttl  = 10
    }
  }

  health_check_custom_config {}
  force_destroy = true
  tags          = local.tags
}

resource "aws_service_discovery_service" "rise" {
  name = "rise"

  dns_config {
    namespace_id   = local.env.cloud_map_namespace_id
    routing_policy = "MULTIVALUE"

    dns_records {
      type = "A"
      ttl  = 10
    }
  }

  health_check_custom_config {}
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
  family                   = "${var.name}-postgres"
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
          "awslogs-stream-prefix" = "postgres"
        }
      }
    }
  ])

  tags = local.tags
}

resource "aws_ecs_service" "postgres" {
  name            = "${var.name}-postgres"
  cluster         = local.env.cluster_arn
  task_definition = aws_ecs_task_definition.postgres.arn
  launch_type     = "FARGATE"
  desired_count   = 1

  network_configuration {
    subnets          = local.env.subnet_ids
    security_groups  = [local.env.internal_security_group_id]
    assign_public_ip = true
  }

  service_registries {
    registry_arn = aws_service_discovery_service.postgres.arn
  }

  propagate_tags = "SERVICE"
  tags           = local.tags
}

# -----------------------------------------------------------------------------
# The Rise control plane, running the image under test
# -----------------------------------------------------------------------------

resource "aws_ecs_task_definition" "rise" {
  family                   = "${var.name}-rise"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = "1024"
  memory                   = "2048"
  execution_role_arn       = local.env.execution_role_arn
  task_role_arn            = local.env.controller_role_arn

  runtime_platform {
    cpu_architecture        = var.cpu_architecture
    operating_system_family = "LINUX"
  }

  container_definitions = jsonencode([
    {
      name      = "rise"
      image     = "${var.rise_image}:${var.rise_image_tag}"
      essential = true
      command   = ["backend", "server"]

      portMappings = [{ containerPort = 3000 }]

      # Plain environment, not Secrets Manager. The values live and die with the
      # run, and per-run secrets would need new ARNs each time -- which the
      # execution role's grant, fixed at bootstrap, could only follow by way of
      # an IAM write the harness is not allowed to make.
      environment = [
        for k, v in merge(module.control_plane_env.environment, {
          DATABASE_URL            = local.database_url
          RISE_JWT_SIGNING_SECRET = var.jwt_signing_secret
          RISE_ENCRYPTION_KEY     = var.encryption_key
          OIDC_CLIENT_SECRET      = "rise-backend-secret"
        }) : { name = k, value = tostring(v) }
      ]

      healthCheck = {
        command     = ["CMD-SHELL", "curl -fsS http://localhost:3000/health || exit 1"]
        interval    = 15
        timeout     = 5
        retries     = 5
        startPeriod = 60
      }

      dockerLabels = module.control_plane_env.docker_labels

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = local.env.log_group_name
          "awslogs-region"        = var.region
          "awslogs-stream-prefix" = "rise"
        }
      }
    }
  ])

  tags = local.tags
}

resource "aws_ecs_service" "rise" {
  name            = "${var.name}-rise"
  cluster         = local.env.cluster_arn
  task_definition = aws_ecs_task_definition.rise.arn
  launch_type     = "FARGATE"
  desired_count   = 1

  network_configuration {
    subnets          = local.env.subnet_ids
    security_groups  = [local.env.internal_security_group_id]
    assign_public_ip = true
  }

  service_registries {
    registry_arn = aws_service_discovery_service.rise.arn
  }

  # A control plane that cannot start should roll back rather than sit in a
  # restart loop the suite then waits out.
  deployment_circuit_breaker {
    enable   = true
    rollback = true
  }

  propagate_tags = "SERVICE"
  tags           = local.tags

  depends_on = [aws_ecs_service.postgres]
}
