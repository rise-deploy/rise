# The zone is created alongside the install, so its ID is unknown until apply.
resource "aws_route53_zone" "this" {
  name = "rise.example.com"
}

module "rise" {
  source = "../../.."

  name                = "rise"
  ingress_domain      = "rise.example.com"
  admin_email         = "ops@example.com"
  rise_image_tag      = "0.23.0"
  acme_email          = "ops@example.com"
  controller_role_arn = "arn:aws:iam::123456789012:role/rise"
  execution_role_arn  = "arn:aws:iam::123456789012:role/rise-ecs-execution"
  ecr_push_role_arn   = "arn:aws:iam::123456789012:role/rise-ecr-push"
  oidc_issuer         = "https://id.example.com"
  oidc_client_secret  = "s3cret"
  route53_zone        = aws_route53_zone.this
}

output "dns_records_required" {
  value = module.rise.dns_records_required
}
