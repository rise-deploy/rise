# Plans the bootstrap and asserts the things that are easy to get silently wrong.

# Replaces the root module's provider block, which would otherwise validate
# credentials against a real account.
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
  target = data.aws_partition.current
  values = { partition = "aws" }
}

# The submodule reads identity too.
override_data {
  target = module.rise_aws.data.aws_caller_identity.current
  values = { account_id = "123456789012" }
}

override_data {
  target = module.rise_aws.data.aws_region.current
  values = { region = "eu-central-1" }
}

override_data {
  target = module.rise_aws.data.aws_partition.current
  values = { partition = "aws" }
}

override_data {
  target = data.aws_availability_zones.available
  values = { names = ["eu-central-1a", "eu-central-1b", "eu-central-1c"] }
}

variables {
  name              = "rise-e2e"
  region            = "eu-central-1"
  dns_zone_name     = "rise-deploy.click"
  github_repository = "rise-deploy/rise"
}

run "ci_role_can_pass_roles_but_not_write_iam" {
  command = plan

  # Registering a task definition that names a role requires PassRole. Losing it
  # to a blanket deny surfaces as a confusing ECS error, so it is asserted.
  assert {
    condition     = strcontains(data.aws_iam_policy_document.ci.json, "iam:PassRole")
    error_message = "the CI role cannot pass the pre-created roles, so it cannot register a task definition"
  }

  assert {
    condition     = strcontains(data.aws_iam_policy_document.ci.json, "iam:PassedToService")
    error_message = "PassRole is not confined to launching ECS tasks"
  }

  # The whole posture: everything needing an IAM write is created here, by hand,
  # never by the per-run apply.
  assert {
    condition = alltrue([
      for s in jsondecode(data.aws_iam_policy_document.ci.json).Statement :
      length([for a in flatten([s.Action]) : a if startswith(a, "iam:") && a != "iam:PassRole"]) == 0
    ])
    error_message = "the CI role has an IAM action beyond PassRole"
  }
}

run "the_bootstrap_apply_role_is_off_by_default" {
  command = plan

  # It can write IAM, and a principal that can create roles and attach policies
  # can escalate. Nobody should acquire it by applying this module.
  assert {
    condition     = length(aws_iam_role.ci_bootstrap) == 0
    error_message = "the IAM-writing role is created without being asked for"
  }
}

run "the_bootstrap_apply_role_is_narrowly_trusted_and_scoped" {
  command = plan

  variables {
    enable_ci_bootstrap_role = true
  }

  # Not repo:<repo>:* like the run role: a pull request, or a branch anyone can
  # push, must not reach an identity that can write IAM.
  assert {
    condition = alltrue([
      for s in jsondecode(data.aws_iam_policy_document.ci_bootstrap_assume[0].json).Statement :
      !contains(
        flatten([try(s.Condition.StringLike["token.actions.githubusercontent.com:sub"], [])]),
        "repo:rise-deploy/rise:*"
      )
    ])
    error_message = "the bootstrap role trusts every ref in the repository"
  }

  assert {
    condition     = strcontains(data.aws_iam_policy_document.ci_bootstrap_assume[0].json, "refs/heads/develop")
    error_message = "the default trust subject is not the develop branch"
  }

  # The escalation-capable actions must not reach roles this workspace does not
  # own.
  assert {
    condition = alltrue([
      for s in jsondecode(data.aws_iam_policy_document.ci_bootstrap[0].json).Statement :
      s.Sid != "ManageEnvironmentIAM" || !contains(flatten([s.Resource]), "*")
    ])
    error_message = "IAM writes are granted on every role in the account"
  }

  # Recreating the account-global OIDC provider would break every other workflow
  # that trusts it.
  assert {
    condition     = !strcontains(data.aws_iam_policy_document.ci_bootstrap[0].json, "iam:CreateOpenIDConnectProvider")
    error_message = "the bootstrap role can replace the shared OIDC provider"
  }
}
