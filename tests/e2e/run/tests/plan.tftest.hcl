# Plans the per-run stack against a mocked bootstrap state.

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
  target = data.terraform_remote_state.bootstrap
  values = {
    outputs = {
      region                   = "eu-central-1"
      cluster_name             = "rise-e2e"
      cluster_arn              = "arn:aws:ecs:eu-central-1:123456789012:cluster/rise-e2e"
      subnet_ids               = ["subnet-aaa", "subnet-bbb"]
      vpc_id                   = "vpc-abc"
      cloud_map_namespace_id   = "ns-abc"
      cloud_map_namespace_name = "rise-e2e.internal"
      log_group_name           = "/rise-e2e"
      log_retention_days       = 30
      traefik_task_role_arn    = "arn:aws:iam::123456789012:role/rise-e2e-traefik"
      controller_role_arn      = "arn:aws:iam::123456789012:role/rise-e2e"
      execution_role_arn       = "arn:aws:iam::123456789012:role/rise-e2e-ecs-execution"
      ecr_push_role_arn        = "arn:aws:iam::123456789012:role/rise-e2e-ecr-push"
      ecr_repo_prefix          = "rise-e2e/"
      dns_zone_id              = "Z123"
      dns_zone_name            = "rise-deploy.click"
      state_bucket             = "rise-e2e-tfstate-123456789012"
      ci_role_arn              = "arn:aws:iam::123456789012:role/rise-e2e-ci"
    }
  }
}

variables {
  name               = "rise-e2e"
  state_bucket       = "rise-e2e-tfstate-123456789012"
  rise_image_tag     = "0.23.0"
  scope              = "pr-457"
  dns_zone_name      = "rise-deploy.click"
  jwt_signing_secret = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE9"
  encryption_key     = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE9"
}

run "per_run_stack_plans" {
  command = plan

  # Plain HTTP: no load balancer to terminate at, no ACME. A Secure cookie is
  # never sent over HTTP, so this must follow the scheme.
  assert {
    condition     = module.control_plane_env.environment["RISE_COOKIE_SECURE"] == "false"
    error_message = "cookie security must follow the scheme, or login breaks over http"
  }

  assert {
    condition     = module.control_plane_env.public_url == "http://rise.pr-457.rise-deploy.click"
    error_message = "public_url must match what the CI bearer's iss will be"
  }

  # The issuer is an in-VPC plaintext address, which the SSRF defaults refuse.
  assert {
    condition     = module.control_plane_env.environment["RISE_SSRF_ALLOW_HTTP"] == "true"
    error_message = "Rise would refuse OIDC discovery against the Cloud Map issuer"
  }

  assert {
    condition     = module.control_plane_env.environment["DEX_ISSUER"] == "http://dex-pr-457.rise-e2e.internal:5556/dex"
    error_message = "the issuer must be the Cloud Map address the password grant is served from"
  }

  # No NAT in this topology, so a task without a public IP cannot reach ECR.
  assert {
    condition     = module.control_plane_env.environment["RISE_ECS_ASSIGN_PUBLIC_IP"] == "true"
    error_message = "workloads need public IPs here; there is no NAT for them to egress through"
  }

  assert {
    condition     = module.control_plane_env.environment["RISE_ECS_LOG_RETENTION_HINT"] == "30d"
    error_message = "the bootstrap log retention must reach Rise's empty-log status hint"
  }

  # Two runs sharing the cluster with the same controller class delete each
  # other's services, so it must be the scope and not the default.
  assert {
    condition     = module.control_plane_env.environment["RISE_CONTROLLER_CLASS_NAME"] == "pr-457"
    error_message = "the controller class must be the run scope"
  }

  assert {
    condition     = module.control_plane_env.environment["RISE_ECR_ACCOUNT_ID"] == "123456789012"
    error_message = "ECR account must be the caller's own; the backend asserts it at startup"
  }
}

