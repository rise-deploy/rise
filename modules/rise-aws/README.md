# Rise AWS Terraform Module

This module creates the AWS IAM resources required for the Rise backend to manage AWS services and deploy applications onto ECS.

## Features

- **Controller role**: Manages the enabled providers and ECS deployment controller
- **Push role**: Separate role for generating scoped image push credentials
- Separate ECS execution, application, and Traefik discovery roles
- Optional common permissions boundary on every generated role
- Inline policy mode for deployers that may create roles but not customer-managed policies
- Supports IAM role (for AWS deployments with IRSA or instance profiles)
- Supports IAM user with access keys (for non-AWS deployments)
- Configurable repository prefix for multi-tenant isolation
- Optional auto-remove or soft-delete (orphan tagging) modes
- Default lifecycle policy to limit image retention
- RDS instance management for project extensions

## Architecture

The module creates distinct identities for each responsibility:

1. **Backend Role** (e.g., `rise-backend`): Used by the Rise backend to manage AWS resources
   - ECR repositories: Create/delete, tag, list for discovery
   - RDS instances: Create/delete/modify PostgreSQL instances with `rise-*` prefix
   - Security groups and subnet groups for RDS networking
   - KMS encryption (if enabled)

2. **Push Role** (e.g., `rise-backend-ecr-push`): Assumed by the backend to generate scoped credentials
   - Push images to ECR
   - The backend uses STS AssumeRole with an inline session policy to scope credentials to specific repositories

3. **ECS execution role**: ECS image pulls, logs, and task secret resolution

4. **Application task role**: The identity applications run as; it has no controller policy

5. **Traefik task role**: Read-only ECS, EC2, and SSM discovery calls

## Usage

### Basic Usage (IAM Role)

```hcl
module "rise_aws" {
  source = "./modules/rise-aws"

  name       = "rise-backend"
  enable_ecr = true  # Enable for ECR container registry
  enable_rds = true  # Enable for AWS RDS extension

  tags = {
    Environment = "production"
  }
}
```

### Delegated ECS installation

```hcl
module "rise_aws" {
  source = "./modules/rise-aws"

  name                     = "rise-prod-controller"
  enable_ecr               = true
  enable_ecs               = true
  enable_rds               = false
  iam_policy_mode          = "inline"
  permissions_boundary_arn = var.runtime_boundary_arn

  controller_role_name    = "rise-prod-control-plane"
  ecr_repository_prefix   = "rise-prod/"
  ecr_push_role_name      = "rise-prod-ecr-push"
  ecs_cluster_name        = "platform"
  ecs_log_group_name      = "/rise-prod"
  ecs_execution_role_name = "rise-prod-execution"
  ecs_workload_role_name  = "rise-prod-app"
  ecs_traefik_role_name   = "rise-prod-traefik"
  ssm_parameter_prefix    = "rise-prod"
}
```

### With IRSA (EKS)

```hcl
module "rise_aws" {
  source = "./modules/rise-aws"

  name                   = "rise-backend"
  enable_ecr             = true
  enable_rds             = true
  enable_kms             = true  # Enable KMS encryption
  irsa_oidc_provider_arn = module.eks.oidc_provider_arn
  irsa_namespace         = "rise-system"
  irsa_service_account   = "rise-backend"
}
```

### With RDS and VPC Configuration

```hcl
module "rise_aws" {
  source = "./modules/rise-aws"

  name       = "rise-backend"
  enable_ecr = true
  enable_rds = true

  # RDS VPC configuration
  rds_vpc_id                  = module.vpc.vpc_id
  rds_subnet_ids              = module.vpc.private_subnets  # Subnets for RDS instances
  rds_allowed_security_groups = [
    module.eks.cluster_security_group_id  # Allow access from EKS cluster
  ]
  rds_allowed_cidr_blocks = [
    "10.0.0.0/16"  # Optional: Allow access from specific CIDR blocks
  ]
}

# Use the rise_config output in your Rise backend config
output "rise_config" {
  value = module.rise_aws.rise_config
}
```

### With IAM User (Non-AWS Deployment)

```hcl
module "rise_aws" {
  source = "./modules/rise-aws"

  name            = "rise-backend"
  enable_ecr      = true
  enable_rds      = true
  create_iam_user = true
}

# Store credentials securely
resource "aws_secretsmanager_secret" "rise_backend_creds" {
  name = "rise/backend-credentials"
}

resource "aws_secretsmanager_secret_version" "rise_backend_creds" {
  secret_id = aws_secretsmanager_secret.rise_backend_creds.id
  secret_string = jsonencode({
    access_key_id     = module.rise_aws.access_key_id
    secret_access_key = module.rise_aws.secret_access_key
  })
}
```

## Rise Backend Configuration

After applying this module, use the `rise_config` output to configure your Rise backend:

