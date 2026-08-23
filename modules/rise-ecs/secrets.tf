# Nothing sensitive is passed as a plain environment variable. Each value below
# becomes a Secrets Manager secret that the task definition references through
# its `secrets` block, so ECS resolves it at task start under the execution role
# and it never appears in a DescribeTaskDefinition response.

resource "random_password" "database" {
  count = local.create_database ? 1 : 0

  length  = 40
  special = false # Keeps the value safe to embed in a postgres:// URL unescaped.
}

resource "random_bytes" "jwt_signing_secret" {
  length = 32
}

resource "random_bytes" "encryption_key" {
  length = 32 # AES-GCM-256.
}

resource "aws_secretsmanager_secret" "database_url" {
  count = local.create_database ? 1 : 0

  name                    = "${local.name}/database-url"
  description             = "Rise control-plane database connection URL"
  recovery_window_in_days = var.secret_recovery_window_days
  tags                    = local.tags
}

resource "aws_secretsmanager_secret_version" "database_url" {
  count = local.create_database ? 1 : 0

  secret_id = aws_secretsmanager_secret.database_url[0].id
  secret_string = format(
    "postgres://%s:%s@%s/%s",
    aws_db_instance.this[0].username,
    random_password.database[0].result,
    aws_db_instance.this[0].endpoint,
    aws_db_instance.this[0].db_name
  )
}

resource "aws_secretsmanager_secret" "jwt_signing_secret" {
  name                    = "${local.name}/jwt-signing-secret"
  description             = "Rise JWT signing key"
  recovery_window_in_days = var.secret_recovery_window_days
  tags                    = local.tags
}

resource "aws_secretsmanager_secret_version" "jwt_signing_secret" {
  secret_id     = aws_secretsmanager_secret.jwt_signing_secret.id
  secret_string = random_bytes.jwt_signing_secret.base64
}

resource "aws_secretsmanager_secret" "encryption_key" {
  name                    = "${local.name}/encryption-key"
  description             = "Rise encryption key (AES-GCM-256)"
  recovery_window_in_days = var.secret_recovery_window_days
  tags                    = local.tags
}

resource "aws_secretsmanager_secret_version" "encryption_key" {
  secret_id     = aws_secretsmanager_secret.encryption_key.id
  secret_string = random_bytes.encryption_key.base64
}

resource "aws_secretsmanager_secret" "oidc_client_secret" {
  name                    = "${local.name}/oidc-client-secret"
  description             = "OIDC client secret for the Rise backend"
  recovery_window_in_days = var.secret_recovery_window_days
  tags                    = local.tags
}

resource "aws_secretsmanager_secret_version" "oidc_client_secret" {
  secret_id     = aws_secretsmanager_secret.oidc_client_secret.id
  secret_string = coalesce(var.oidc_client_secret, "rise-backend-secret")
}
