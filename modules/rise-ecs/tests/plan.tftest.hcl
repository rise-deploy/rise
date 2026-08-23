# Plans the module in both topologies. `terraform validate` only type-checks;
# these catch the wiring errors -- a bad reference, a count/for_each mismatch, a
# precondition that fires on a valid configuration.
#
# The identity data sources are overridden because they call AWS. Everything
# else is planned for real against the provider schema.

provider "aws" {
  region                      = "eu-central-1"
  access_key                  = "AKIAIOSFODNN7EXAMPLE"
  secret_key                  = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
  skip_credentials_validation = true
  skip_requesting_account_id  = true
  skip_metadata_api_check     = true
  skip_region_validation      = true
}

override_data {
  target = data.aws_caller_identity.current
  values = { account_id = "123456789012" }
}

override_data {
  target = data.aws_region.current
  values = { id = "eu-central-1", region = "eu-central-1" }
}

override_data {
  target = data.aws_partition.current
  values = { partition = "aws" }
}

override_data {
  target = data.aws_availability_zones.available
  values = { names = ["eu-central-1a", "eu-central-1b", "eu-central-1c"] }
}

variables {
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
}

run "creates_a_whole_install" {
  command = plan

  assert {
    condition     = aws_ecs_service.traefik.desired_count == 1
    error_message = "Traefik must run one replica: its ACME file store is not multi-writer safe"
  }

  # The backend asserts registry account == ECS credentials' account at startup,
  # because Rise writes no ECR repository policy.
  assert {
    condition     = local.rise_environment["RISE_ECR_ACCOUNT_ID"] == "123456789012"
    error_message = "ECR account must be the caller's own account"
  }

  # Never the public URL. Traefik calls it for every forwardAuth subrequest, and
  # the backend refuses to start when it is empty.
  assert {
    condition     = local.rise_environment["RISE_AUTH_BACKEND_URL"] == "http://rise.rise.internal:3000"
    error_message = "auth_backend_url must be the internal Cloud Map address"
  }

  assert {
    condition     = local.rise_environment["RISE_ECS_ASSIGN_PUBLIC_IP"] == "false"
    error_message = "workloads must run in private subnets without public IPs"
  }
}

run "brings_an_existing_vpc_and_cluster" {
  command = plan

  variables {
    vpc = {
      id                 = "vpc-0123456789abcdef0"
      public_subnet_ids  = ["subnet-0aaa", "subnet-0bbb"]
      private_subnet_ids = ["subnet-0ccc", "subnet-0ddd"]
    }
    cluster = { name = "existing-cluster" }
  }

  override_data {
    target = data.aws_ecs_cluster.brought[0]
    values = {
      arn    = "arn:aws:ecs:eu-central-1:123456789012:cluster/existing-cluster"
      status = "ACTIVE"
    }
  }

  override_data {
    target = data.aws_subnet.brought["subnet-0aaa"]
    values = { vpc_id = "vpc-0123456789abcdef0" }
  }
  override_data {
    target = data.aws_subnet.brought["subnet-0bbb"]
    values = { vpc_id = "vpc-0123456789abcdef0" }
  }
  override_data {
    target = data.aws_subnet.brought["subnet-0ccc"]
    values = { vpc_id = "vpc-0123456789abcdef0" }
  }
  override_data {
    target = data.aws_subnet.brought["subnet-0ddd"]
    values = { vpc_id = "vpc-0123456789abcdef0" }
  }

  assert {
    condition     = local.vpc_id == "vpc-0123456789abcdef0"
    error_message = "should deploy into the given VPC"
  }

  assert {
    condition     = local.cluster_name == "existing-cluster"
    error_message = "should deploy into the given cluster"
  }

  assert {
    condition     = length(aws_vpc.this) == 0 && length(aws_ecs_cluster.this) == 0
    error_message = "should create neither a VPC nor a cluster when both are brought"
  }

  # A comma-joined string, not a list: the settings loader accepts either
  # precisely so a Terraform output can travel through one environment variable.
  # Asserted here rather than in the create-VPC run, where the ids are only
  # known after apply.
  assert {
    condition     = local.rise_environment["RISE_ECS_SUBNETS"] == "subnet-0ccc,subnet-0ddd"
    error_message = "subnets should reach the backend as a comma-separated string"
  }

  assert {
    condition     = local.rise_environment["RISE_ECS_CLUSTER"] == "existing-cluster"
    error_message = "the backend should be pointed at the brought cluster"
  }
}
