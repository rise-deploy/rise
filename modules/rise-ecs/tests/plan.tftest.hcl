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
  target = module.network.data.aws_availability_zones.available
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
      module.runtime.rise.discovery_name == "rise-control-plane",
      module.runtime.traefik.discovery_name == "rise-traefik",
      local.rise_environment["RISE_TRAEFIK_API_URL"] == "http://rise-traefik.rise.internal:8080",
    ])
    error_message = "Cloud Map names must be scoped to the Rise installation"
  }

  assert {
    condition     = local.rise_environment["RISE_ECS_ASSIGN_PUBLIC_IP"] == "false"
    error_message = "workloads must run in private subnets without public IPs"
  }
  assert {
    condition     = local.rise_environment["RISE_ECS_LOG_RETENTION_HINT"] == "30d"
    error_message = "the CloudWatch retention policy must reach Rise's empty-log status hint"
  }
  # The identity sidecar defaults to the control plane's own image (read from
  # the task metadata at startup) and to the public URL: workloads reach the
  # control plane only through the edge.
  assert {
    condition = alltrue([
      for key in ["RISE_ECS_IDENTITY_AGENT_IMAGE", "RISE_ECS_IDENTITY_EXCHANGE_URL", "RISE_IDENTITY_TOKEN_TTL_SECONDS"] :
      !contains(keys(local.rise_environment), key)
    ])
    error_message = "the identity sidecar must keep Rise's defaults unless configured"
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

run "injects_additional_control_plane_secrets" {
  command = plan

  variables {
    control_plane_secret_environment = {
      SNOWFLAKE_OAUTH_PRIVATE_KEY = "arn:aws:secretsmanager:eu-central-1:123456789012:secret:rise/snowflake-oauth-abc123:private_key:AWSCURRENT:"
      SHARED_SECRET_COPY          = "arn:aws:secretsmanager:eu-central-1:123456789012:secret:rise/snowflake-oauth-abc123"
    }
  }

  assert {
    condition = one([
      for secret in local.control_plane_secrets :
      secret.valueFrom
      if secret.name == "SNOWFLAKE_OAUTH_PRIVATE_KEY"
    ]) == "arn:aws:secretsmanager:eu-central-1:123456789012:secret:rise/snowflake-oauth-abc123:private_key:AWSCURRENT:"
    error_message = "additional secrets must reach the task with ECS selectors intact"
  }

  assert {
    condition     = output.rise_task_secrets["SNOWFLAKE_OAUTH_PRIVATE_KEY"] == "arn:aws:secretsmanager:eu-central-1:123456789012:secret:rise/snowflake-oauth-abc123:private_key:AWSCURRENT:"
    error_message = "module outputs must expose additional task secret references"
  }

  assert {
    condition = (
      length(output.secret_arns_for_execution_role) == 5
      && output.secret_arns_for_execution_role[4] == "arn:aws:secretsmanager:eu-central-1:123456789012:secret:rise/snowflake-oauth-abc123"
    )
    error_message = "execution-role output must contain each base secret ARN once"
  }
}

run "uses_an_external_traefik_role_without_creating_iam" {
  command = plan

  variables {
    create_traefik_task_role = false
    traefik_task_role_arn    = "arn:aws:iam::123456789012:role/rise-traefik"
  }

  assert {
    condition     = module.runtime.traefik.task_role_arn == "arn:aws:iam::123456789012:role/rise-traefik"
    error_message = "the external Traefik role must reach the task definition"
  }
}

run "identity_sidecar_settings_reach_the_control_plane" {
  command = plan

  variables {
    identity_agent_image       = "123456789012.dkr.ecr.eu-central-1.amazonaws.com/rise:mirror"
    identity_exchange_url      = "http://rise.internal:3000"
    identity_token_ttl_seconds = 900
  }

  assert {
    condition = alltrue([
      local.rise_environment["RISE_ECS_IDENTITY_AGENT_IMAGE"] == "123456789012.dkr.ecr.eu-central-1.amazonaws.com/rise:mirror",
      local.rise_environment["RISE_ECS_IDENTITY_EXCHANGE_URL"] == "http://rise.internal:3000",
      local.rise_environment["RISE_IDENTITY_TOKEN_TTL_SECONDS"] == "900",
    ])
    error_message = "the identity sidecar settings must be configurable through the root module"
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

run "traefik_discovery_is_unconstrained_by_default" {
  command = plan

  assert {
    condition     = length([for c in module.runtime.traefik_command : c if strcontains(c, "constraints")]) == 0
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
      module.runtime.traefik_command,
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
    target = module.cluster.data.aws_ecs_cluster.brought[0]
    values = {
      arn    = "arn:aws:ecs:eu-central-1:123456789012:cluster/existing-cluster"
      status = "ACTIVE"
    }
  }

  override_data {
    target = module.network.data.aws_subnet.brought["subnet-0aaa"]
    values = { vpc_id = "vpc-0123456789abcdef0" }
  }
  override_data {
    target = module.network.data.aws_subnet.brought["subnet-0bbb"]
    values = { vpc_id = "vpc-0123456789abcdef0" }
  }
  override_data {
    target = module.network.data.aws_subnet.brought["subnet-0ccc"]
    values = { vpc_id = "vpc-0123456789abcdef0" }
  }
  override_data {
    target = module.network.data.aws_subnet.brought["subnet-0ddd"]
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
