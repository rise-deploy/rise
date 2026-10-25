# A demo identity provider, so the module produces a usable install without an
# existing IdP. Storage is in-memory: sessions and refresh tokens do not survive
# a task replacement, and there is no HA. Point oidc_issuer at a real provider
# for anything you intend to keep.

locals {
  dex_config = var.enabled ? templatefile("${path.module}/templates/dex-config.yaml.tftpl", {
    # Public, through Traefik -- deliberately not the Cloud Map address. A
    # browser performs the authorization-code redirect, and a browser cannot
    # resolve a private DNS namespace. It also keeps the backend's SSRF defaults
    # closed, since discovery is then an outbound HTTPS fetch to a public name.
    issuer                = var.identity.issuer
    client_id             = var.identity.client_id
    client_secret         = var.identity.client_secret
    public_url            = var.identity.public_url
    admin_email           = var.identity.admin_email
    admin_username        = split("@", var.identity.admin_email)[0]
    admin_password_bcrypt = var.identity.admin_password_bcrypt
  }) : ""
}

resource "aws_ecs_task_definition" "dex" {
  count = var.enabled ? 1 : 0

  family                   = "${var.name}-dex"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = "256"
  memory                   = "512"
  execution_role_arn       = var.task.execution_role_arn

  runtime_platform {
    cpu_architecture        = var.task.cpu_architecture
    operating_system_family = "LINUX"
  }

  container_definitions = jsonencode([
    {
      name      = "dex"
      image     = var.task.image
      essential = true

      # Fargate cannot bind-mount a file, so the config is carried in as base64
      # and written at start.
      entryPoint = ["/bin/sh", "-c"]
      command = [
        "echo \"$DEX_CONFIG_B64\" | base64 -d > /tmp/dex.yaml && exec /usr/local/bin/dex serve /tmp/dex.yaml"
      ]

      environment = [
        { name = "DEX_CONFIG_B64", value = base64encode(local.dex_config) }
      ]

      portMappings = [{ containerPort = 5556 }]

      dockerLabels = merge(
        {
          "traefik.enable"                                     = "true"
          "traefik.http.routers.dex.rule"                      = "Host(`dex.${var.ingress.domain}`)"
          "traefik.http.routers.dex.entrypoints"               = var.ingress.entrypoint
          "traefik.http.routers.dex.service"                   = "dex"
          "traefik.http.services.dex.loadbalancer.server.port" = "5556"
        },
        var.ingress.acme_enabled ? {
          "traefik.http.routers.dex.tls.certresolver" = "letsencrypt"
        } : {}
      )

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = var.logging.group_name
          "awslogs-region"        = var.logging.region
          "awslogs-stream-prefix" = "dex"
        }
      }
    }
  ])

  tags = var.tags

  lifecycle {
    precondition {
      condition     = var.identity.admin_password_bcrypt != null
      error_message = "deploy_dex requires dex_admin_password_bcrypt. Generate it with: htpasswd -bnBC 10 \"\" 'your-password' | tr -d ':\\n'"
    }
  }
}

resource "aws_ecs_service" "dex" {
  count = var.enabled ? 1 : 0

  name            = "${var.name}-dex"
  cluster         = var.cluster_arn
  task_definition = aws_ecs_task_definition.dex[0].arn
  launch_type     = "FARGATE"
  desired_count   = 1

  network_configuration {
    subnets          = var.network.subnet_ids
    security_groups  = [var.network.security_group_id]
    assign_public_ip = false
  }

  service_registries {
    registry_arn = aws_service_discovery_service.dex[0].arn
  }

  deployment_circuit_breaker {
    enable   = true
    rollback = true
  }

  propagate_tags = "SERVICE"
  tags           = var.tags

  lifecycle {
    replace_triggered_by = [aws_service_discovery_service.dex[count.index]]
  }

  depends_on = [var.runtime_services]
}

resource "aws_service_discovery_service" "dex" {
  count = var.enabled ? 1 : 0

  name = var.discovery.name

  dns_config {
    namespace_id   = var.discovery.namespace_id
    routing_policy = "MULTIVALUE"

    dns_records {
      type = "A"
      ttl  = 10
    }
  }

  force_destroy = true
  tags          = var.tags
}
