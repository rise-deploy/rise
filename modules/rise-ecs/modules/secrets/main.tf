# Nothing sensitive is passed as a plain environment variable. Each value below
# becomes a Secrets Manager secret that the task definition references through
# its `secrets` block, so ECS resolves it at task start under the execution role
# and it never appears in a DescribeTaskDefinition response.

# Each `secret_string_wo_version` is a hash of the value, so a new value writes a
# new version and an unchanged one writes nothing. Thirteen hex digits is 52
# bits: the provider carries the number through a float64, and anything above
# 2^53 comes back rounded, differs from the configuration, and replaces the
# version on every plan.

resource "random_bytes" "jwt_signing_secret" {
  length = 32
}

resource "random_bytes" "encryption_key" {
  length = 32 # AES-GCM-256.
}

resource "aws_secretsmanager_secret" "jwt_signing_secret" {
  name                    = "${var.name}/jwt-signing-secret"
  description             = "Rise JWT signing key"
  recovery_window_in_days = var.recovery_window_days
  tags                    = var.tags
}

resource "aws_secretsmanager_secret_version" "jwt_signing_secret" {
  secret_id                = aws_secretsmanager_secret.jwt_signing_secret.id
  secret_string_wo         = random_bytes.jwt_signing_secret.base64
  secret_string_wo_version = parseint(substr(sha256(random_bytes.jwt_signing_secret.base64), 0, 13), 16)
}

resource "aws_secretsmanager_secret" "encryption_key" {
  name                    = "${var.name}/encryption-key"
  description             = "Rise encryption key (AES-GCM-256)"
  recovery_window_in_days = var.recovery_window_days
  tags                    = var.tags
}

resource "aws_secretsmanager_secret_version" "encryption_key" {
  secret_id                = aws_secretsmanager_secret.encryption_key.id
  secret_string_wo         = random_bytes.encryption_key.base64
  secret_string_wo_version = parseint(substr(sha256(random_bytes.encryption_key.base64), 0, 13), 16)
}

resource "aws_secretsmanager_secret" "oidc_client_secret" {
  name                    = "${var.name}/oidc-client-secret"
  description             = "OIDC client secret for the Rise backend"
  recovery_window_in_days = var.recovery_window_days
  tags                    = var.tags
}

resource "aws_secretsmanager_secret_version" "oidc_client_secret" {
  secret_id                = aws_secretsmanager_secret.oidc_client_secret.id
  secret_string_wo         = var.oidc_client_secret
  secret_string_wo_version = parseint(substr(sha256(var.oidc_client_secret), 0, 13), 16)
}
