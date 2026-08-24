resource "aws_ecs_cluster" "this" {
  name = var.name
  tags = local.tags
}

resource "aws_ecs_cluster_capacity_providers" "this" {
  cluster_name       = aws_ecs_cluster.this.name
  capacity_providers = ["FARGATE"]

  default_capacity_provider_strategy {
    capacity_provider = "FARGATE"
    weight            = 1
  }
}

# awslogs does not create a missing group, it fails the task -- and the
# reconciler stamps this name onto every task definition it registers.
resource "aws_cloudwatch_log_group" "this" {
  name              = "/${var.name}"
  retention_in_days = var.log_retention_days
  tags              = local.tags
}

resource "aws_service_discovery_private_dns_namespace" "this" {
  name        = local.namespace_name
  description = "Internal discovery for the Rise e2e environment"
  vpc         = aws_vpc.this.id
  tags        = local.tags
}

resource "aws_service_discovery_service" "traefik" {
  name = "traefik"

  dns_config {
    namespace_id   = aws_service_discovery_private_dns_namespace.this.id
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

resource "aws_service_discovery_service" "dex" {
  name = "dex"

  dns_config {
    namespace_id   = aws_service_discovery_private_dns_namespace.this.id
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
# Traefik
#
# Persistent: slow to start, carries no per-run state, and its address is what
# the harness points DNS at. Plain HTTP -- no ACME, no EFS, no certificate store.
# -----------------------------------------------------------------------------

resource "aws_ecs_task_definition" "traefik" {
  family                   = "${var.name}-traefik"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = "512"
  memory                   = "1024"
  execution_role_arn       = module.rise_aws.ecs_execution_role_arn
  task_role_arn            = aws_iam_role.traefik.arn

  container_definitions = jsonencode([
    {
      name      = "traefik"
      image     = var.traefik_image
      essential = true
      command = [
        "--providers.ecs=true",
        "--providers.ecs.clusters=${var.name}",
        "--providers.ecs.region=${var.region}",
        # Defaults to true, which would give every task in the cluster a router.
        "--providers.ecs.exposedByDefault=false",
        # Faster than the production default: a test suite waiting on routing is
        # waiting on this poll.
        "--providers.ecs.refreshSeconds=5",
        "--providers.ecs.healthyTasksOnly=true",
        "--entrypoints.web.address=:80",
        "--entrypoints.ping.address=:8082",
        "--ping=true",
        "--ping.entrypoint=ping",
        # Unauthenticated, and contained by the security group alone. Rise reads
        # serverStatus here, which is the only readiness signal for a project
        # with a health_check.
        "--api=true",
        "--api.insecure=true",
        "--log.level=INFO",
      ]

      portMappings = [
        { containerPort = 80 },
        { containerPort = 8080 },
        { containerPort = 8082 },
      ]

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.this.name
          "awslogs-region"        = var.region
          "awslogs-stream-prefix" = "traefik"
        }
      }
    }
  ])

  tags = local.tags
}

resource "aws_ecs_service" "traefik" {
  name            = "${var.name}-traefik"
  cluster         = aws_ecs_cluster.this.arn
  task_definition = aws_ecs_task_definition.traefik.arn
  launch_type     = "FARGATE"
  desired_count   = 1

  network_configuration {
    subnets          = [for s in aws_subnet.public : s.id]
    security_groups  = [aws_security_group.edge.id]
    assign_public_ip = true
  }

  service_registries {
    registry_arn = aws_service_discovery_service.traefik.arn
  }

  propagate_tags = "SERVICE"
  tags           = local.tags
}

# -----------------------------------------------------------------------------
# Dex
#
# Persistent, which is only safe because the domain is stable: its Traefik router
# encodes the domain, so under a changing address it would be stranded by the
# first task replacement.
#
# The issuer is the Cloud Map name, matching what the harness's password-grant
# flow needs -- no browser is involved, so it never has to be publicly
# resolvable, and Rise fetches JWKS from inside the VPC.
# -----------------------------------------------------------------------------

locals {
  dex_issuer = "http://dex.${local.namespace_name}:5556/dex"

  # The repo's dev Dex config, verbatim except for the issuer -- the same trick
  # the bash stack used. It is not decoration: the harness mints tokens with the
  # resource-owner password grant, which that file enables per-client, against
  # the static user it defines (tests/e2e/src/scenario.rs:345,367). A
  # hand-written template here would diverge from it silently.
  dex_config = replace(
    file("${path.module}/../../../dev/dex/config.yaml"),
    "/(?m)^issuer:.*$/",
    "issuer: ${local.dex_issuer}"
  )
}

resource "aws_ecs_task_definition" "dex" {
  family                   = "${var.name}-dex"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = "256"
  memory                   = "512"
  execution_role_arn       = module.rise_aws.ecs_execution_role_arn

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
        "traefik.http.routers.dex.rule"                      = "Host(`dex.${var.dns_zone_name}`)"
        "traefik.http.routers.dex.entrypoints"               = "web"
        "traefik.http.routers.dex.service"                   = "dex"
        "traefik.http.services.dex.loadbalancer.server.port" = "5556"
      }

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.this.name
          "awslogs-region"        = var.region
          "awslogs-stream-prefix" = "dex"
        }
      }
    }
  ])

  tags = local.tags
}

resource "aws_ecs_service" "dex" {
  name            = "${var.name}-dex"
  cluster         = aws_ecs_cluster.this.arn
  task_definition = aws_ecs_task_definition.dex.arn
  launch_type     = "FARGATE"
  desired_count   = 1

  network_configuration {
    subnets          = [for s in aws_subnet.public : s.id]
    security_groups  = [aws_security_group.internal.id]
    assign_public_ip = true
  }

  service_registries {
    registry_arn = aws_service_discovery_service.dex.arn
  }

  propagate_tags = "SERVICE"
  tags           = local.tags

  depends_on = [aws_ecs_service.traefik]
}