# The three jobs the scope does. Any one of them silently reverting to a shared
# value turns concurrent runs into runs that break each other.
run "the_scope_isolates_dns_routing_and_collection" {
  command = plan

  # Traefik must route only this run's containers. Without the constraint every
  # concurrent run's Traefik answers for every run's hosts.
  assert {
    condition = anytrue([
      for c in jsondecode(aws_ecs_task_definition.traefik.container_definitions)[0].command :
      c == "--providers.ecs.constraints=Label(`rise.dev/controller-class`, `pr-457`)"
    ])
    error_message = "this run's Traefik is not confined to its own controller class"
  }

  # Dex is routed by that same Traefik, so it needs the label the constraint
  # matches -- the constraint applies to every container, not just Rise's.
  assert {
    condition = jsondecode(
      aws_ecs_task_definition.dex.container_definitions
    )[0].dockerLabels["rise.dev/controller-class"] == "pr-457"
    error_message = "Dex would be filtered out by the run's own Traefik constraint"
  }

  assert {
    condition = jsondecode(
      aws_ecs_task_definition.dex.container_definitions
    )[0].dockerLabels["traefik.http.routers.dex.rule"] == "Host(`dex.pr-457.rise-deploy.click`)"
    error_message = "Dex must be routed under this run's scope, not the zone apex"
  }

  # And so does Rise. It is the container the whole run is waiting on: without
  # the label its own Traefik filters it out, /health never answers through the
  # proxy, and the run times out with no clue that routing was the problem.
  # Asserted on the label map rather than on the rendered container definition,
  # which carries generated secrets and so is unknown at plan time.
  assert {
    condition     = module.control_plane_env.docker_labels["rise.dev/controller-class"] == "pr-457"
    error_message = "the control plane would be filtered out by the run's own Traefik constraint"
  }

  # ECS service names are unique per cluster and the cluster is shared, so an
  # unscoped name is not a cosmetic slip: the second concurrent run's apply
  # fails outright on CreateService. Asserted for all four rather than for the
  # ones that were wrong, so the next service added has to answer for itself.
  assert {
    condition = alltrue([
      for n in [
        aws_ecs_service.postgres.name,
        aws_ecs_service.rise.name,
        aws_ecs_service.traefik.name,
        aws_ecs_service.dex.name,
      ] : strcontains(n, "-pr-457-")
    ])
    error_message = "an ECS service name does not carry the run's scope"
  }

  # Same for what Rise itself creates. These are enumerated by prefix at the
  # next run's bring-up sweep, so an unscoped prefix hands that sweep a
  # concurrent run's live images and secrets.
  assert {
    condition     = local.scoped_ecr_repo_prefix == "rise-e2e/pr-457/"
    error_message = "Rise's ECR repositories are not confined to this run"
  }

  assert {
    condition     = module.control_plane_env.environment["RISE_ECS_SSM_PREFIX"] == "rise-e2e/pr-457"
    error_message = "Rise's SSM parameters are not confined to this run"
  }

  # Cloud Map is shared across runs, so per-run services need distinct names.
  assert {
    condition     = aws_service_discovery_service.traefik.name == "traefik-pr-457"
    error_message = "Cloud Map names must be scoped or concurrent runs collide"
  }
}

# Empty by default: nothing but the run itself has business reaching the edge.
# EC2 accepts only a restricted character set in a rule description, and
# rejects the whole AuthorizeSecurityGroupIngress call otherwise -- an
# apostrophe in a sentence is enough to fail the apply after the rest of the
# stack is already up. Nothing local catches it, so it is pinned here.
run "security_group_rule_descriptions_use_only_what_ec2_accepts" {
  command = plan

  # Both client-facing rules are per-address, so without one authorized there
  # are no instances of them to check and the assertion passes vacuously.
  variables {
    authorized_cidrs = ["203.0.113.7/32"]
  }

  assert {
    condition = alltrue([
      for d in concat(
        [for r in values(aws_vpc_security_group_ingress_rule.edge_from_client) : r.description],
        [for r in values(aws_vpc_security_group_ingress_rule.edge_api_from_client) : r.description],
        [
          aws_vpc_security_group_ingress_rule.edge_from_internal.description,
          aws_vpc_security_group_ingress_rule.internal_from_edge.description,
          aws_vpc_security_group_ingress_rule.internal_self.description,
          aws_vpc_security_group_egress_rule.edge_all.description,
          aws_vpc_security_group_egress_rule.internal_all.description,
        ],
      ) : d != null && length(regexall("[^a-zA-Z0-9. _:/()#,@\\[\\]+=&;{}!$*-]", d == null ? "" : d)) == 0
    ])
    error_message = "a security group rule description is missing or uses a character EC2 rejects"
  }
}

run "the_edge_is_closed_unless_an_address_is_authorized" {
  command = plan

  assert {
    condition     = length(aws_vpc_security_group_ingress_rule.edge_from_client) == 0
    error_message = "the edge is open without an address being authorized"
  }
}

run "an_authorized_address_reaches_the_edge_on_80" {
  command = plan

  variables {
    authorized_cidrs = ["203.0.113.7/32"]
  }

  assert {
    condition     = aws_vpc_security_group_ingress_rule.edge_from_client["203.0.113.7/32"].to_port == 80
    error_message = "the authorized address does not reach Traefik's web entrypoint"
  }
}

run "dex_config_is_the_dev_one_with_a_patched_issuer" {
  command = plan

  # The harness mints tokens with the resource-owner password grant against the
  # static user dev/dex/config.yaml defines. If the patch broke the file, or the
  # grant went missing, sa-token-exchange fails with an opaque 401.
  assert {
    condition     = strcontains(local.dex_config, "issuer: http://dex-pr-457.rise-e2e.internal:5556/dex")
    error_message = "the issuer was not patched into the dev Dex config"
  }

  assert {
    condition     = !strcontains(local.dex_config, "issuer: http://rise-dex:5556/dex")
    error_message = "the original dev issuer survived the patch"
  }

  assert {
    condition     = strcontains(local.dex_config, "- password")
    error_message = "the password grant is missing; the harness cannot mint id_tokens"
  }

  assert {
    condition     = strcontains(local.dex_config, "user@example.com")
    error_message = "the static user sa-token-exchange authenticates as is missing"
  }
}
