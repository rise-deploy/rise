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
