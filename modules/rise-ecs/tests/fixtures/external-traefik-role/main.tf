resource "aws_iam_role" "traefik" {
  name = "rise-traefik"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "ecs-tasks.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

module "rise" {
  source = "../../.."

  name                     = "rise"
  ingress_domain           = "rise.example.com"
  admin_email              = "ops@example.com"
  rise_image_tag           = "0.23.0"
  acme_email               = "ops@example.com"
  controller_role_arn      = "arn:aws:iam::123456789012:role/rise"
  execution_role_arn       = "arn:aws:iam::123456789012:role/rise-ecs-execution"
  ecr_push_role_arn        = "arn:aws:iam::123456789012:role/rise-ecr-push"
  oidc_issuer              = "https://id.example.com"
  oidc_client_secret       = "s3cret"
  create_traefik_task_role = false
  traefik_task_role_arn    = aws_iam_role.traefik.arn
}
