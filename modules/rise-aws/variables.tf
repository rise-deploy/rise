variable "name" {
  description = "Name for the IAM roles and policies (e.g., 'rise-backend', 'rise-prod-backend'). Also used as ECR repository prefix."
  type        = string
  default     = "rise-backend"
}

variable "tags" {
  description = "Tags to apply to all resources"
  type        = map(string)
  default     = {}
}

# Authentication method - choose one

variable "create_iam_user" {
  description = "Create an IAM user with access keys for the Rise backend (use for non-AWS deployments)"
  type        = bool
  default     = false
}

variable "assume_role_services" {
  description = <<-EOT
    AWS service principals allowed to assume the backend role, e.g.
    ["ecs-tasks.amazonaws.com"] when Rise runs as an ECS task and takes its
    credentials from the task role. Empty means only the account root may
    assume it. Ignored when irsa_oidc_provider_arn is set, which selects the
    EKS federated trust policy instead.
  EOT
  type        = list(string)
  default     = []
}

variable "irsa_oidc_provider_arn" {
  description = "OIDC provider ARN for IRSA (IAM Roles for Service Accounts). Required if using EKS."
  type        = string
  default     = null
}

variable "irsa_namespace" {
  description = "Kubernetes namespace where the Rise backend runs (for IRSA)"
  type        = string
  default     = "rise-system"
}

variable "irsa_service_account" {
  description = "Kubernetes service account name for the Rise backend (for IRSA)"
  type        = string
  default     = "rise-backend"
}

# Feature flags

variable "enable_ecr" {
  description = "Enable ECR permissions for the Rise backend. Set to true if using ECR for container registry."
  type        = bool
  default     = true
}

variable "enable_rds" {
  description = "Enable RDS permissions for the Rise backend. Set to true if using the AWS RDS extension."
  type        = bool
  default     = false
}

variable "rds_vpc_id" {
  description = "VPC ID where RDS instances will be created. Required if enable_rds = true."
  type        = string
  default     = null
}

variable "rds_allowed_security_groups" {
  description = "List of security group IDs allowed to access RDS instances (e.g., your EKS cluster security group)"
  type        = list(string)
  default     = []
}

variable "rds_allowed_cidr_blocks" {
  description = "List of CIDR blocks allowed to access RDS instances on PostgreSQL port (5432)"
  type        = list(string)
  default     = []
}

variable "rds_subnet_ids" {
  description = "List of subnet IDs for the RDS subnet group (must be in the VPC specified by rds_vpc_id)"
  type        = list(string)
  default     = []
}

# ECR settings

variable "image_tag_mutability" {
  description = "The tag mutability setting for repositories created by the controller"
  type        = string
  default     = "MUTABLE"

  validation {
    condition     = contains(["MUTABLE", "IMMUTABLE"], var.image_tag_mutability)
    error_message = "image_tag_mutability must be either MUTABLE or IMMUTABLE"
  }
}

variable "scan_on_push" {
  description = "Enable image scanning on push for repositories created by the controller"
  type        = bool
  default     = true
}

variable "enable_kms" {
  description = "Enable KMS encryption. If true, a KMS key will be automatically created for encryption. If false, AES256 encryption is used where applicable."
  type        = bool
  default     = false
}

variable "kms_key_alias" {
  description = "Override the KMS key alias name. Defaults to '{name}' for new deployments. For backwards compatibility with existing deployments, set to '{name}-ecr'."
  type        = string
  default     = null
}

variable "enable_s3" {
  description = "Enable S3 bucket provisioning permissions for the Rise backend. Set to true if using the AWS S3 bucket extension."
  type        = bool
  default     = false
}

variable "s3_bucket_prefix" {
  description = "Prefix for S3 bucket and IAM user names managed by Rise. Must match the bucket_prefix configured in the Rise backend. Defaults to var.name."
  type        = string
  default     = null
}

# Lifecycle policies

variable "max_image_count" {
  description = "Maximum number of images to retain per repository"
  type        = number
  default     = 100
}

# -----------------------------------------------------------------------------
# ECS deployment controller (ADR-0005 D14)
#
# This module makes IAM, not infrastructure: the cluster, namespace and log group
# are created elsewhere (modules/rise-ecs, or by hand). It scopes its policies by
# interpolating ARNs from *names* rather than accepting ARNs as inputs, exactly as
# the ECR/RDS/S3 sections already do. That is what keeps the two modules free of a
# dependency cycle -- rise-ecs consumes role ARNs from here, so nothing here may
# consume a resource reference from there.
# -----------------------------------------------------------------------------

variable "enable_ecs" {
  description = "Create the ECS task execution role and add the deployment-controller statements to the backend policy."
  type        = bool
  default     = false
}

variable "ecs_cluster_name" {
  description = "Name of the ECS cluster Rise reconciles. Defaults to var.name; must match the cluster rise-ecs creates or the one you already run."
  type        = string
  default     = null
}

variable "ecs_execution_role_name" {
  description = "Name for the ECS task execution role this module creates. Defaults to \"<name>-ecs-execution\"."
  type        = string
  default     = null
}

variable "ssm_parameter_prefix" {
  description = <<-EOT
    Path prefix for the SSM SecureString parameters carrying secret environment
    variables. Must equal deployment_controller.ssm_parameter_prefix, or the
    controller loses access to the parameters it wrote itself.
  EOT
  type        = string
  default     = "rise"
}

variable "ssm_kms_key_arn" {
  description = <<-EOT
    CMK the SSM SecureStrings are encrypted with. Null means the AWS-managed
    alias/aws/ssm key, which needs no explicit grant.
  EOT
  type        = string
  default     = null
}

variable "ecs_secret_arns" {
  description = <<-EOT
    Secrets Manager ARNs the task execution role may read, for values injected
    into containers through the task definition's `secrets` block -- the control
    plane's own DATABASE_URL and signing keys, and any
    repository_credentials_secret_arn for a private non-ECR registry. Secrets
    Manager appends a random suffix to every ARN, so these are usually prefix
    patterns ending in `-*`.
  EOT
  type        = list(string)
  default     = []
}
