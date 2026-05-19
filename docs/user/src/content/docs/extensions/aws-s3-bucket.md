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

## Notes

- Provisioning is synchronous and completes in seconds (unlike RDS which may take minutes).
- Non-empty buckets block deletion when the extension is removed. Use the "Empty bucket and delete" option in the UI to have the controller incrementally empty and delete the bucket.
- One bucket per project is the intended usage for v0.
