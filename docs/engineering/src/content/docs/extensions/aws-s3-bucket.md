---
title: "AWS S3 Bucket Extension"
---

The `aws-s3-bucket` extension provisions an S3 bucket for a project and injects full-access credentials as environment variables into every deployment.

## What It Does

- Creates a dedicated S3 bucket named after the project and extension instance.
- Creates a scoped IAM user with full access to **only** that bucket. The user is created with a permissions boundary that prevents it from ever being granted permissions beyond S3 access on the Rise bucket prefix.
- Generates an IAM access key and securely stores the credentials.
- Injects the bucket name and credentials into deployments automatically.
- On extension deletion: removes the IAM user and access key; deletes the bucket only if empty. Non-empty buckets block deletion — you can choose "Empty bucket and delete" in the UI to have the controller empty and delete the bucket.

## Injected Environment Variables

| Variable | Description |
|---|---|
| `S3_BUCKET_NAME` | Name of the provisioned S3 bucket |
| `AWS_ACCESS_KEY_ID` | IAM access key ID |
| `AWS_SECRET_ACCESS_KEY` | IAM secret access key |
| `AWS_REGION` | AWS region where the bucket is located |

These are recognized by all AWS SDKs, the AWS CLI, and most S3-compatible libraries.

## Configuration (Spec)

No configuration is required for v0. The extension spec is an empty object:

```json
{}
```

## Backend Configuration

Add the `aws-s3-bucket` provider to your backend configuration:

```yaml
extensions:
  providers:
    - type: aws-s3-bucket
      region: us-east-1
      # user_permissions_boundary_arn is the output of the rise-aws Terraform module
      user_permissions_boundary_arn: "arn:aws:iam::123456789012:policy/rise-backend-s3-user-boundary"
      # Prefix for bucket and IAM user names (default: "rise").
      # Must match the Terraform module's s3_bucket_prefix for IAM permissions to work.
      # bucket_prefix: rise
      # Optional: bucket name template (default: "{prefix}-{project_name}-{extension_name}")
      # bucket_name_template: "{prefix}-{project_name}-{extension_name}"
      # Optional: static credentials for development (production uses IAM role/IRSA)
      # access_key_id: ...
      # secret_access_key: ...
```

## Terraform Setup

Enable S3 support in the `rise-aws` Terraform module:

```hcl
module "rise_aws" {
  source = "path/to/modules/rise-aws"

  name       = "rise-backend"
  enable_s3  = true
  # s3_bucket_prefix must match the backend's bucket_prefix (both default differently:
  # Terraform defaults to var.name, backend defaults to "rise"). Set explicitly to match.
  s3_bucket_prefix = "rise"
}
```

Wire the permissions boundary ARN into the Rise backend configuration:

```hcl
# The module outputs the boundary ARN for use in the Rise backend config
output "s3_user_permissions_boundary_arn" {
  value = module.rise_aws.s3_user_permissions_boundary_arn
}
```

## Notes

- Provisioning is synchronous and completes in seconds (unlike RDS which may take minutes).
- Non-empty buckets block deletion when the extension is removed. Use the "Empty bucket and delete" option in the UI to have the controller incrementally empty and delete the bucket.
- One bucket per project is the intended usage for v0.
