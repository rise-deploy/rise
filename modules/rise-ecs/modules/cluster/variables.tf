variable "name" {
  description = "Installation name used in AWS resource names."
  type        = string
}

variable "tags" {
  description = "Tags applied to managed resources."
  type        = map(string)
}

variable "cluster" {
  description = "Existing ECS cluster name, or null to create a Fargate cluster."
  type = object({
    name = string
  })
}

variable "vpc_id" {
  description = "VPC for the private DNS namespace."
  type        = string
}

variable "discovery" {
  description = "Namespace name and optional existing namespace ID."
  type = object({
    id   = string
    name = string
  })
}

variable "logging" {
  description = "Log group name and retention period, including for a supplied cluster."
  type = object({
    name           = string
    retention_days = number
  })
}

variable "enable_container_insights" {
  description = "Enable Container Insights on a managed cluster."
  type        = bool
}
