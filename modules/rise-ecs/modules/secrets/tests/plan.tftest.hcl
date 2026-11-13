provider "aws" {
  region                      = "eu-central-1"
  access_key                  = "AKIAIOSFODNN7EXAMPLE"
  secret_key                  = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
  skip_credentials_validation = true
  skip_requesting_account_id  = true
  skip_metadata_api_check     = true
  skip_region_validation      = true
}

variables {
  name                 = "rise"
  tags                 = {}
  oidc_client_secret   = "s3cret"
  recovery_window_days = 7
}

run "write_only_secret_versions" {
  command = plan
  assert {
    condition = (
      aws_secretsmanager_secret_version.oidc_client_secret.secret_string == null
      && aws_secretsmanager_secret_version.oidc_client_secret.secret_string_wo_version == parseint(substr(sha256("s3cret"), 0, 13), 16)
    )
    error_message = "managed secret versions must use a content-sensitive write-only version"
  }
}

run "traefik_config_secret_is_a_write_only_secret" {
  command = plan
  assert {
    condition = (
      random_bytes.traefik_config_secret.length == 32
      && aws_secretsmanager_secret.traefik_config_secret.name == "rise/traefik-config-secret"
      && aws_secretsmanager_secret_version.traefik_config_secret.secret_string == null
    )
    error_message = "the Traefik config secret must be 32 random bytes in a write-only Secrets Manager secret"
  }
}
