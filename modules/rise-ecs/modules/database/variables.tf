variable "name" {
  description = "Installation name used in AWS resource names."
  type        = string
}

variable "tags" {
  description = "Tags applied to managed resources."
  type        = map(string)
}

variable "enabled" {
  description = "Create the PostgreSQL instance, credentials and connection URL secret."
  type        = bool
}

variable "network" {
  description = "Database subnets and control-plane database security group."
  type = object({
    subnet_ids        = list(string)
    security_group_id = string
  })
}

variable "postgres" {
  description = "PostgreSQL sizing, version, availability, backup and deletion protection settings."
  type = object({
    engine_version        = string
    instance_class        = string
    allocated_storage     = number
    multi_az              = bool
    backup_retention_days = number
    deletion_protection   = bool
  })
}

variable "secret_recovery_window_days" {
  description = "Secrets Manager recovery window for the database URL."
  type        = number
}
