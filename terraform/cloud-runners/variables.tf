variable "region" {
  type        = string
  default     = "eu-central-1"
  description = "Skippr Cloud region (CLOUD_REGION)."
}

variable "github_installation_id" {
  type        = string
  default     = "156978374"
  description = "Skippr Cloud Runners GitHub App installation id on the skipprd org."
}

variable "github_account_login" {
  type        = string
  default     = "skipprd"
  description = "GitHub account login for the installation."
}

variable "github_account_type" {
  type        = string
  default     = "Organization"
  description = "GitHub account type for the installation."
}

variable "repo_id" {
  type        = string
  default     = "678245671"
  description = "Numeric GitHub repository id for skipprd/skipprd-private."
}

variable "repo_owner" {
  type    = string
  default = "skipprd"
}

variable "repo_name" {
  type    = string
  default = "skipprd-private"
}

variable "default_branch" {
  type    = string
  default = "master"
}

variable "pool_id" {
  type    = string
  default = "skipprd-linux-x64-16"
}

variable "pool_size" {
  type    = string
  default = "linux_x64_16"
}

variable "pool_labels" {
  type    = list(string)
  default = ["self-hosted", "linux", "x64", "skippr-linux-x64-16"]
}

variable "pool_image" {
  type    = string
  default = "skippr-ubuntu-24.04-x64"
}

variable "pool_fork_policy" {
  type        = string
  default     = "deny"
  description = "GitHub fork run policy for this pool."
}

variable "cloud_repo_id" {
  type        = string
  default     = "1311314317"
  description = "Numeric GitHub repository id for skipprd/cloud."
}

variable "cloud_repo_name" {
  type    = string
  default = "cloud"
}
