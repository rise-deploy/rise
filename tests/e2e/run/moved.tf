moved {
  from = aws_ecs_task_definition.rise
  to   = module.runtime.aws_ecs_task_definition.rise
}

moved {
  from = aws_ecs_task_definition.traefik
  to   = module.runtime.aws_ecs_task_definition.traefik
}

moved {
  from = aws_ecs_service.rise
  to   = module.runtime.aws_ecs_service.rise
}

moved {
  from = aws_ecs_service.traefik
  to   = module.runtime.aws_ecs_service.traefik
}

moved {
  from = aws_service_discovery_service.rise
  to   = module.runtime.aws_service_discovery_service.rise
}

moved {
  from = aws_service_discovery_service.traefik
  to   = module.runtime.aws_service_discovery_service.traefik
}
