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

  health_check_custom_config {}
  force_destroy = true
  tags          = local.tags
}

resource "aws_service_discovery_service" "rise" {
  name = "rise-${var.scope}"

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
}

# -----------------------------------------------------------------------------
# The Rise control plane, running the image under test
# -----------------------------------------------------------------------------

resource "aws_ecs_task_definition" "rise" {
  family                   = "${var.name}-${var.scope}-rise"
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
          "awslogs-stream-prefix" = "rise-${var.scope}"
        }
      }
    }
  ])

  tags = local.tags
}

resource "aws_ecs_service" "rise" {
  name            = "${var.name}-${var.scope}-rise"
  cluster         = local.env.cluster_arn
  task_definition = aws_ecs_task_definition.rise.arn
  launch_type     = "FARGATE"
  desired_count   = 1

  network_configuration {
    subnets          = local.env.subnet_ids
    security_groups  = [aws_security_group.internal.id]
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

# -----------------------------------------------------------------------------
# Traefik
#
# Per run, like everything else here. That costs a task start per suite and buys
# three things: the routing layer is exercised coming up from scratch, which is
# what an operator actually does; a run cannot be broken by another run's task
# replacement; and nothing of it outlives the run.
#
# Plain HTTP -- no ACME, no EFS, no certificate store.
# -----------------------------------------------------------------------------

resource "aws_service_discovery_service" "traefik" {
  name = "traefik-${var.scope}"

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

resource "aws_ecs_task_definition" "traefik" {
  family                   = "${var.name}-${var.scope}-traefik"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = "512"
  memory                   = "1024"
  execution_role_arn       = local.env.execution_role_arn
  task_role_arn            = local.env.traefik_task_role_arn

  runtime_platform {
    cpu_architecture        = var.cpu_architecture
    operating_system_family = "LINUX"
  }

  container_definitions = jsonencode([
    {
      name      = "traefik"
      image     = var.traefik_image
      essential = true

      command = [
        # Traefik defaults to ERROR, at which a provider that discovers nothing
        # logs nothing at all -- the failure this run is most likely to hit is
        # exactly the one the default level hides.
        "--log.level=INFO",
        "--providers.ecs=true",
        "--providers.ecs.clusters=${local.env.cluster_name}",
        "--providers.ecs.region=${var.region}",
        # Defaults to *true*, which would give every task in the cluster a
        # router -- including ones Rise did not create.
        "--providers.ecs.exposedByDefault=false",
        # And this confines it to *this run's* containers. The cluster is shared
        # by every concurrent run; without it each Traefik would route them all
        # and several would answer for the same hosts. Rise stamps the
        # controller class into dockerLabels for exactly this.
        "--providers.ecs.constraints=Label(`rise.dev/controller-class`, `${local.controller_class}`)",
        "--providers.ecs.refreshSeconds=5",
        # See modules/rise-ecs/traefik.tf: gating on ECS task health would
        # drop every task that carries no container healthCheck, which is all
        # of them.
        "--providers.ecs.healthyTasksOnly=false",
        "--entrypoints.web.address=:80",
        "--entrypoints.ping.address=:8082",
        "--ping=true",
        "--ping.entrypoint=ping",
        # Unauthenticated, contained by the security group alone. Rise reads
        # serverStatus here, which is the sole readiness signal for a project
        # with a health_check.
        "--api=true",
        "--api.insecure=true",
        "--entrypoints.traefik.address=:8080",
      ]

      portMappings = [
        { containerPort = 80 },
        { containerPort = 8080 },
        { containerPort = 8082 },
      ]

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = local.env.log_group_name
          "awslogs-region"        = var.region
          "awslogs-stream-prefix" = "traefik-${var.scope}"
        }
      }
    }
  ])

  tags = local.tags
}

resource "aws_ecs_service" "traefik" {
  name            = "${var.name}-${var.scope}-traefik"
  cluster         = local.env.cluster_arn
  task_definition = aws_ecs_task_definition.traefik.arn
  desired_count   = 1
  launch_type     = "FARGATE"

  network_configuration {
    subnets          = local.env.subnet_ids
    security_groups  = [aws_security_group.edge.id]
    assign_public_ip = true
  }

  service_registries {
    registry_arn = aws_service_discovery_service.traefik.arn
  }

  tags = local.tags
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

  health_check_custom_config {}
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
}
