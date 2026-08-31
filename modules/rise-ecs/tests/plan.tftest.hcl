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

  assert {
    condition = jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].healthCheck == {
      command = [
        "CMD",
        "traefik",
        "healthcheck",
        "--ping=true",
        "--entrypoints.ping.address=:8082",
        "--ping.entrypoint=ping",
      ]
      interval    = 30
      timeout     = 5
      retries     = 3
      startPeriod = 10
    }
    error_message = "Traefik must report ECS health through its dedicated ping entrypoint"
  }

  assert {
    condition = (
      aws_secretsmanager_secret_version.oidc_client_secret.secret_string == null
      && aws_secretsmanager_secret_version.oidc_client_secret.secret_string_wo_version == parseint(substr(sha256("s3cret"), 0, 15), 16)
    )
    error_message = "managed secret versions must use a content-sensitive write-only version"
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
    condition     = local.rise_environment["RISE_AUTH_BACKEND_URL"] == "http://rise-control-plane.rise.internal:3000"
    error_message = "auth_backend_url must be the internal Cloud Map address"
  }

  assert {
    condition = alltrue([
      aws_service_discovery_service.rise.name == "rise-control-plane",
      aws_service_discovery_service.traefik.name == "rise-traefik",
      local.rise_environment["RISE_TRAEFIK_API_URL"] == "http://rise-traefik.rise.internal:8080",
    ])
    error_message = "Cloud Map names must be scoped to the Rise installation"
  }

  assert {
    condition = alltrue([
      length(aws_service_discovery_service.rise.health_check_custom_config) == 0,
      length(aws_service_discovery_service.traefik.health_check_custom_config) == 0,
    ])
    error_message = "empty custom health checks cause perpetual Cloud Map service replacement"
  }

  assert {
    condition     = local.rise_environment["RISE_ECS_ASSIGN_PUBLIC_IP"] == "false"
    error_message = "workloads must run in private subnets without public IPs"
  }
  assert {
    condition     = local.rise_environment["RISE_ECS_LOG_RETENTION_HINT"] == "30d"
    error_message = "the CloudWatch retention policy must reach Rise's empty-log status hint"
  }
}

run "loads_a_secret_local_config_overlay" {
  command = plan

  variables {
    control_plane_local_config_secret_arn = "arn:aws:secretsmanager:eu-central-1:123456789012:secret:rise/local-config-abc123"
  }

  assert {
    condition = one([
      for secret in local.control_plane_secrets :
      secret.valueFrom
      if secret.name == "RISE_LOCAL_CONFIG_YAML"
    ]) == "arn:aws:secretsmanager:eu-central-1:123456789012:secret:rise/local-config-abc123"
    error_message = "the overlay must reach the task through ECS secret injection"
  }

  assert {
    condition     = output.rise_task_secrets["RISE_LOCAL_CONFIG_YAML"] == "arn:aws:secretsmanager:eu-central-1:123456789012:secret:rise/local-config-abc123"
    error_message = "module outputs must expose the overlay secret to external task-definition wiring"
  }
}

run "uses_an_external_traefik_role_without_creating_iam" {
  command = plan

  variables {
    create_traefik_task_role = false
    traefik_task_role_arn    = "arn:aws:iam::123456789012:role/rise-traefik"
  }

  assert {
    condition     = length(aws_iam_role.traefik) == 0
    error_message = "an external Traefik role must disable module-owned IAM"
  }

  assert {
    condition     = aws_ecs_task_definition.traefik.task_role_arn == "arn:aws:iam::123456789012:role/rise-traefik"
    error_message = "the external Traefik role must reach the task definition"
  }
}

run "alb_uses_https_and_group_restricted_auth" {
  command = plan

  variables {
    edge_mode                  = "alb-acm"
    acme_email                 = null
    acm_certificate_arn        = "arn:aws:acm:eu-central-1:123456789012:certificate/abc"
    rise_image_tag             = null
    rise_image_ref             = "ghcr.io/rise-deploy/rise@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    oidc_group_claim           = "cognito:groups"
    idp_group_sync_prefixes    = ["rise-"]
    admin_idp_group            = "rise-admins"
    platform_access_policy     = "restrictive"
    platform_allowed_idp_group = "rise-platform-users"
  }

  assert {
    condition     = local.rise_environment["RISE_IDP_GROUP_SYNC_PREFIXES"] == "rise-"
    error_message = "IdP group sync prefixes must reach the shipped ECS configuration"
  }

  assert {
    condition = (
      aws_lb_listener.http.default_action[0].type == "redirect"
      && aws_lb_listener.http.default_action[0].redirect[0].protocol == "HTTPS"
      && aws_lb_listener.http.default_action[0].redirect[0].status_code == "HTTP_301"
    )
    error_message = "ALB mode must redirect HTTP to HTTPS"
  }

  assert {
    condition = alltrue([
      local.rise_image_ref == "ghcr.io/rise-deploy/rise@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      local.rise_environment["OIDC_GROUP_CLAIM"] == "cognito:groups",
      local.rise_environment["RISE_ADMIN_IDP_GROUP"] == "rise-admins",
      local.rise_environment["RISE_PLATFORM_ACCESS_POLICY"] == "restrictive",
      local.rise_environment["RISE_PLATFORM_ALLOWED_IDP_GROUP"] == "rise-platform-users",
    ])
    error_message = "digest and group-based authorization settings must reach the control-plane task"
  }
}

