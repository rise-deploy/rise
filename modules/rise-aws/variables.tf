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
