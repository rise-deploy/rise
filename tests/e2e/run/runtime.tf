module "runtime" {
  source = "../../../modules/rise-ecs/modules/runtime"

  cluster = {
    name = local.env.cluster_name
    arn  = local.env.cluster_arn
  }
  discovery = {
    namespace_id   = local.env.cloud_map_namespace_id
    namespace_name = local.env.cloud_map_namespace_name
  }
  network = {
    control_plane = {
      subnet_ids         = local.env.subnet_ids
      security_group_ids = [aws_security_group.internal.id]
      assign_public_ip   = true
    }
    traefik = {
      subnet_ids         = local.env.subnet_ids
      security_group_ids = [aws_security_group.edge.id]
      assign_public_ip   = true
    }
  }
  roles = {
    execution     = local.env.execution_role_arn
    control_plane = local.env.controller_role_arn
    traefik       = local.env.traefik_task_role_arn
  }
  logging = {
    group_name = local.env.log_group_name
    region     = var.region
  }
  control_plane = {
    name              = "${var.name}-${var.scope}-rise"
    discovery_name    = "rise-${var.scope}"
    image             = "${var.rise_image}:${var.rise_image_tag}"
    log_stream_prefix = "rise-${var.scope}"
    labels            = module.control_plane_env.docker_labels
    health_check = {
      interval = 15
      retries  = 5
    }
  }
  environment = merge(module.control_plane_env.environment, {
    DATABASE_URL            = local.database_url
    RISE_JWT_SIGNING_SECRET = var.jwt_signing_secret
    RISE_ENCRYPTION_KEY     = var.encryption_key
    OIDC_CLIENT_SECRET      = "rise-backend-secret"
  })
  traefik = {
    name              = "${var.name}-${var.scope}-traefik"
    discovery_name    = "traefik-${var.scope}"
    image             = var.traefik_image
    log_stream_prefix = "traefik-${var.scope}"
    refresh_seconds   = 5
    constraints       = "Label(`rise.dev/controller-class`, `${local.controller_class}`)"
  }
  cpu_architecture = var.cpu_architecture
  tags             = local.tags

  depends_on = [aws_ecs_service.postgres]
}
