locals {
  public_url = "${var.ingress_scheme}://rise.${var.ingress_domain}"

  environment = merge(
    {
      RISE_CONFIG_RUN_MODE = "ecs"
      PUBLIC_URL           = local.public_url
      # Follows the scheme: a Secure cookie is never sent over plain HTTP, so
      # hardcoding it true would break login on an http install.
      RISE_COOKIE_SECURE  = tostring(var.ingress_scheme == "https")
      RISE_INGRESS_DOMAIN = var.ingress_domain
      RISE_INGRESS_SCHEME = var.ingress_scheme
      AWS_REGION          = var.region
      ADMIN_EMAIL         = var.admin_email

      DEX_ISSUER     = var.oidc_issuer
      OIDC_CLIENT_ID = var.oidc_client_id

      RISE_SSRF_ALLOW_PRIVATE = tostring(var.allow_private_ssrf)
      RISE_SSRF_ALLOW_HTTP    = tostring(var.allow_private_ssrf)

      RISE_AUTH_BACKEND_URL   = var.auth_backend_url
      RISE_TRAEFIK_API_URL    = var.traefik_api_url
      RISE_TRAEFIK_ENTRYPOINT = var.traefik_entrypoint
      RISE_CERTRESOLVER       = var.traefik_certresolver != null ? var.traefik_certresolver : ""

      RISE_ECS_CLUSTER = var.cluster_name
      # Comma-joined on purpose: the settings loader accepts a YAML list or a
      # comma-separated string precisely so a Terraform output can feed it
      # through a single environment variable.
      RISE_ECS_SUBNETS                 = join(",", var.subnet_ids)
      RISE_ECS_SECURITY_GROUPS         = join(",", var.security_group_ids)
      RISE_ECS_ASSIGN_PUBLIC_IP        = tostring(var.assign_public_ip)
      RISE_ECS_EXECUTION_ROLE_ARN      = var.execution_role_arn
      RISE_ECS_TASK_ROLE_ARN           = var.workload_task_role_arn
      RISE_ECS_LOG_GROUP               = var.log_group_name
      RISE_ECS_RESOURCE_PREFIX         = var.resource_prefix
      RISE_ECS_SSM_PREFIX              = var.ssm_parameter_prefix
      RISE_ECS_SSM_KMS_KEY_ID          = var.ssm_kms_key_arn != null ? var.ssm_kms_key_arn : ""
      RISE_ECS_CPU_ARCHITECTURE        = var.cpu_architecture
      RISE_ECS_RECONCILE_INTERVAL_SECS = tostring(var.reconcile_interval_secs)
      RISE_CONTROLLER_CLASS_NAME       = var.controller_class_name

      RISE_MAX_REPLICAS  = tostring(var.max_replicas)
      RISE_REGISTRY_TYPE = var.registry.type
    },
    var.repository_credentials_secret_arn != null ? {
      RISE_ECS_REPOSITORY_CREDENTIALS_SECRET_ARN = var.repository_credentials_secret_arn
    } : {},
    var.registry.type == "ecr" ? {
      RISE_ECR_ACCOUNT_ID    = var.registry.account_id
      RISE_ECR_PUSH_ROLE_ARN = var.registry.push_role_arn
      RISE_ECR_REPO_PREFIX   = var.registry.repo_prefix
      RISE_ECR_AUTO_REMOVE   = tostring(var.registry.auto_remove)
    } : {},
    var.registry.type == "oci-client-auth" ? {
      RISE_REGISTRY_URL       = var.registry.registry_url
      RISE_REGISTRY_NAMESPACE = var.registry.namespace
    } : {}
  )

  # Traefik reads routing from the container definition's dockerLabels -- the
  # only place its ECS provider looks.
  tls_labels = var.traefik_certresolver != null ? {
    "traefik.http.routers.rise-cp.tls.certresolver"      = var.traefik_certresolver
    "traefik.http.routers.rise-dotrise.tls.certresolver" = var.traefik_certresolver
  } : {}

  docker_labels = merge({
    # Traefik's constraint applies to every container it considers, the control
    # plane included. Without this label an install that confines its Traefik to
    # one class -- the whole point of sharing a cluster -- makes Rise itself
    # invisible to the proxy that is meant to publish it.
    "${var.label_namespace}/controller-class" = var.controller_class_name

    "traefik.enable"                                         = "true"
    "traefik.http.routers.rise-cp.rule"                      = "Host(`rise.${var.ingress_domain}`)"
    "traefik.http.routers.rise-cp.entrypoints"               = var.traefik_entrypoint
    "traefik.http.routers.rise-cp.service"                   = "rise-cp"
    "traefik.http.services.rise-cp.loadbalancer.server.port" = "3000"

    # The OAuth and ingress-auth endpoints must answer on every project
    # hostname, not just rise.<domain>, because Traefik redirects there from
    # whichever host the user was on. The high priority keeps it ahead of the
    # project routers matching the same host.
    "traefik.http.routers.rise-dotrise.rule"        = "PathPrefix(`/.rise`)"
    "traefik.http.routers.rise-dotrise.entrypoints" = var.traefik_entrypoint
    "traefik.http.routers.rise-dotrise.priority"    = "1000"
    "traefik.http.routers.rise-dotrise.service"     = "rise-cp"
  }, local.tls_labels)
}
