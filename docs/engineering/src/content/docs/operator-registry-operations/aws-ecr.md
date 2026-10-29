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

## Deleted projects and `auto_remove`

A project's repository is `<repo_prefix><project>`. When a project is
deleted, `auto_remove = true` deletes its repository and images;
`auto_remove = false` keeps them and tags the repository `rise:orphaned=true`.

An orphaned repository is never handed to a project created later under the
same name: the controller does not adopt it, credentials for it are refused,
and prebuilt-image admission rejects retained images. The project cannot use
that repository until an operator acts.
Delete the repository to give the new project a fresh one. To deliberately
transfer its images, set `rise:project-id` to the new project's UUID, assign a
fresh UUID to `rise:credential-generation`, and remove `rise:orphaned`.

## Credential scope

Push and pull credentials are scoped to one repository, its immutable project
UUID (`rise:project-id`), and its `rise:credential-generation` tag. A repository
owned by a different project UUID is never adopted, even when its name matches.
Pull credentials permit image reads only.
Rise checks repository eligibility before returning cached credentials, and the
session policy also requires the repository to be non-orphaned.

Kubernetes image-pull Secrets record the credential generation and refresh when
Rise observes a different generation. Runtime scope observations are cached for
at most 60 seconds per repository and project UUID; out-of-band repository recreation or tag
rotation therefore converges within that observation window plus the next
reconciliation. Push and pull credential requests check AWS directly and publish
the current generation for runtime refresh. AWS resource-tag conditions still
restrict credentials while a runtime is waiting to observe a rotation.

The controller assigns a fresh generation when creating a repository and
rotates it when retaining a deleted project's repository. Recreating a repository
under the same name or deliberately adopting a controller-retained repository
therefore does not restore access to credentials issued for its previous
generation. Operators must preserve this tag; when manually transferring a
repository outside the controller's cleanup flow, assign a fresh UUID generation
before making the repository available to its new project.

Credentials require matching ownership and a provisioned generation. Retry
credential requests after reconciliation when a managed repository is waiting
for that metadata. An unbound repository without the project's ECR finalizer
requires operator review; delete it or explicitly transfer it as described above.

## EKS + IRSA

For Kubernetes-based production installs, prefer IRSA over static credentials and wire the backend service account to an IAM role.

## Non-AWS Runtime

If Rise runs outside AWS, provision an IAM user/keys path and store credentials in a secure secret store.

## Troubleshooting

### ECR access denied

- Verify controller role can assume push role.
- Verify push-role policy scope and repo prefix alignment.
- Verify target repository exists and naming conventions match.
