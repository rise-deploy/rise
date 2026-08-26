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
