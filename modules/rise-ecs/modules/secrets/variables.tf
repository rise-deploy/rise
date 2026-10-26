variable "name" {
  description = "Installation name used in AWS resource names."
  type        = string
}

variable "tags" {
  description = "Tags applied to managed resources."
  type        = map(string)
}

variable "oidc_client_secret" {
  sensitive   = true
  description = "OIDC client credential stored in Secrets Manager."
  type        = string
}

variable "recovery_window_days" {
  description = "Secrets Manager recovery window for the managed secrets."
  type        = number
}
