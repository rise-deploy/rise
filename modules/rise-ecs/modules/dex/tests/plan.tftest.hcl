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
  name             = "rise"
  tags             = {}
  enabled          = true
  cluster_arn      = "arn:aws:ecs:eu-central-1:123456789012:cluster/rise"
  discovery        = { namespace_id = "ns-abc", name = "rise-dex" }
  network          = { subnet_ids = ["subnet-a"], security_group_id = "sg-dex" }
  logging          = { group_name = "/rise", region = "eu-central-1" }
  task             = { image = "dexidp/dex:v2.45.1", execution_role_arn = "arn:aws:iam::123456789012:role/execution", cpu_architecture = "X86_64" }
  identity         = { issuer = "https://dex.rise.example.com/dex", client_id = "rise", client_secret = "s3cret", public_url = "https://rise.example.com", admin_email = "ops@example.com", admin_password_bcrypt = "$2y$10$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }
  ingress          = { domain = "rise.example.com", entrypoint = "websecure", acme_enabled = true }
  runtime_services = []
}

run "discovery_registration" {
  command = plan
  assert {
    condition = alltrue([
      aws_service_discovery_service.dex[0].name == "rise-dex",
      length(aws_service_discovery_service.dex[0].health_check_custom_config) == 0,
    ])
    error_message = "Dex discovery must be install-scoped without an empty custom health check"
  }
}