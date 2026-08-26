output "environment" {
  description = "Plain environment variables for the Rise control-plane container. Contains no secrets."
  value       = local.environment
}

output "docker_labels" {
  description = "Traefik routing labels for the Rise control-plane container definition."
  value       = local.docker_labels
}

output "public_url" {
  description = "Rise's public URL. The CI bearer's `iss` must equal this."
  value       = local.public_url
}
