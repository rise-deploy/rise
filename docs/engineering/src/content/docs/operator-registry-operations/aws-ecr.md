---
title: "AWS ECR"
---

## Architecture: Two-Role Pattern

**Controller Role (`rise-backend`)**:
- Create/delete ECR repositories
- Tag repositories (managed, orphaned)
- Configure repository settings
- Assume the push role

**Push Role (`rise-backend-ecr-push`)**:
- Push/pull images to ECR (under configured prefix)
- Used by backend to generate scoped credentials for CLI workflows

## Terraform Module

Use `modules/rise-aws` to provision ECR access patterns:

```hcl
module "rise_ecr" {
  source = "../modules/rise-aws"

  name        = "rise-backend"
  repo_prefix = "rise/"
  auto_remove = false
}
```

## EKS + IRSA

For Kubernetes-based production installs, prefer IRSA over static credentials and wire the backend service account to an IAM role.

## Non-AWS Runtime

If Rise runs outside AWS, provision an IAM user/keys path and store credentials in a secure secret store.

## Troubleshooting

### ECR access denied

- Verify controller role can assume push role.
- Verify push-role policy scope and repo prefix alignment.
- Verify target repository exists and naming conventions match.
