# Read by the per-run apply through terraform_remote_state, and by the harness
# through `terraform output -json`.

output "region" {
  value = var.region
}

output "cluster_name" {
  value = aws_ecs_cluster.this.name
}

output "cluster_arn" {
  value = aws_ecs_cluster.this.arn
}

output "subnet_ids" {
  description = "Public subnets. Every task in this environment runs here with a public IP."
  value       = [for s in aws_subnet.public : s.id]
}

output "internal_security_group_id" {
  description = "Group for Rise, Postgres and deployed workloads."
  value       = aws_security_group.internal.id
}

output "edge_security_group_id" {
  description = "Traefik's group. The harness opens port 80 here for the length of a run."
  value       = aws_security_group.edge.id
}

output "cloud_map_namespace_id" {
  value = aws_service_discovery_private_dns_namespace.this.id
}

output "cloud_map_namespace_name" {
  value = aws_service_discovery_private_dns_namespace.this.name
}

output "log_group_name" {
  value = aws_cloudwatch_log_group.this.name
}

output "traefik_service_name" {
  description = "The harness resolves this service's task address at run start."
  value       = aws_ecs_service.traefik.name
}

output "dex_issuer" {
  description = "Cloud Map address. Never publicly resolvable, and it does not need to be: the harness uses the password grant, so no browser redirect is involved."
  value       = local.dex_issuer
}

output "controller_role_arn" {
  value = module.rise_aws.role_arn
}

output "execution_role_arn" {
  value = module.rise_aws.ecs_execution_role_arn
}

output "ecr_push_role_arn" {
  value = module.rise_aws.push_role_arn
}

output "ecr_repo_prefix" {
  description = "rise-aws derives this from `name`; the per-run apply must give Rise the same value."
  value       = "${var.name}/"
}

output "dns_zone_id" {
  value = aws_route53_zone.this.zone_id
}

output "dns_zone_name" {
  value = aws_route53_zone.this.name
}

output "dns_name_servers" {
  description = "Delegate the zone to these from the parent domain, once."
  value       = aws_route53_zone.this.name_servers
}

output "state_bucket" {
  description = "Where the per-run apply keeps its state."
  value       = aws_s3_bucket.state.id
}

output "ci_role_arn" {
  description = "Role the GitHub Actions workflow assumes."
  value       = aws_iam_role.ci.arn
}
