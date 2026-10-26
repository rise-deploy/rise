locals {
  create_cluster = var.cluster == null
  cluster_name   = local.create_cluster ? aws_ecs_cluster.this[0].name : var.cluster.name
  cluster_arn    = local.create_cluster ? aws_ecs_cluster.this[0].arn : data.aws_ecs_cluster.brought[0].arn

  # --- Cloud Map ------------------------------------------------------------
  create_namespace = var.discovery.id == null
  namespace_name   = var.discovery.name
  namespace_id     = local.create_namespace ? aws_service_discovery_private_dns_namespace.this[0].id : var.discovery.id
}
