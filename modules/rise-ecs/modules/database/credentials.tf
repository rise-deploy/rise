resource "random_password" "database" {
  count = var.enabled ? 1 : 0

  length  = 40
  special = false # Keeps the value safe to embed in a postgres:// URL unescaped.
}

resource "aws_secretsmanager_secret" "database_url" {
  count = var.enabled ? 1 : 0

  name                    = "${var.name}/database-url"
  description             = "Rise control-plane database connection URL"
  recovery_window_in_days = var.secret_recovery_window_days
  tags                    = var.tags
}

# The version hash is cut to 52 bits -- see modules/secrets for why.
resource "aws_secretsmanager_secret_version" "database_url" {
  count = var.enabled ? 1 : 0

  secret_id                = aws_secretsmanager_secret.database_url[0].id
  secret_string_wo         = local.database_url
  secret_string_wo_version = parseint(substr(sha256(local.database_url), 0, 13), 16)
}
locals {
  database_url = var.enabled ? format(
    "postgres://%s:%s@%s/%s",
    aws_db_instance.this[0].username,
    random_password.database[0].result,
    aws_db_instance.this[0].endpoint,
    aws_db_instance.this[0].db_name
  ) : null
}
