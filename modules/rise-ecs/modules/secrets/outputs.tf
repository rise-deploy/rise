output "environment" {
  description = "Populated Secrets Manager references for ECS injection."
  value = {
    RISE_JWT_SIGNING_SECRET = aws_secretsmanager_secret.jwt_signing_secret.arn
    RISE_ENCRYPTION_KEY     = aws_secretsmanager_secret.encryption_key.arn
    OIDC_CLIENT_SECRET      = aws_secretsmanager_secret.oidc_client_secret.arn
  }
  depends_on = [aws_secretsmanager_secret_version.jwt_signing_secret, aws_secretsmanager_secret_version.encryption_key, aws_secretsmanager_secret_version.oidc_client_secret]
}
