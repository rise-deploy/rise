output "public_url" {
  description = "Rise's URL. The CI bearer's `iss` must equal this."
  value       = module.control_plane_env.public_url
}

output "ingress_domain" {
  description = "This run's subtree, `<scope>.<dns_zone_name>`."
  value       = local.domain
}

output "scope" {
  value = var.scope
}

output "controller_class" {
  description = "What isolates this run's services from every other run's."
  value       = local.controller_class
}

output "rise_internal_url" {
  description = "Rise's in-VPC address. Workloads use it to reach the API, since the edge admits only the harness."
  value       = local.auth_backend_url
}

output "dex_issuer" {
  description = "In-VPC Cloud Map address; only Rise resolves it."
  value       = local.dex_issuer
}

output "dex_token_url" {
  description = "Public token endpoint, through this run's Traefik. The harness mints user tokens here with the password grant."
  value       = "http://dex.${local.domain}/dex/token"
}

output "cluster_name" {
  value = local.env.cluster_name
}

output "region" {
  value = var.region
}

output "ecr_repo_prefix" {
  description = "Scoped to this run, so the harness sweep never enumerates another run's repositories."
  value       = local.scoped_ecr_repo_prefix
}

output "traefik_service_name" {
  description = "The harness resolves this service's task address to point DNS at it."
  value       = module.runtime.traefik.service_name
}

output "dex_service_name" {
  description = "The harness waits for this to be healthy before minting a token."
  value       = aws_ecs_service.dex.name
}

output "rise_service_name" {
  value = module.runtime.rise.service_name
}
