resource "aws_ecs_cluster" "this" {
  count = local.create_cluster ? 1 : 0

  name = var.name

  setting {
    name  = "containerInsights"
    value = var.enable_container_insights ? "enabled" : "disabled"
  }

  tags = merge(var.tags, { Name = var.name })
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
  name              = var.logging.name
  retention_in_days = var.logging.retention_days
  tags              = var.tags
}

# Private discovery names connect the control plane, Traefik and optional Dex.
# Workload discovery is owned by the deployment backend.

resource "aws_service_discovery_private_dns_namespace" "this" {
  count = local.create_namespace ? 1 : 0

  name        = local.namespace_name
  description = "Internal service discovery for the Rise control plane"
  vpc         = var.vpc_id
  tags        = var.tags
}
