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
  dns_zone_name     = "e2e.example.com"
  github_repository = "rise-deploy/rise"
}

run "dex_config_is_the_dev_one_with_a_patched_issuer" {
  command = plan

  # The harness mints tokens with the resource-owner password grant against the
  # static user dev/dex/config.yaml defines. If the patch broke the file, or the
  # grant went missing, sa-token-exchange fails with an opaque 401.
  assert {
    condition     = strcontains(local.dex_config, "issuer: http://dex.rise-e2e.internal:5556/dex")
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