```yaml
# config/default.yaml

# ECR registry configuration (if enable_ecr = true)
registry:
  type: ecr
  region: eu-west-1                   # From rise_config.ecr.region
  account_id: "123456789012"          # From rise_config.ecr.account_id
  repo_prefix: rise/                  # From rise_config.ecr.repo_prefix
  role_arn: arn:aws:iam::...          # From rise_config.ecr.role_arn
  push_role_arn: arn:aws:iam::...     # From rise_config.ecr.push_role_arn

# RDS extension configuration (if enable_rds = true)
extensions:
  providers:
    - type: aws-rds-provisioner
      name: aws-rds
      region: eu-west-1
      instance_size: db.t3.micro
      disk_size: 20
      default_engine_version: "18.2"  # Default PostgreSQL version
      # VPC configuration from Terraform outputs:
      vpc_security_group_ids:         # From rise_config.rds.vpc_security_group_ids
        - sg-0123456789abcdef0
      db_subnet_group_name: my-group  # From rise_config.rds.db_subnet_group_name
```

**Finding Available PostgreSQL Versions:**
```bash
# List all available PostgreSQL versions in your region
aws rds describe-db-engine-versions \
  --engine postgres \
  --region eu-west-1 \
  --query "DBEngineVersions[*].EngineVersion" \
  --output table
```

**Simplified Configuration with Terraform Outputs:**

The `rise_config` output is structured to match Rise's configuration format:
- `rise_config.ecr.*` - Maps directly to `registry:` section
- `rise_config.rds.*` - Maps directly to extension provider settings

This makes it easy to reference in your configuration management tool (e.g., using `templatefile()` in Terraform).

## Inputs

| Name | Description | Type | Default | Required |
|------|-------------|------|---------|:--------:|
| name | Name for the IAM role and policy | `string` | `"rise-backend"` | no |
| controller_role_name | Control-plane IAM role name | `string` | `<name>` | no |
| permissions_boundary_arn | Boundary attached to every generated role | `string` | `null` | no |
| iam_policy_mode | `managed` or `inline` generated policies | `string` | `"managed"` | no |
| tags | Tags to apply to all resources | `map(string)` | `{}` | no |
| enable_ecr | Enable ECR permissions | `bool` | `true` | no |
| ecr_repository_prefix | Repository path prefix | `string` | `<name>/` | no |
| ecr_push_role_name | ECR publisher role name | `string` | `<name>-ecr-push` | no |
| enable_rds | Enable RDS permissions | `bool` | `false` | no |
| rds_vpc_id | VPC ID for RDS resources | `string` | `null` | no |
| rds_subnet_ids | Subnet IDs for RDS DB subnet group | `list(string)` | `[]` | no |
| rds_allowed_security_groups | Security groups allowed to access RDS | `list(string)` | `[]` | no |
| rds_allowed_cidr_blocks | CIDR blocks allowed to access RDS on port 5432 | `list(string)` | `[]` | no |
| enable_kms | Enable KMS encryption | `bool` | `false` | no |
| kms_key_alias | Override KMS key alias name (defaults to `{name}`, use `{name}-ecr` for backwards compatibility) | `string` | `null` | no |
| create_iam_user | Create an IAM user with access keys | `bool` | `false` | no |
| irsa_oidc_provider_arn | OIDC provider ARN for IRSA | `string` | `null` | no |
| irsa_namespace | Kubernetes namespace for IRSA | `string` | `"rise-system"` | no |
| irsa_service_account | Kubernetes service account for IRSA | `string` | `"rise-backend"` | no |
| image_tag_mutability | Tag mutability for repositories | `string` | `"MUTABLE"` | no |
| scan_on_push | Enable image scanning on push | `bool` | `true` | no |
| max_image_count | Max images to retain per repository | `number` | `100` | no |
| enable_ecs | Enable ECS controller identities and policies | `bool` | `false` | no |
| ecs_log_group_name | CloudWatch log group read by the ECS controller | `string` | `null` | when `enable_ecs` |
| ecs_cluster_name | Existing cluster Rise reconciles | `string` | `<name>` | no |
| ecs_execution_role_name | ECS execution role name | `string` | `<name>-ecs-execution` | no |
| ecs_workload_role_name | Application task role name | `string` | `<name>-app` | no |
| ecs_traefik_role_name | Traefik discovery role name | `string` | `<name>-traefik` | no |

## Outputs

### Primary Output (Use This!)

| Name | Description |
|------|-------------|
| rise_config | **Configuration values for Rise backend** - Contains `ecr.*` and `rds.*` sub-objects with all values needed for Rise configuration |

### Additional Outputs

