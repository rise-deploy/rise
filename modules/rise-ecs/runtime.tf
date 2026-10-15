module "runtime" {
  source = "./modules/runtime"

  cluster = {
    name = local.cluster_name
    arn  = local.cluster_arn
  }
  discovery = {
    namespace_id   = local.namespace_id
    namespace_name = local.namespace_name
  }
  network = {
    control_plane = {
      subnet_ids         = local.private_subnet_ids
      security_group_ids = [module.security.groups.control_plane]
    }
    traefik = {
      subnet_ids         = local.private_subnet_ids
      security_group_ids = [module.security.groups.traefik]
    }
  }
  roles = {
    execution     = var.execution_role_arn
    control_plane = var.controller_role_arn
    traefik       = local.traefik_task_role_arn
  }
  logging = {
    group_name = module.cluster.logging.name
    region     = local.region
  }
  control_plane = {
    name                   = "${local.name}-control-plane"
    discovery_name         = local.control_plane_discovery_name
    image                  = local.rise_image_ref
    cpu                    = var.rise_cpu
    memory                 = var.rise_memory
    desired_count          = var.rise_desired_count
    enable_execute_command = var.enable_execute_command
    labels                 = module.control_plane_env.docker_labels
  }
  environment        = local.rise_environment
  secret_environment = local.control_plane_secret_environment
  traefik = {
    name            = "${local.name}-traefik"
    discovery_name  = local.traefik_discovery_name
    image           = var.traefik_image
    refresh_seconds = var.traefik_refresh_seconds
    constraints     = var.traefik_constraints
  }
  acme = {
    enabled         = local.acme_enabled
    email           = var.acme_email
    file_system_id  = module.ingress.acme.file_system_id
    access_point_id = module.ingress.acme.access_point_id
  }
  load_balancer_targets = module.ingress.target_groups
  cpu_architecture      = var.cpu_architecture
  tags                  = local.tags
}
