output "endpoint" {
  description = "PostgreSQL endpoint, when managed."
  value       = try(aws_db_instance.this[0].endpoint, null)
}
output "url_secret_arn" {
  description = "Populated database URL secret, when managed."
  value       = try(aws_secretsmanager_secret.database_url[0].arn, null)
  depends_on  = [aws_secretsmanager_secret_version.database_url]
}
