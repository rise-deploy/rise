locals {
  region     = data.aws_region.current.region
  account_id = data.aws_caller_identity.current.account_id
  partition  = data.aws_partition.current.partition

  name = var.name
  tags = merge({
    "app.kubernetes.io/managed-by" = "terraform"
    "rise.dev/module"              = "rise-ecs"
  }, var.tags)

  create_vpc          = var.vpc == null
  vpc_id              = module.network.vpc.id
  public_subnet_ids   = module.network.vpc.public_subnet_ids
  private_subnet_ids  = module.network.vpc.private_subnet_ids
  database_subnet_ids = module.network.vpc.database_subnet_ids
  cluster_name        = module.cluster.cluster.name
  cluster_arn         = module.cluster.cluster.arn
  namespace_name      = coalesce(var.cloud_map_namespace_name, "${local.name}.internal")
  namespace_id        = module.cluster.discovery.namespace_id

  control_plane_discovery_name = "${local.name}-control-plane"
  traefik_discovery_name       = "${local.name}-traefik"
  dex_discovery_name           = "${local.name}-dex"

  # The two internal URLs the install turns on. auth_backend_url must be
  # reachable from inside the cluster — Traefik calls it for every forwardAuth
  # subrequest — and must never be the public URL. traefik_api_url is where
  # readiness comes from: serverStatus, with no fallback, so a project with a
  # health_check never becomes Healthy without it.
  auth_backend_url = "http://${local.control_plane_discovery_name}.${local.namespace_name}:3000"
  traefik_api_url  = "http://${local.traefik_discovery_name}.${local.namespace_name}:8080"

  # --- Edge -----------------------------------------------------------------
  acme_enabled       = var.edge_mode == "nlb-traefik-acme"
  traefik_entrypoint = local.acme_enabled ? "websecure" : "web"
  ingress_scheme     = "https"
  control_plane_host = coalesce(var.control_plane_host, "rise.${var.ingress_domain}")
  public_url         = "${local.ingress_scheme}://${local.control_plane_host}"

  # --- Database -------------------------------------------------------------
  create_database = var.database_url_secret_arn == null
  database_url_secret_arn = local.create_database ? (
    module.database.url_secret_arn
  ) : var.database_url_secret_arn

  oidc_client_secret = coalesce(var.oidc_client_secret, "rise-backend-secret")

  # --- Identity -------------------------------------------------------------
  oidc_issuer = var.deploy_dex ? "https://dex.${var.ingress_domain}/dex" : var.oidc_issuer

  workload_task_role_arn = coalesce(var.workload_task_role_arn, var.controller_role_arn)
  create_dns_records     = var.route53_zone != null
  create_traefik_task_role = coalesce(
    var.create_traefik_task_role,
    var.traefik_task_role_arn == null,
  )
  traefik_task_role_arn = module.ingress.traefik_role_arn

  log_group_name = coalesce(var.log_group_name, "/${local.name}")
  rise_image_ref = var.rise_image_ref != null ? var.rise_image_ref : (
    var.rise_image_tag != null ? "${var.rise_image}:${var.rise_image_tag}" : ""
  )

  # --- Control-plane environment -------------------------------------------
  # Both runtime callers compose the environment and labels through this module.
  rise_environment = module.control_plane_env.environment

  control_plane_builtin_secret_environment = merge({
    DATABASE_URL            = local.database_url_secret_arn
    RISE_JWT_SIGNING_SECRET = module.secrets.environment.RISE_JWT_SIGNING_SECRET
    RISE_ENCRYPTION_KEY     = module.secrets.environment.RISE_ENCRYPTION_KEY
    OIDC_CLIENT_SECRET      = module.secrets.environment.OIDC_CLIENT_SECRET
    }, var.control_plane_local_config_secret_arn == null ? {} : {
    RISE_LOCAL_CONFIG_YAML = var.control_plane_local_config_secret_arn
  })
  control_plane_secret_environment = merge(
    local.control_plane_builtin_secret_environment,
    var.control_plane_secret_environment,
  )
  control_plane_secrets = [
    for name, value_from in local.control_plane_secret_environment : {
      name      = name
      valueFrom = value_from
    }
  ]
  control_plane_additional_secret_arns = distinct([
    for value_from in values(var.control_plane_secret_environment) :
    join(":", slice(split(":", value_from), 0, 7))
  ])
}

module "control_plane_env" {
  source = "./modules/control-plane-env"

  ingress_domain     = var.ingress_domain
  control_plane_host = local.control_plane_host
  ingress_scheme     = local.ingress_scheme
  region             = local.region
  admin_email        = var.admin_email

  cluster_name           = local.cluster_name
  subnet_ids             = local.private_subnet_ids
  security_group_ids     = [module.security.groups.apps]
  assign_public_ip       = false
  execution_role_arn     = var.execution_role_arn
  workload_task_role_arn = local.workload_task_role_arn
  log_group_name         = module.cluster.logging.name
  log_retention_days     = var.log_retention_days

  auth_backend_url     = local.auth_backend_url
  traefik_api_url      = local.traefik_api_url
  traefik_entrypoint   = local.traefik_entrypoint
  traefik_certresolver = local.acme_enabled ? "letsencrypt" : null

  oidc_issuer                = local.oidc_issuer
  oidc_client_id             = var.oidc_client_id
  oidc_group_claim           = var.oidc_group_claim
  idp_group_sync_prefixes    = var.idp_group_sync_prefixes
  admin_idp_group            = var.admin_idp_group
  platform_access_policy     = var.platform_access_policy
  platform_allowed_idp_group = var.platform_allowed_idp_group
  # The issuer is public here (Traefik-fronted, or an operator's own IdP), so
  # the SSRF defaults stay closed.
  allow_private_ssrf = false

  resource_prefix         = var.resource_prefix
  controller_class_name   = var.controller_class_name
  ssm_parameter_prefix    = var.ssm_parameter_prefix
  ssm_kms_key_arn         = var.ssm_kms_key_arn
  cpu_architecture        = var.cpu_architecture
  reconcile_interval_secs = var.reconcile_interval_secs
  max_replicas            = var.max_replicas

  registry = var.registry_type == "ecr" ? {
    type = "ecr"
    # Never a variable: the backend asserts at startup that the registry account
    # equals the ECS credentials' account, because Rise writes no ECR repository
    # policy and cross-account pulls cannot work.
    account_id    = local.account_id
    push_role_arn = var.ecr_push_role_arn
    repo_prefix   = var.ecr_repo_prefix
    auto_remove   = var.ecr_auto_remove
    registry_host = var.ecr_registry_host
    } : {
    type         = "oci-client-auth"
    registry_url = var.oci_registry_url
    namespace    = var.oci_registry_namespace
  }

  repository_credentials_secret_arn = var.repository_credentials_secret_arn
}
