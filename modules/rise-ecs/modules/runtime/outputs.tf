output "rise" {
  description = "Rise service and discovery identifiers."
  value = {
    service_name        = aws_ecs_service.rise.name
    service_id          = aws_ecs_service.rise.id
    task_definition_arn = aws_ecs_task_definition.rise.arn
    discovery_name      = aws_service_discovery_service.rise.name
    discovery_arn       = aws_service_discovery_service.rise.arn
  }
}

output "traefik" {
  description = "Traefik service, role, and discovery identifiers."
  value = {
    service_name        = aws_ecs_service.traefik.name
    service_id          = aws_ecs_service.traefik.id
    task_definition_arn = aws_ecs_task_definition.traefik.arn
    task_role_arn       = aws_ecs_task_definition.traefik.task_role_arn
    discovery_name      = aws_service_discovery_service.traefik.name
    discovery_arn       = aws_service_discovery_service.traefik.arn
  }
}

output "traefik_command" {
  description = "Traefik startup configuration, including discovery and internal routing."
  value       = local.traefik_command
}
