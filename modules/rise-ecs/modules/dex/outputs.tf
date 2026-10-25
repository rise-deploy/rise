output "service_name" {
  description = "Demo identity provider service, when enabled."
  value       = try(aws_ecs_service.dex[0].name, null)
}
