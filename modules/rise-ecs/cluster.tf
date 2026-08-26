resource "aws_ecs_cluster" "this" {
  count = local.create_cluster ? 1 : 0

  name = local.name

  setting {
    name  = "containerInsights"
    value = var.enable_container_insights ? "enabled" : "disabled"
  }

  tags = merge(local.tags, { Name = local.name })
}

resource "aws_ecs_cluster_capacity_providers" "this" {
  count = local.create_cluster ? 1 : 0

  cluster_name       = aws_ecs_cluster.this[0].name
  capacity_providers = ["FARGATE", "FARGATE_SPOT"]

  default_capacity_provider_strategy {
    capacity_provider = "FARGATE"
    weight            = 1
  }
}

# Created unconditionally, including when bringing your own cluster: awslogs
# does not create a missing group, it fails the task. The reconciler writes the
# group name onto every task definition it registers, so its absence is a
# cluster-wide failure to start anything.
resource "aws_cloudwatch_log_group" "this" {
  name              = local.log_group_name
  retention_in_days = var.log_retention_days
  tags              = local.tags
}

# --- Cloud Map ---------------------------------------------------------------
#
# Used for the services *this module* runs, not by the reconciler: Cloud Map
# registration for deployed workloads (ADR-0005 D10, cross-container discovery)
# is not implemented, and multi-container deployments fail closed at deploy
# time. It exists here because two internal URLs depend on stable names —
# without it there is no address Traefik can call Rise at for forwardAuth.

resource "aws_service_discovery_private_dns_namespace" "this" {
  count = local.create_namespace ? 1 : 0

  name        = local.namespace_name
  description = "Internal service discovery for the Rise control plane"
  vpc         = local.vpc_id
  tags        = local.tags
}

resource "aws_service_discovery_service" "rise" {
  name = "rise"

  dns_config {
    namespace_id   = local.namespace_id
    routing_policy = "MULTIVALUE"

    dns_records {
      type = "A"
      ttl  = 10
    }
  }

  # Registration is ECS's job, not a Cloud Map health check's.
  health_check_custom_config {}

  # Instances must be deregistered before a Cloud Map service will delete, and
  # ECS deregisters them only as its own tasks drain. `terraform destroy` hits
  # the same wall and may need a retry; that is AWS's ordering, not a bug here.
  force_destroy = true

  tags = local.tags
}

resource "aws_service_discovery_service" "traefik" {
  name = "traefik"

  dns_config {
    namespace_id   = local.namespace_id
    routing_policy = "MULTIVALUE"

    dns_records {
      type = "A"
      ttl  = 10
    }
  }

  # Registration is ECS's job, not a Cloud Map health check's.
  health_check_custom_config {}

  force_destroy = true
  tags          = local.tags
}

resource "aws_service_discovery_service" "dex" {
  count = var.deploy_dex ? 1 : 0

  name = "dex"

  dns_config {
    namespace_id   = local.namespace_id
    routing_policy = "MULTIVALUE"

    dns_records {
      type = "A"
      ttl  = 10
    }
  }

  # Registration is ECS's job, not a Cloud Map health check's.
  health_check_custom_config {}

  force_destroy = true
  tags          = local.tags
}
