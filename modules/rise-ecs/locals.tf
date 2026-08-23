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
  public_url         = "https://rise.${var.ingress_domain}"

  # --- Database -------------------------------------------------------------
  create_database = var.database_url_secret_arn == null
  database_url_secret_arn = local.create_database ? (
    aws_secretsmanager_secret.database_url[0].arn
  ) : var.database_url_secret_arn

  # --- Identity -------------------------------------------------------------
  oidc_issuer = var.deploy_dex ? "https://dex.${var.ingress_domain}/dex" : var.oidc_issuer

  workload_task_role_arn = coalesce(var.workload_task_role_arn, var.controller_role_arn)

  log_group_name = "/${local.name}"

  # --- Control-plane environment -------------------------------------------
  # The module's whole job on the Rise side: the shipped config/ecs.yaml is
  # selected with RISE_CONFIG_RUN_MODE=ecs and every value below fills one of
  # its ${...} interpolations. Exposed as an output too, so an operator running
  # the control plane outside this module gets the same map rather than
  # reconstructing it.
  rise_environment = merge(
    {
      RISE_CONFIG_RUN_MODE = "ecs"
      PUBLIC_URL           = local.public_url
      RISE_COOKIE_SECURE   = "true"
      RISE_INGRESS_DOMAIN  = var.ingress_domain
      RISE_INGRESS_SCHEME  = local.ingress_scheme
      AWS_REGION           = local.region
      ADMIN_EMAIL          = var.admin_email

      DEX_ISSUER     = local.oidc_issuer
      OIDC_CLIENT_ID = var.oidc_client_id

      RISE_AUTH_BACKEND_URL   = local.auth_backend_url
      RISE_TRAEFIK_API_URL    = local.traefik_api_url
      RISE_TRAEFIK_ENTRYPOINT = local.traefik_entrypoint
      RISE_CERTRESOLVER       = local.acme_enabled ? "letsencrypt" : ""

      RISE_ECS_CLUSTER = local.cluster_name
      # Comma-joined on purpose: the settings loader accepts a list or a
      # comma-separated string precisely so a Terraform output can feed it
      # through a single environment variable.
      RISE_ECS_SUBNETS                 = join(",", local.private_subnet_ids)
      RISE_ECS_SECURITY_GROUPS         = aws_security_group.apps.id
      RISE_ECS_ASSIGN_PUBLIC_IP        = "false"
      RISE_ECS_EXECUTION_ROLE_ARN      = var.execution_role_arn
      RISE_ECS_TASK_ROLE_ARN           = local.workload_task_role_arn
      RISE_ECS_LOG_GROUP               = local.log_group_name
      RISE_ECS_RESOURCE_PREFIX         = var.resource_prefix
      RISE_ECS_SSM_PREFIX              = var.ssm_parameter_prefix
      RISE_ECS_SSM_KMS_KEY_ID          = var.ssm_kms_key_arn != null ? var.ssm_kms_key_arn : ""
      RISE_ECS_CPU_ARCHITECTURE        = var.cpu_architecture
      RISE_ECS_RECONCILE_INTERVAL_SECS = tostring(var.reconcile_interval_secs)

      RISE_MAX_REPLICAS  = tostring(var.max_replicas)
      RISE_REGISTRY_TYPE = var.registry_type
    },
    var.repository_credentials_secret_arn != null ? {
      RISE_ECS_REPOSITORY_CREDENTIALS_SECRET_ARN = var.repository_credentials_secret_arn
    } : {},
    var.registry_type == "ecr" ? {
      # Never a variable: the backend asserts at startup that the registry
      # account equals the ECS credentials' account, because Rise writes no ECR
      # repository policy and cross-account pulls cannot work. Making this
      # settable could only produce an install that refuses to start.
      RISE_ECR_ACCOUNT_ID    = local.account_id
      RISE_ECR_PUSH_ROLE_ARN = var.ecr_push_role_arn
      RISE_ECR_REPO_PREFIX   = var.ecr_repo_prefix
      RISE_ECR_AUTO_REMOVE   = tostring(var.ecr_auto_remove)
    } : {},
    var.registry_type == "oci-client-auth" ? {
      RISE_REGISTRY_URL       = var.oci_registry_url
      RISE_REGISTRY_NAMESPACE = var.oci_registry_namespace
    } : {}
  )
}
