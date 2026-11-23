provider "aws" {
  region                      = "eu-central-1"
  access_key                  = "AKIAIOSFODNN7EXAMPLE"
  secret_key                  = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
  skip_credentials_validation = true
  skip_requesting_account_id  = true
  skip_metadata_api_check     = true
  skip_region_validation      = true
}

variables {
  cluster = {
    name = "rise"
    arn  = "arn:aws:ecs:eu-central-1:123456789012:cluster/rise"
  }
  discovery = {
    namespace_id   = "ns-abc"
    namespace_name = "rise.internal"
  }
  network = {
    control_plane = { subnet_ids = ["subnet-abc"], security_group_ids = ["sg-rise"] }
    traefik       = { subnet_ids = ["subnet-abc"], security_group_ids = ["sg-traefik"] }
  }
  roles = {
    execution     = "arn:aws:iam::123456789012:role/execution"
    control_plane = "arn:aws:iam::123456789012:role/rise"
    traefik       = "arn:aws:iam::123456789012:role/traefik"
  }
  logging = { group_name = "/rise", region = "eu-central-1" }
  control_plane = {
    name           = "rise-control-plane"
    discovery_name = "rise-control-plane"
    image          = "ghcr.io/rise-deploy/rise:0.23.0"
    labels         = { "rise.dev/controller-class" = "default" }
  }
  environment        = { RISE_CONFIG_RUN_MODE = "ecs" }
  secret_environment = { DATABASE_URL = "arn:aws:secretsmanager:eu-central-1:123456789012:secret:rise/database-abc123" }
  traefik = {
    name           = "rise-traefik"
    discovery_name = "rise-traefik"
    image          = "traefik:v3.7.10"
  }
}

run "routing_and_discovery_contract" {
  command = plan

  assert {
    condition = (
      aws_ecs_service.rise.health_check_grace_period_seconds == 1800 &&
      one(aws_ecs_service.rise.deployment_circuit_breaker).rollback
    )
    error_message = "Rise must have a configurable startup budget and retain the default rollback policy."
  }

  assert {
    condition = alltrue([
      for flag in [
        "--providers.ecs=true",
        "--providers.ecs.clusters=rise",
        "--providers.ecs.exposedByDefault=false",
        "--providers.ecs.healthyTasksOnly=false",
        "--providers.http.endpoint=http://rise-control-plane.rise.internal:3001/internal/traefik/config",
        "--providers.http.pollInterval=5s",
        "--entrypoints.rise-catalog.address=127.0.0.1:8083",
        "--api.insecure=true",
      ] : contains(jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].command, flag)
    ])
    error_message = "Traefik must discover ECS servers and fetch public routes from Rise's internal listener."
  }

  assert {
    condition = alltrue([
      for port in [3000, 3001] : contains([
        for mapping in jsondecode(aws_ecs_task_definition.rise.container_definitions)[0].portMappings : mapping.containerPort
      ], port)
    ])
    error_message = "Rise must declare both its public and internal listeners."
  }

  assert {
    condition = alltrue([
      length(aws_service_discovery_service.rise.health_check_custom_config) == 0,
      length(aws_service_discovery_service.traefik.health_check_custom_config) == 0,
    ])
    error_message = "Cloud Map custom health checks must be absent to avoid perpetual replacement."
  }

  assert {
    condition = jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].healthCheck == {
      command     = ["CMD", "traefik", "healthcheck", "--ping=true", "--entrypoints.ping.address=:8082", "--ping.entrypoint=ping"]
      interval    = 30
      timeout     = 5
      retries     = 3
      startPeriod = 10
    }
    error_message = "Traefik's ECS health check must use its dedicated ping entrypoint."
  }

  assert {
    condition = (
      jsondecode(aws_ecs_task_definition.rise.container_definitions)[0].secrets[0].valueFrom == var.secret_environment.DATABASE_URL &&
      !contains([for entry in jsondecode(aws_ecs_task_definition.rise.container_definitions)[0].environment : entry.name], "DATABASE_URL")
    )
    error_message = "Secret references must reach ECS secret injection without becoming plain environment values."
  }
}

run "traefik_without_a_config_secret_sends_no_header" {
  command = plan

  assert {
    condition = (
      !contains(keys(jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0]), "entryPoint") &&
      !contains(keys(jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0]), "secrets")
    )
    error_message = "Without a config secret Traefik must start with its image entrypoint and no secret injection."
  }
}

