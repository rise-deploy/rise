# Every create-or-bring decision resolves here, and nowhere else. No resource in
# this module refers to aws_vpc.this[0] or aws_ecs_cluster.this[0] directly —
# they all go through these locals, which is what keeps "create it" and "use
# mine" from forking the rest of the module.

locals {
  region     = data.aws_region.current.region
  account_id = data.aws_caller_identity.current.account_id
  partition  = data.aws_partition.current.partition

  name = var.name
  tags = merge({
    "app.kubernetes.io/managed-by" = "terraform"
    "rise.dev/module"              = "rise-ecs"
  }, var.tags)

  azs = slice(data.aws_availability_zones.available.names, 0, var.availability_zone_count)

  # --- VPC ------------------------------------------------------------------
  create_vpc = var.vpc == null
  vpc_id     = local.create_vpc ? aws_vpc.this[0].id : var.vpc.id

  public_subnet_ids  = local.create_vpc ? [for s in aws_subnet.public : s.id] : var.vpc.public_subnet_ids
  private_subnet_ids = local.create_vpc ? [for s in aws_subnet.private : s.id] : var.vpc.private_subnet_ids
  database_subnet_ids = local.create_vpc ? [for s in aws_subnet.database : s.id] : (
    length(var.vpc.database_subnet_ids) > 0 ? var.vpc.database_subnet_ids : var.vpc.private_subnet_ids
  )

  # Keyed by something statically known in both modes -- AZ name when the module
  # creates the subnets, subnet id when they are brought. for_each cannot take
  # keys that are only known after apply, and in create mode the ids are exactly
  # that.
  private_subnets_by_key = local.create_vpc ? {
    for az in local.azs : az => aws_subnet.private[az].id
    } : {
    for id in var.vpc.private_subnet_ids : id => id
  }

  nat_gateway_count = local.create_vpc ? (
    var.nat_gateway_mode == "per_az" ? var.availability_zone_count :
    var.nat_gateway_mode == "single" ? 1 : 0
  ) : 0

  # --- Cluster --------------------------------------------------------------
  create_cluster = var.cluster == null
  cluster_name   = local.create_cluster ? aws_ecs_cluster.this[0].name : var.cluster.name
  cluster_arn    = local.create_cluster ? aws_ecs_cluster.this[0].arn : data.aws_ecs_cluster.brought[0].arn

  # --- Cloud Map ------------------------------------------------------------
  create_namespace = var.cloud_map_namespace_id == null
  namespace_name   = coalesce(var.cloud_map_namespace_name, "${local.name}.internal")
  namespace_id     = local.create_namespace ? aws_service_discovery_private_dns_namespace.this[0].id : var.cloud_map_namespace_id

  # The two internal URLs the install turns on. auth_backend_url must be
  # reachable from inside the cluster — Traefik calls it for every forwardAuth
  # subrequest — and must never be the public URL. traefik_api_url is where
  # readiness comes from: serverStatus, with no fallback, so a project with a
  # health_check never becomes Healthy without it.
  auth_backend_url = "http://rise.${local.namespace_name}:3000"
  traefik_api_url  = "http://traefik.${local.namespace_name}:8080"

  # --- Edge -----------------------------------------------------------------
  acme_enabled       = var.edge_mode == "nlb-traefik-acme"
  traefik_entrypoint = local.acme_enabled ? "websecure" : "web"
  ingress_scheme     = "https"
  control_plane_host = coalesce(var.control_plane_host, "rise.${var.ingress_domain}")
  public_url         = "${local.ingress_scheme}://${local.control_plane_host}"

  # --- Database -------------------------------------------------------------
  create_database = var.database_url_secret_arn == null
  database_url_secret_arn = local.create_database ? (
    aws_secretsmanager_secret.database_url[0].arn
  ) : var.database_url_secret_arn

  # --- Identity -------------------------------------------------------------
  oidc_issuer = var.deploy_dex ? "https://dex.${var.ingress_domain}/dex" : var.oidc_issuer

  workload_task_role_arn = coalesce(var.workload_task_role_arn, var.controller_role_arn)
  traefik_task_role_arn  = coalesce(var.traefik_task_role_arn, try(aws_iam_role.traefik[0].arn, null))

  log_group_name = coalesce(var.log_group_name, "/${local.name}")
  rise_image_ref = var.rise_image_ref != null ? var.rise_image_ref : (
    var.rise_image_tag != null ? "${var.rise_image}:${var.rise_image_tag}" : ""
  )

  # --- Control-plane environment -------------------------------------------
  # Built by modules/control-plane-env, which the e2e test root also consumes so
  # the two cannot drift on the one contract that matters: what the control
  # plane's environment and Traefik labels look like on ECS.
  rise_environment = module.control_plane_env.environment
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
  security_group_ids     = [aws_security_group.apps.id]
  assign_public_ip       = false
  execution_role_arn     = var.execution_role_arn
  workload_task_role_arn = local.workload_task_role_arn
  log_group_name         = aws_cloudwatch_log_group.this.name

  auth_backend_url     = local.auth_backend_url
  traefik_api_url      = local.traefik_api_url
  traefik_entrypoint   = local.traefik_entrypoint
  traefik_certresolver = local.acme_enabled ? "letsencrypt" : null

  oidc_issuer                = local.oidc_issuer
  oidc_client_id             = var.oidc_client_id
  oidc_group_claim           = var.oidc_group_claim
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
    } : {
    type         = "oci-client-auth"
    registry_url = var.oci_registry_url
    namespace    = var.oci_registry_namespace
  }

  repository_credentials_secret_arn = var.repository_credentials_secret_arn
}
