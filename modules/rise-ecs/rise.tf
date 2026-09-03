# The control plane. It runs the config/ecs.yaml that ships in the image,
# selected with RISE_CONFIG_RUN_MODE=ecs, so this module's job is to fill that
# file's ${...} interpolations rather than to write configuration of its own.
# The environment map is built in locals.tf and exported as an output, so an
# operator running Rise elsewhere gets the same values.

resource "aws_ecs_task_definition" "rise" {
  family                   = "${local.name}-control-plane"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = var.rise_cpu
  memory                   = var.rise_memory
  execution_role_arn       = var.execution_role_arn
  task_role_arn            = var.controller_role_arn

  runtime_platform {
    cpu_architecture        = var.cpu_architecture
    operating_system_family = "LINUX"
  }

  container_definitions = jsonencode([
    {
      name      = "rise"
      image     = local.rise_image_ref
      essential = true
      command   = ["backend", "server"]

      portMappings = [
        { containerPort = 3000 },
        { containerPort = 3001 },
      ]

      environment = [
        for k, v in local.rise_environment : { name = k, value = tostring(v) }
      ]

      # Resolved by ECS at task start under the execution role, so none of these
      # appear in a DescribeTaskDefinition response.
      secrets = local.control_plane_secrets

      healthCheck = {
        # `rise backend health` rather than curl: an ECS health check runs
        # inside the container, and the binary is already there. See
        # `rise backend health --help`.
        command     = ["CMD", "rise", "backend", "health"]
        interval    = 30
        timeout     = 5
        retries     = 3
        startPeriod = 60
      }

      # Routing for the control plane itself, from the shared submodule.
      dockerLabels = module.control_plane_env.docker_labels

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.this.name
          "awslogs-region"        = local.region
          "awslogs-stream-prefix" = "rise"
        }
      }
    }
  ])

  tags = local.tags

  depends_on = [
    aws_secretsmanager_secret_version.database_url,
    aws_secretsmanager_secret_version.jwt_signing_secret,
    aws_secretsmanager_secret_version.encryption_key,
    aws_secretsmanager_secret_version.oidc_client_secret,
  ]
}

resource "aws_ecs_service" "rise" {
  name            = "${local.name}-control-plane"
  cluster         = local.cluster_arn
  task_definition = aws_ecs_task_definition.rise.arn
  launch_type     = "FARGATE"

  # Safe above one: the reconcile loop runs under a leader election held in
  # Postgres (rise-runtime-sync), so replicas serve the API and only one
  # reconciles.
  desired_count = var.rise_desired_count

  enable_execute_command = var.enable_execute_command

  network_configuration {
    subnets          = local.private_subnet_ids
    security_groups  = [aws_security_group.control_plane.id]
    assign_public_ip = false
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
  tags           = local.tags

  lifecycle {
    # Cloud Map derives service ids from the namespace and name, so recreating a
    # service can produce the same ARN after deregistering every task. Replace
    # the ECS service with it so ECS registers the new tasks against that ARN.
    replace_triggered_by = [aws_service_discovery_service.rise]

    precondition {
      condition     = length(local.private_subnet_ids) <= 16
      error_message = "At most 16 subnets: an awsvpc network configuration accepts no more, and the backend rejects the setting at startup."
    }

    precondition {
      condition     = (var.rise_image_ref == null) != (var.rise_image_tag == null)
      error_message = "Set exactly one of rise_image_ref or rise_image_tag."
    }
  }

  depends_on = [aws_ecs_service.traefik]
}