# The database /24s must stay clear of the private /20s at the maximum four AZs.
# The private subnets are /20s at netnum `index + 1`, so the fourth AZ's is
# 10.42.64.0/20 (covering /24-netnums 64..79). A database range starting at
# netnum 64 would fall inside it, and `apply` rejects overlapping subnet CIDRs --
# masked at the 2-AZ default, a hard failure at 4. `plan` cannot see the overlap
# (AWS validates it at apply), so this pins the computed CIDRs instead.
run "database_subnets_clear_the_private_range_at_four_azs" {
  command = plan

  override_data {
    target = data.aws_availability_zones.available
    values = { names = ["eu-central-1a", "eu-central-1b", "eu-central-1c", "eu-central-1d"] }
  }

  variables {
    availability_zone_count = 4
  }

  # The private /20 that used to collide with the database range.
  assert {
    condition     = aws_subnet.private["eu-central-1d"].cidr_block == "10.42.64.0/20"
    error_message = "the fourth private subnet moved; re-check the database offset"
  }
  # Database /24s at netnum 128+, well above the private range's /24-netnum 79.
  assert {
    condition     = aws_subnet.database["eu-central-1a"].cidr_block == "10.42.128.0/24"
    error_message = "database subnet must start clear of the private /20 range"
  }
  assert {
    condition     = aws_subnet.database["eu-central-1d"].cidr_block == "10.42.131.0/24"
    error_message = "database subnet must start clear of the private /20 range"
  }
}

# A cluster shared by two installs is only safe if each Traefik discovers just
# its own containers. The constraint is what enforces that, and it must stay off
# by default so a single-install cluster keeps routing everything Rise labels.
# Traefik's ECS provider calls ec2:DescribeInstances and
# ssm:DescribeInstanceInformation even on Fargate -- they are in the policy its
# own documentation publishes. Missing either, discovery silently yields nothing
# and Traefik 404s every host while looking perfectly healthy.
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

run "traefik_discovery_is_unconstrained_by_default" {
  command = plan

  assert {
    condition     = length([for c in local.traefik_command : c if strcontains(c, "constraints")]) == 0
    error_message = "an unset traefik_constraints must add no constraint flag"
  }
}

# Confining Traefik to one class is what lets two installs share a cluster --
# and it applies to every container Traefik considers, the control plane
# included. An unlabelled Rise is invisible to the proxy meant to publish it,
# which takes the whole install down rather than degrading.
run "the_control_plane_carries_the_label_its_own_traefik_constrains_on" {
  command = plan

  # Asserted on the label map rather than on the rendered container definition,
  # which carries generated secrets and so is unknown at plan time.
  assert {
    condition     = module.control_plane_env.docker_labels["rise.dev/controller-class"] == "default"
    error_message = "the control plane would be filtered out by its own Traefik constraint"
  }
}

run "the_control_plane_answers_on_a_subdomain_by_default" {
  command = plan

  assert {
    condition     = module.control_plane_env.docker_labels["traefik.http.routers.rise-cp.rule"] == "Host(`rise.rise.example.com`)"
    error_message = "the default control-plane host changed"
  }

  assert {
    condition     = module.control_plane_env.public_url == "https://rise.rise.example.com"
    error_message = "public_url must name the host the router matches, or login redirects land nowhere"
  }
}

# The layout an internal platform tends to want: Rise on the domain itself,
# projects on the labels below it.
run "the_control_plane_can_answer_at_the_apex" {
  command = plan

  variables {
    ingress_domain     = "apps.platform.internal"
    control_plane_host = "apps.platform.internal"
  }

  assert {
    condition     = module.control_plane_env.docker_labels["traefik.http.routers.rise-cp.rule"] == "Host(`apps.platform.internal`)"
    error_message = "the control plane does not answer at the apex"
  }

  assert {
    condition     = module.control_plane_env.public_url == "https://apps.platform.internal"
    error_message = "public_url still names a subdomain the router does not match"
  }

  # A project's router must remain distinct from the control plane's, or the
  # two collide on one host and whichever Traefik sorts first wins.
  assert {
    condition     = module.control_plane_env.docker_labels["traefik.http.routers.rise-cp.rule"] != "Host(`myapp.apps.platform.internal`)"
    error_message = "the control plane would swallow a project hostname"
  }
}

run "traefik_discovery_can_be_confined_to_one_install" {
  command = plan

  variables {
    controller_class_name = "pr-462"
    traefik_constraints   = "Label(`rise.dev/controller-class`, `pr-462`)"
  }

  assert {
    condition = contains(
      local.traefik_command,
      "--providers.ecs.constraints=Label(`rise.dev/controller-class`, `pr-462`)"
    )
    error_message = "traefik_constraints must reach Traefik as a provider flag"
  }

  assert {
    condition     = module.control_plane_env.docker_labels["rise.dev/controller-class"] == "pr-462"
    error_message = "controller_class_name must label the control plane matched by Traefik"
  }

  assert {
    condition     = module.control_plane_env.environment.RISE_CONTROLLER_CLASS_NAME == "pr-462"
    error_message = "controller_class_name must reach the controller's orphan-reconciliation scope"
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
