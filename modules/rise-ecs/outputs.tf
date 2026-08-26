# -----------------------------------------------------------------------------
# Network
# -----------------------------------------------------------------------------

output "vpc_id" {
  description = "VPC the install runs in, created or brought."
  value       = local.vpc_id
}

output "public_subnet_ids" {
  description = "Public subnets (load balancer, NAT)."
  value       = local.public_subnet_ids
}

output "private_subnet_ids" {
  description = "Private subnets. Deployed workloads land here."
  value       = local.private_subnet_ids
}

output "database_subnet_ids" {
  description = "Database subnets."
  value       = local.database_subnet_ids
}

output "apps_security_group_id" {
  description = "Security group deployed workloads run in — the value of deployment_controller.security_groups."
  value       = aws_security_group.apps.id
}

# -----------------------------------------------------------------------------
# Runtime
# -----------------------------------------------------------------------------

output "cluster_name" {
  description = "ECS cluster name."
  value       = local.cluster_name
}

output "cluster_arn" {
  description = "ECS cluster ARN."
  value       = local.cluster_arn
}

output "cloud_map_namespace_name" {
  description = "Private DNS namespace backing the internal service URLs."
  value       = local.namespace_name
}

output "log_group_name" {
  description = "CloudWatch log group carrying control-plane and workload logs."
  value       = aws_cloudwatch_log_group.this.name
}

output "rise_service_name" {
  description = "ECS service running the control plane."
  value       = aws_ecs_service.rise.name
}

output "traefik_service_name" {
  description = "ECS service running the ingress router."
  value       = aws_ecs_service.traefik.name
}

# -----------------------------------------------------------------------------
# Reaching the install
# -----------------------------------------------------------------------------

output "load_balancer_dns_name" {
  description = "DNS name of the edge load balancer."
  value       = aws_lb.this.dns_name
}

output "public_url" {
  description = "Rise's public URL."
  value       = local.public_url
}

output "dns_records_required" {
  description = <<-EOT
    DNS records to create, unless route53_zone_id was set and the module made
    them. The wildcard is required, not optional: projects are served at
    <project>.<domain>, and groups and environments add another label.
  EOT
  value = var.route53_zone_id != null ? [] : [
    "${var.ingress_domain}    ALIAS/CNAME -> ${aws_lb.this.dns_name}",
    "*.${var.ingress_domain}  ALIAS/CNAME -> ${aws_lb.this.dns_name}",
  ]
}

# -----------------------------------------------------------------------------
# Configuration
# -----------------------------------------------------------------------------

output "rise_task_environment" {
  description = <<-EOT
    The plain environment variables the control plane needs, exactly as this
    module sets them on its own task definition. Useful when running Rise
    somewhere else — the two cannot drift, because the module consumes this same
    map. Contains no secrets; those are in `rise_task_secrets`.
  EOT
  value       = local.rise_environment
}

output "rise_task_secrets" {
  description = "Secrets Manager ARNs to inject as environment variables, keyed by variable name."
  value = {
    DATABASE_URL            = local.database_url_secret_arn
    RISE_JWT_SIGNING_SECRET = aws_secretsmanager_secret.jwt_signing_secret.arn
    RISE_ENCRYPTION_KEY     = aws_secretsmanager_secret.encryption_key.arn
    OIDC_CLIENT_SECRET      = aws_secretsmanager_secret.oidc_client_secret.arn
  }
}

output "secret_arns_for_execution_role" {
  description = <<-EOT
    Pass to modules/rise-aws's `ecs_secret_arns` so the task execution role may
    read them. Secrets Manager appends a random suffix to every ARN, so these
    are exact ARNs rather than patterns.
  EOT
  value = compact([
    local.database_url_secret_arn,
    aws_secretsmanager_secret.jwt_signing_secret.arn,
    aws_secretsmanager_secret.encryption_key.arn,
    aws_secretsmanager_secret.oidc_client_secret.arn,
    var.repository_credentials_secret_arn,
  ])
}

output "database_endpoint" {
  description = "Control-plane database endpoint, when the module created one."
  value       = local.create_database ? aws_db_instance.this[0].endpoint : null
}

output "rise_config" {
  description = "Aggregate view of what this module configured, mirroring modules/rise-aws's rise_config."
  sensitive   = true
  value = {
    ecs = {
      region                  = local.region
      cluster                 = local.cluster_name
      subnets                 = local.private_subnet_ids
      security_groups         = [aws_security_group.apps.id]
      assign_public_ip        = false
      execution_role_arn      = var.execution_role_arn
      task_role_arn           = local.workload_task_role_arn
      log_group               = aws_cloudwatch_log_group.this.name
      resource_prefix         = var.resource_prefix
      ssm_parameter_prefix    = var.ssm_parameter_prefix
      ssm_kms_key_id          = var.ssm_kms_key_arn
      cpu_architecture        = var.cpu_architecture
      auth_backend_url        = local.auth_backend_url
      traefik_api_url         = local.traefik_api_url
      traefik_entrypoint      = local.traefik_entrypoint
      traefik_certresolver    = local.acme_enabled ? "letsencrypt" : null
      reconcile_interval_secs = var.reconcile_interval_secs
    }
    ingress = {
      domain     = var.ingress_domain
      scheme     = local.ingress_scheme
      public_url = local.public_url
    }
    auth = {
      issuer    = local.oidc_issuer
      client_id = var.oidc_client_id
    }
  }
}
