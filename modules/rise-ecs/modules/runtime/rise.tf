resource "aws_ecs_task_definition" "rise" {
  family                   = var.control_plane.name
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = var.control_plane.cpu
  memory                   = var.control_plane.memory
  execution_role_arn       = var.roles.execution
  task_role_arn            = var.roles.control_plane

  runtime_platform {
    cpu_architecture        = var.cpu_architecture
    operating_system_family = "LINUX"
  }

  container_definitions = jsonencode([
    {
      name      = "rise"
      image     = var.control_plane.image
      essential = true
      command   = ["backend", "server"]

      portMappings = [
        { containerPort = 3000 },
        { containerPort = 3001 },
      ]

      environment = [
        for k, v in var.environment : { name = k, value = tostring(v) }
      ]

      # Resolved by ECS at task start under the execution role, so none of these
      # appear in a DescribeTaskDefinition response.
      secrets = [for name, value_from in var.secret_environment : { name = name, valueFrom = value_from }]

      healthCheck = {
        # `rise backend health` rather than curl: an ECS health check runs
        # inside the container, and the binary is already there. See
        # `rise backend health --help`.
        command     = ["CMD", "rise", "backend", "health"]
        interval    = var.control_plane.health_check.interval
        timeout     = var.control_plane.health_check.timeout
        retries     = var.control_plane.health_check.retries
        startPeriod = var.control_plane.health_check.start_period
      }

      # Routing for the control plane itself, from the shared submodule.
      dockerLabels = var.control_plane.labels

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = var.logging.group_name
          "awslogs-region"        = var.logging.region
          "awslogs-stream-prefix" = var.control_plane.log_stream_prefix
        }
      }
    }
  ])

  tags = var.tags
}

resource "aws_ecs_service" "rise" {
  name            = var.control_plane.name
  cluster         = var.cluster.arn
  task_definition = aws_ecs_task_definition.rise.arn
  launch_type     = "FARGATE"

  # Safe above one: the reconcile loop runs under a leader election held in
  # Postgres (rise-runtime-sync), so replicas serve the API and only one
  # reconciles.
  desired_count = var.control_plane.desired_count

  enable_execute_command = var.control_plane.enable_execute_command

  network_configuration {
    subnets          = var.network.control_plane.subnet_ids
    security_groups  = var.network.control_plane.security_group_ids
    assign_public_ip = var.network.control_plane.assign_public_ip
  }

  service_registries {
    registry_arn = aws_service_discovery_service.rise.arn
  }

  # A control plane that cannot start should roll back rather than sit in a
  # restart loop while the previous version is already gone.
  deployment_circuit_breaker {
    enable   = true
    rollback = true
  }

  propagate_tags = "SERVICE"
  tags           = var.tags

  lifecycle {
    # Cloud Map derives service ids from the namespace and name, so recreating a
    # service can produce the same ARN after deregistering every task. Replace
    # the ECS service with it so ECS registers the new tasks against that ARN.
    replace_triggered_by = [aws_service_discovery_service.rise]

    precondition {
      condition     = length(var.network.control_plane.subnet_ids) <= 16
      error_message = "At most 16 subnets: an awsvpc network configuration accepts no more, and the backend rejects the setting at startup."
    }

  }

  depends_on = [aws_ecs_service.traefik]
}