run "traefik_presents_the_config_secret_without_exposing_it" {
  command = plan

  variables {
    traefik = {
      name              = "rise-traefik"
      discovery_name    = "rise-traefik"
      image             = "traefik:v3.7.10"
      config_secret_arn = "arn:aws:secretsmanager:eu-central-1:123456789012:secret:rise/traefik-config-secret-abc123"
    }
  }

  assert {
    condition = (
      jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].secrets == [
        { name = "RISE_TRAEFIK_CONFIG_SECRET", valueFrom = var.traefik.config_secret_arn }
      ] &&
      !contains(keys(jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0]), "environment")
    )
    error_message = "The config secret must reach Traefik through ECS secret injection, not a plain environment value."
  }

  # The flag is appended by the wrapper from the injected variable, so the
  # command (and the task definition) never contain the token.
  assert {
    condition = (
      jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].entryPoint[0] == "/bin/sh" &&
      strcontains(jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].entryPoint[2], "--providers.http.headers.Authorization=Bearer $${RISE_TRAEFIK_CONFIG_SECRET}") &&
      contains(jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].command, "--providers.http.endpoint=http://rise-control-plane.rise.internal:3001/internal/traefik/config")
    )
    error_message = "Traefik must append the Authorization header flag from the injected secret and keep its own flags."
  }
}

run "control_plane_maintenance_startup_policy" {
  command = plan
  variables {
    control_plane = {
      name                              = "rise-control-plane"
      discovery_name                    = "rise-control-plane"
      image                             = "ghcr.io/rise-deploy/rise:0.23.0"
      labels                            = { "rise.dev/controller-class" = "default" }
      health_check_grace_period_seconds = 5400
      deployment_rollback               = false
    }
  }
  assert {
    condition = (
      aws_ecs_service.rise.health_check_grace_period_seconds == 5400 &&
      !one(aws_ecs_service.rise.deployment_circuit_breaker).rollback
    )
    error_message = "Maintenance startup settings must reach the service without enabling automatic rollback."
  }
}

run "rejects_fractional_startup_budget" {
  command = plan
  variables {
    control_plane = {
      name                              = "rise-control-plane"
      discovery_name                    = "rise-control-plane"
      image                             = "ghcr.io/rise-deploy/rise:0.23.0"
      labels                            = { "rise.dev/controller-class" = "default" }
      health_check_grace_period_seconds = 0.5
    }
  }
  expect_failures = [var.control_plane]
}

run "acme_has_a_single_writer" {
  command = plan
  variables {
    acme = {
      enabled         = true
      email           = "ops@example.com"
      file_system_id  = "fs-abc"
      access_point_id = "fsap-abc"
    }
    load_balancer_targets = {
      "80"  = "arn:aws:elasticloadbalancing:eu-central-1:123456789012:targetgroup/rise-80/1234567890123456"
      "443" = "arn:aws:elasticloadbalancing:eu-central-1:123456789012:targetgroup/rise-443/1234567890123456"
    }
  }

  assert {
    condition = (
      aws_ecs_service.traefik.desired_count == 1 &&
      aws_ecs_service.traefik.deployment_minimum_healthy_percent == 0 &&
      aws_ecs_service.traefik.deployment_maximum_percent == 100 &&
      length(aws_ecs_service.traefik.load_balancer) == 2 &&
      one(aws_ecs_task_definition.traefik.volume).efs_volume_configuration[0].authorization_config[0].iam == "ENABLED"
    )
    error_message = "ACME requires authenticated EFS access and stop-before-start deployment of a single Traefik task."
  }
}

run "direct_http_uses_no_certificate_store_or_load_balancer" {
  command = plan
  variables {
    network = {
      control_plane = { subnet_ids = ["subnet-abc"], security_group_ids = ["sg-internal"], assign_public_ip = true }
      traefik       = { subnet_ids = ["subnet-abc"], security_group_ids = ["sg-edge"], assign_public_ip = true }
    }
    environment        = { DATABASE_URL = "postgres://rise:ephemeral@postgres:5432/rise" }
    secret_environment = {}
  }

  assert {
    condition = (
      length(aws_ecs_task_definition.traefik.volume) == 0 &&
      length(aws_ecs_service.traefik.load_balancer) == 0 &&
      aws_ecs_service.traefik.network_configuration[0].assign_public_ip &&
      aws_ecs_service.rise.network_configuration[0].assign_public_ip &&
      aws_ecs_service.traefik.deployment_minimum_healthy_percent == 100 &&
      aws_ecs_service.traefik.deployment_maximum_percent == 200
    )
    error_message = "Direct HTTP must work with public task IPs and without certificate storage or a load balancer."
  }
}

