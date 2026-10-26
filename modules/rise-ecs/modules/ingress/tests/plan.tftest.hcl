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
  name            = "rise"
  tags            = {}
  network         = { vpc_id = "vpc-abc", public_subnet_ids = ["subnet-a", "subnet-b"], private_subnets_by_key = { a = "subnet-c", b = "subnet-d" } }
  security_groups = { edge = "sg-edge", efs = "sg-efs" }
  edge            = { mode = "nlb-traefik-acme", acm_certificate_arn = null, deletion_protection = true }
  dns             = { create = false, zone_id = null, domain = "rise.example.com" }
  traefik_role    = { create = true, arn = null, account_id = "123456789012" }
}

run "external_traefik_role" {
  command = plan
  variables { traefik_role = { create = false, arn = "arn:aws:iam::123456789012:role/traefik", account_id = "123456789012" } }
  assert {
    condition     = length(aws_iam_role.traefik) == 0
    error_message = "an external Traefik role must disable module-owned IAM"
  }
}

run "alb_redirects_http" {
  command = plan
  variables { edge = { mode = "alb-acm", acm_certificate_arn = "arn:aws:acm:eu-central-1:123456789012:certificate/abc", deletion_protection = true } }
  assert {
    condition = (
      aws_lb_listener.http.default_action[0].type == "redirect"
      && aws_lb_listener.http.default_action[0].redirect[0].protocol == "HTTPS"
      && aws_lb_listener.http.default_action[0].redirect[0].status_code == "HTTP_301"
    )
    error_message = "ALB mode must redirect HTTP to HTTPS"
  }
}

run "the_traefik_role_grants_what_its_ecs_provider_actually_calls" {
  command = plan

  assert {
    condition = setunion(toset(local.traefik_discovery_actions), toset([
      "ecs:ListClusters", "ecs:DescribeClusters", "ecs:ListTasks",
      "ecs:DescribeTasks", "ecs:DescribeContainerInstances",
      "ecs:DescribeTaskDefinition", "ec2:DescribeInstances",
      "ssm:DescribeInstanceInformation",
    ])) == toset(local.traefik_discovery_actions)
    error_message = "the Traefik task role is missing an action its ECS provider calls"
  }
}