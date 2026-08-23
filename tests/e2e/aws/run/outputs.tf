output "public_url" {
  description = "Rise's URL. The CI bearer's `iss` must equal this."
  value       = module.control_plane_env.public_url
}

output "ingress_domain" {
  value = var.ingress_domain
}

output "dex_issuer" {
  value = local.env.dex_issuer
}

output "cluster_name" {
  value = local.env.cluster_name
}

output "region" {
  value = var.region
}

output "ecr_repo_prefix" {
  value = local.env.ecr_repo_prefix
}

output "edge_security_group_id" {
  value = local.env.edge_security_group_id
}

output "traefik_service_name" {
  value = local.env.traefik_service_name
}

output "rise_service_name" {
  value = aws_ecs_service.rise.name
}
