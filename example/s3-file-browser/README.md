# S3 File Browser

A minimal web-based file browser for an S3 bucket. Supports listing, uploading, downloading, and deleting files with folder navigation.

## Environment Variables

| Variable | Required | Description |
|---|---|---|
| `S3_BUCKET_NAME` | Yes | Name of the S3 bucket to browse |
| `AWS_REGION` | Yes | AWS region (set automatically by Rise AWS extensions) |
| `AWS_ACCESS_KEY_ID` | Yes | AWS credentials (set automatically by Rise AWS extensions) |
| `AWS_SECRET_ACCESS_KEY` | Yes | AWS credentials (set automatically by Rise AWS extensions) |

When deployed with Rise using the AWS S3 bucket extension, the AWS credentials and bucket name are injected automatically.

## Local Development

```bash
npm install

# Set your AWS credentials and bucket
export S3_BUCKET_NAME=my-bucket
export AWS_REGION=eu-central-1
export AWS_ACCESS_KEY_ID=...
export AWS_SECRET_ACCESS_KEY=...

npm start
# Visit http://localhost:8080
```

## Deploy with Rise

```bash
rise project create s3-file-browser
rise deployment create s3-file-browser
```
