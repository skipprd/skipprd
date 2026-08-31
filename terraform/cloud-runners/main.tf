# skipprd Deploy Runner bind. This is the product path (not OTP Create*).
# ACME examples/acme/terraform remains the Cloud e2e fixture.

resource "cloud_deploy_runner_github_installation" "app" {
  installation_id = var.github_installation_id
  account_login   = var.github_account_login
  account_type    = var.github_account_type
}

resource "cloud_deploy_runner_repository_binding" "app" {
  repo_id         = var.repo_id
  installation_id = cloud_deploy_runner_github_installation.app.installation_id
  owner           = var.repo_owner
  name            = var.repo_name
  default_branch  = var.default_branch
}

resource "cloud_deploy_runner_pool" "linux_x64_16" {
  pool_id         = var.pool_id
  installation_id = cloud_deploy_runner_github_installation.app.installation_id
  repository_id   = cloud_deploy_runner_repository_binding.app.repo_id
  size            = var.pool_size
  labels          = var.pool_labels
  image           = var.pool_image
  max_concurrent  = var.pool_max_concurrent
  fork_policy     = var.pool_fork_policy
}
