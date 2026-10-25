output "load_balancer" {
  description = "Public load balancer address."
  value       = { dns_name = aws_lb.this.dns_name, zone_id = aws_lb.this.zone_id }
}
output "target_groups" {
  description = "Attached target groups keyed by entrypoint port."
  value       = { for port, group in aws_lb_target_group.traefik : port => group.arn }
  depends_on  = [aws_lb_listener.http, aws_lb_listener.https, aws_lb_listener.alb_https]
}
output "acme" {
  description = "Mounted certificate storage for Traefik."
  value = {
    enabled         = local.acme_enabled
    file_system_id  = try(aws_efs_file_system.acme[0].id, null)
    access_point_id = try(aws_efs_access_point.acme[0].id, null)
  }
  depends_on = [aws_efs_mount_target.acme]
}
output "traefik_role_arn" {
  description = "Traefik task identity with its managed policy attached."
  value       = coalesce(var.traefik_role.arn, try(aws_iam_role.traefik[0].arn, null))
  depends_on  = [aws_iam_role_policy.traefik]
}