run "rejects_incomplete_acme" {
  command = plan
  variables {
    acme = { enabled = true }
  }
  expect_failures = [var.acme]
}

run "client_ip_is_untrusted_by_default" {
  command = plan

  assert {
    condition = !anytrue([
      for flag in jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].command :
      strcontains(flag, "trustedIPs")
    ])
    error_message = "With no load balancer in front, Traefik's direct peer is the client and no peer may be trusted."
  }
}

run "forwarded_headers_are_trusted_on_web_only" {
  command = plan
  variables {
    client_ip = { source = "forwarded-headers", trusted_cidrs = ["10.0.0.0/24", "10.0.1.0/24"] }
  }

  assert {
    condition = [
      for flag in jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].command :
      flag if strcontains(flag, "trustedIPs")
    ] == ["--entrypoints.web.forwardedHeaders.trustedIPs=10.0.0.0/24,10.0.1.0/24"]
    error_message = "ALB traffic arrives on web only; websecure and ping must stay untrusted."
  }
}

run "proxy_protocol_is_read_on_every_entrypoint_the_nlb_reaches" {
  command = plan
  variables {
    client_ip = { source = "proxy-protocol", trusted_cidrs = ["10.0.0.0/24", "10.0.1.0/24"] }
  }

  # web and websecure carry traffic and the ACME HTTP-01 challenge; ping carries
  # the NLB health check, which is prefixed with the header too and would
  # otherwise fail with a 400.
  assert {
    condition = toset([
      for flag in jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].command :
      flag if strcontains(flag, "trustedIPs")
      ]) == toset([
      "--entrypoints.web.proxyProtocol.trustedIPs=10.0.0.0/24,10.0.1.0/24",
      "--entrypoints.websecure.proxyProtocol.trustedIPs=10.0.0.0/24,10.0.1.0/24",
      "--entrypoints.ping.proxyProtocol.trustedIPs=10.0.0.0/24,10.0.1.0/24",
    ])
    error_message = "PROXY protocol must be accepted from the NLB on web, websecure and ping."
  }

  # The container's own health check connects from 127.0.0.1 without a header.
  assert {
    condition     = contains(jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].healthCheck.command, "--entrypoints.ping.address=:8082")
    error_message = "the in-task health check must still address the ping entrypoint"
  }
}

run "rejects_a_trusted_source_without_cidrs" {
  command = plan
  variables {
    client_ip = { source = "proxy-protocol" }
  }
  expect_failures = [var.client_ip]
}

run "rejects_trusting_the_whole_internet" {
  command = plan
  variables {
    client_ip = { source = "forwarded-headers", trusted_cidrs = ["0.0.0.0/0"] }
  }
  expect_failures = [var.client_ip]
}

run "rejects_an_unknown_client_ip_source" {
  command = plan
  variables {
    client_ip = { source = "x-real-ip", trusted_cidrs = ["10.0.0.0/24"] }
  }
  expect_failures = [var.client_ip]
}

# The wrapper is `exec traefik "$@" <header flag>`, so every flag in `command`
# -- including the client-address trust flags -- still reaches Traefik as an
# argument and the header is appended after them.
run "client_ip_flags_and_the_config_secret_coexist" {
  command = plan
  variables {
    client_ip = { source = "proxy-protocol", trusted_cidrs = ["10.0.0.0/24"] }
    traefik = {
      name              = "rise-traefik"
      discovery_name    = "rise-traefik"
      image             = "traefik:v3.7.10"
      config_secret_arn = "arn:aws:secretsmanager:eu-central-1:123456789012:secret:rise/traefik-config-secret-abc123"
    }
  }

  assert {
    condition = toset([
      for flag in jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].command :
      flag if strcontains(flag, "trustedIPs")
      ]) == toset([
      "--entrypoints.web.proxyProtocol.trustedIPs=10.0.0.0/24",
      "--entrypoints.websecure.proxyProtocol.trustedIPs=10.0.0.0/24",
      "--entrypoints.ping.proxyProtocol.trustedIPs=10.0.0.0/24",
    ])
    error_message = "the trust flags must stay in command, where the wrapper passes them on as arguments"
  }

  assert {
    condition = (
      strcontains(jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].entryPoint[2], "exec traefik \"$@\" ") &&
      !strcontains(jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].entryPoint[2], "exec /") &&
      !anytrue([for flag in jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].command : strcontains(flag, "headers.Authorization")])
    )
    error_message = "the wrapper must exec traefik through PATH (the image keeps it at /usr/local/bin, not /traefik) with the command's arguments, and the header must not be in command"
  }
}
