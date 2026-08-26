# Read by the per-run apply through terraform_remote_state, and by the harness
# through `terraform output -json`.

output "region" {
  value = var.region
}

output "cluster_name" {
  value = aws_ecs_cluster.this.name
}

output "cluster_arn" {
  value = aws_ecs_cluster.this.arn
}

output "subnet_ids" {
  description = "Public subnets. Every task in this environment runs here with a public IP."
  value       = [for s in aws_subnet.public : s.id]
}

output "cloud_map_namespace_id" {
  value = aws_service_discovery_private_dns_namespace.this.id
}

output "cloud_map_namespace_name" {
  value = aws_service_discovery_private_dns_namespace.this.name
}

output "log_group_name" {
  value = aws_cloudwatch_log_group.this.name
}

output "traefik_task_role_arn" {
  description = "Pre-created here because the per-run identity cannot write IAM."
  value       = aws_iam_role.traefik.arn
}

output "vpc_id" {
  value = aws_vpc.this.id
}

output "controller_role_arn" {
  value = module.rise_aws.role_arn
}

output "execution_role_arn" {
  value = module.rise_aws.ecs_execution_role_arn
}

output "ecr_push_role_arn" {
  value = module.rise_aws.push_role_arn
}

output "ecr_repo_prefix" {
  description = "rise-aws derives this from `name`; the per-run apply must give Rise the same value."
  value       = "${var.name}/"
}

output "dns_zone_id" {
  value = aws_route53_zone.this.zone_id
}

output "dns_name_servers" {
  description = <<-EOT
    Delegate the domain to these, once, at the registrar. Nothing under the
    zone resolves until the domain's nameservers match them -- including for a
    domain registered through Route 53 Domains, which is delegated to the zone
    *it* created rather than to this one.
  EOT
  value       = aws_route53_zone.this.name_servers
}

output "dns_zone_name" {
  value = aws_route53_zone.this.name
}

output "state_bucket" {
  description = "Where the per-run apply keeps its state."
  value       = aws_s3_bucket.state.id
}

output "ci_role_arn" {
  description = "Role the GitHub Actions workflow assumes."
  value       = aws_iam_role.ci.arn
}

# -----------------------------------------------------------------------------
# The harness's entry point into all of the above.
#
# It needs several of these values *before* the per-run apply -- the zone to
# scope DNS under, the bucket holding remote state -- so it cannot get them from
# the per-run outputs, and reading bootstrap's own state would mean either a
# second backend or a pile of environment variables. One parameter keeps the
# contract to: know the environment name, read one thing.
# -----------------------------------------------------------------------------

resource "aws_ssm_parameter" "bootstrap" {
  name        = "/${var.name}/e2e/bootstrap"
  description = "Outputs of the Rise ECS e2e bootstrap, for the test harness"
  type        = "String"

  value = jsonencode({
    region                   = var.region
    cluster_name             = aws_ecs_cluster.this.name
    cluster_arn              = aws_ecs_cluster.this.arn
    subnet_ids               = [for s in aws_subnet.public : s.id]
    vpc_id                   = aws_vpc.this.id
    cloud_map_namespace_id   = aws_service_discovery_private_dns_namespace.this.id
    cloud_map_namespace_name = aws_service_discovery_private_dns_namespace.this.name
    log_group_name           = aws_cloudwatch_log_group.this.name
    traefik_task_role_arn    = aws_iam_role.traefik.arn
    ecr_repo_prefix          = "${var.name}/"
    dns_zone_id              = aws_route53_zone.this.zone_id
    dns_zone_name            = trimsuffix(aws_route53_zone.this.name, ".")
    state_bucket             = aws_s3_bucket.state.id
  })

  tags = local.tags
}

output "ci_bootstrap_role_arn" {
  description = "Role that applies this workspace from CI. Null unless enable_ci_bootstrap_role is set."
  value       = var.enable_ci_bootstrap_role ? aws_iam_role.ci_bootstrap[0].arn : null
}

output "backend_config" {
  description = "The `terraform init -backend-config=...` arguments for this workspace."
  value = {
    bucket = aws_s3_bucket.state.id
    key    = "bootstrap/terraform.tfstate"
    region = var.region
  }
}
