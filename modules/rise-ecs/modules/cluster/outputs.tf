output "cluster" {
  description = "Resolved ECS cluster."
  value       = { name = local.cluster_name, arn = local.cluster_arn }
}
output "discovery" {
  description = "Resolved private DNS namespace."
  value       = { namespace_id = local.namespace_id, namespace_name = local.namespace_name }
}
output "logging" {
  description = "CloudWatch log group for services and workloads."
  value       = { name = aws_cloudwatch_log_group.this.name }
}
