# Renders the backend policy in ECS mode and asserts its shape.
#
# The three identity data sources are overridden because they call STS/EC2;
# aws_iam_policy_document is left real, since the provider renders it locally
# and rendering it is the whole point of the test.

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

variables {
  name            = "rise-backend"
  enable_ecr      = true
  enable_ecs      = true
  ssm_kms_key_arn = "arn:aws:kms:eu-central-1:123456789012:key/abc"
  ecs_secret_arns = ["arn:aws:secretsmanager:eu-central-1:123456789012:secret:rise/db-*"]
}

run "ecs_policy_grants_only_what_the_controller_calls" {
  command = plan

  # crates/rise-backend-ecs depends on aws-sdk-ecs, aws-sdk-ssm and aws-sdk-sts
  # and nothing else. Task IPs come from DescribeTasks attachment details, app
  # logs are written by the ECS agent under the execution role, and Cloud Map
  # registration (D10) is unimplemented -- so none of these may appear.
  assert {
    condition     = !strcontains(data.aws_iam_policy_document.backend.json, "ec2:")
    error_message = "controller policy grants ec2:*, which the crate never calls"
  }

  assert {
    condition     = !strcontains(data.aws_iam_policy_document.backend.json, "servicediscovery:")
    error_message = "controller policy grants servicediscovery:*, unused until D10 lands"
  }

  assert {
    condition     = !strcontains(data.aws_iam_policy_document.backend.json, "logs:")
    error_message = "controller policy grants logs:*, which the crate never calls"
  }
}

run "passrole_is_scoped_to_the_two_task_roles" {
  command = plan

  # An unscoped iam:PassRole would let anyone who can create a Rise deployment
  # run a task as any role in the account.
  assert {
    condition = alltrue([
      for s in jsondecode(data.aws_iam_policy_document.backend.json).Statement :
      !contains(flatten([s.Action]), "iam:PassRole") || !contains(flatten([s.Resource]), "*")
    ])
    error_message = "iam:PassRole is granted on *"
  }

  assert {
    condition = alltrue([
      for s in jsondecode(data.aws_iam_policy_document.backend.json).Statement :
      !contains(flatten([s.Action]), "iam:PassRole") || length(flatten([s.Resource])) == 2
    ])
    error_message = "iam:PassRole names something other than exactly the execution and task roles"
  }

  assert {
    condition     = strcontains(data.aws_iam_policy_document.backend.json, "iam:PassedToService")
    error_message = "iam:PassRole has no PassedToService condition, so it is usable against other services"
  }
}

run "ecs_writes_are_confined_to_one_cluster" {
  command = plan

  assert {
    condition = alltrue([
      for s in jsondecode(data.aws_iam_policy_document.backend.json).Statement :
      s.Sid != "ManageECSServices" || can(s.Condition.ArnEquals["ecs:cluster"])
    ])
    error_message = "service/task writes are not bounded by an ecs:cluster condition"
  }

  assert {
    condition     = !strcontains(data.aws_iam_policy_document.backend.json, "\"ecs:*\"")
    error_message = "policy grants ecs:* wholesale"
  }
}

# CreateService carries tags, so it needs ecs:TagResource -- and TagResource
# takes no cluster parameter, so gating it on `ecs:cluster` denies every service
# the reconciler tries to create. Confining it by ARN instead costs nothing,
# since a service ARN embeds its cluster.
run "tagging_is_granted_without_a_cluster_condition" {
  command = plan

  assert {
    condition = anytrue([
      for s in jsondecode(data.aws_iam_policy_document.backend.json).Statement :
      s.Sid == "TagECSServices"
      && contains(tolist([s.Action]), "ecs:TagResource")
      && !can(s.Condition)
    ])
    error_message = "ecs:TagResource must be granted unconditionally; an ecs:cluster condition can never match it"
  }

  assert {
    condition = alltrue([
      for s in [
        for st in jsondecode(data.aws_iam_policy_document.backend.json).Statement :
        st if st.Sid == "TagECSServices"
        ] : alltrue([
          for r in s.Resource : strcontains(r, ":service/rise-backend/") || strcontains(r, ":task/rise-backend/")
      ])
    ])
    error_message = "ecs:TagResource is not confined to one cluster's service and task ARNs"
  }
}

run "disabling_ecs_emits_no_ecs_statements" {
  command = plan

  variables {
    enable_ecs = false
  }

  assert {
    condition     = !strcontains(data.aws_iam_policy_document.backend.json, "ecs:")
    error_message = "ECS statements leak into the policy when enable_ecs is false"
  }
}