| Name | Description |
|------|-------------|
| role_arn | ARN of the controller IAM role |
| role_name | Name of the controller IAM role |
| push_role_arn | ARN of the push IAM role (ECR only) |
| push_role_name | Name of the push IAM role (ECR only) |
| ecs_execution_role_arn | ECS execution role ARN |
| ecs_task_role_arn | Application task role ARN |
| ecs_traefik_role_arn | Traefik discovery role ARN |
| user_arn | ARN of the IAM user (if created) |
| user_name | Name of the IAM user (if created) |
| access_key_id | Access key ID (sensitive, if IAM user created) |
| secret_access_key | Secret access key (sensitive, if IAM user created) |
| controller_policy_arn | ARN of the controller IAM policy in managed mode; otherwise null |
| push_policy_arn | ARN of the push IAM policy in managed mode; otherwise null |
| policy_document | The controller IAM policy document JSON |
| lifecycle_policy | ECR lifecycle policy JSON |
| kms_key_arn | ARN of the KMS key (if KMS enabled) |
| kms_key_id | ID of the KMS key (if KMS enabled) |

## IAM Permissions

### Controller Role

**ECR Permissions:**
- `ecr:GetAuthorizationToken` - Required for any ECR operation
- `ecr:DescribeRepositories`, `ecr:ListTagsForResource` - For discovery
- `ecr:CreateRepository`, `ecr:TagResource`, `ecr:PutImageScanningConfiguration`, `ecr:PutImageTagMutability`, `ecr:PutLifecyclePolicy` - For creating repos
- `ecr:DeleteRepository`, `ecr:BatchDeleteImage` - For deleting repos
- `sts:AssumeRole` on push role - To generate scoped credentials

**RDS Permissions (if `enable_rds = true`):**
- `rds:CreateDBInstance`, `rds:DeleteDBInstance`, `rds:DescribeDBInstances`, `rds:ModifyDBInstance` - For managing database instances
- `rds:ListTagsForResource`, `rds:AddTagsToResource`, `rds:RemoveTagsFromResource` - For tagging
- `rds:CreateDBSubnetGroup`, `rds:DeleteDBSubnetGroup`, `rds:DescribeDBSubnetGroups` - For VPC placement
- `ec2:DescribeSecurityGroups`, `ec2:CreateSecurityGroup`, `ec2:DeleteSecurityGroup` - For network security
- `ec2:AuthorizeSecurityGroupIngress`, `ec2:RevokeSecurityGroupIngress` - For security group rules
- `ec2:DescribeVpcs`, `ec2:DescribeSubnets` - For VPC discovery

**RDS Service-Linked Role (Prerequisite):**

Before using this module with `enable_rds = true`, you must ensure the RDS service-linked role (`AWSServiceRoleForRDS`) exists in your AWS account. This role is required for RDS to manage resources on your behalf and only needs to be created once per AWS account.

To create the service-linked role, run this AWS CLI command or use Terraform in your root module:

```bash
# Using AWS CLI
aws iam create-service-linked-role --aws-service-name rds.amazonaws.com
```

```hcl
# Using Terraform in your root module
resource "aws_iam_service_linked_role" "rds" {
  aws_service_name = "rds.amazonaws.com"
  description      = "Service-linked role for Amazon RDS"
}
```

**Note:** If the role already exists, the CLI command will fail with an error, which is safe to ignore. Terraform will detect the existing role and skip creation if you've already run it once.

**RDS VPC Resources:**
If `enable_rds = true`, `rds_vpc_id`, and `rds_subnet_ids` are provided, the module creates:

1. **DB Subnet Group** - Defines which subnets RDS instances can use
   - Created with the subnet IDs you provide via `rds_subnet_ids`
   - Use the output `rds_subnet_group_name` in your Rise config's `db_subnet_group_name`

2. **Security Group** - Controls network access to RDS instances
   - Allows inbound PostgreSQL (port 5432) from security groups in `rds_allowed_security_groups`
   - Allows all outbound traffic for updates and maintenance
   - Use the output `rds_security_group_id` in your Rise config's `vpc_security_group_ids`

Both resources are automatically tagged and named consistently with the module's naming convention.

**RDS Instance Tags:**
All RDS instances created by Rise are automatically tagged with:
- `rise:managed = "true"` - Indicates the instance is managed by Rise
- `rise:project = "{project_name}"` - Links the instance to the Rise project

These tags are useful for cost allocation, resource discovery, and operational management.

**KMS Permissions (if `enable_kms = true`):**
- `kms:Encrypt`, `kms:Decrypt`, `kms:GenerateDataKey*`, `kms:DescribeKey` - For KMS encryption
- The KMS key alias name changed from `{name}-ecr` to `{name}` to better reflect its general-purpose use
- For backwards compatibility with existing deployments, set `kms_key_alias = "{name}-ecr"` to preserve the old naming

**Note:** ECR permissions are scoped to `rise/*` repositories. RDS permissions (if enabled) are scoped to `rise-*` instance names.

### Push Role

- `ecr:GetAuthorizationToken` - For docker login
- `ecr:BatchCheckLayerAvailability`, `ecr:InitiateLayerUpload`, `ecr:UploadLayerPart`, `ecr:CompleteLayerUpload`, `ecr:PutImage` - For pushing images
- `ecr:BatchGetImage`, `ecr:GetDownloadUrlForLayer` - For pulling images

All permissions are scoped to `${repo_prefix}*`.
