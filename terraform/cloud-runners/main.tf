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
  fork_policy     = var.pool_fork_policy
}

resource "cloud_deploy_runner_repository_binding" "cloud" {
  repo_id         = var.cloud_repo_id
  installation_id = cloud_deploy_runner_github_installation.app.installation_id
  owner           = var.repo_owner
  name            = var.cloud_repo_name
  default_branch  = var.default_branch
}

locals {
  cloud_runner_vcpus = toset(["4", "8", "16"])
}

resource "cloud_deploy_runner_pool" "cloud" {
  for_each        = local.cloud_runner_vcpus
  pool_id         = "cloud-linux-x64-${each.key}"
  installation_id = cloud_deploy_runner_github_installation.app.installation_id
  repository_id   = cloud_deploy_runner_repository_binding.cloud.repo_id
  size            = "linux_x64_${each.key}"
  labels          = ["self-hosted", "linux", "x64", "skippr-linux-x64-${each.key}"]
  image           = var.pool_image
  fork_policy     = var.pool_fork_policy
}

resource "cloud_deploy_runner_pool" "private_darwin" {
  pool_id         = "skipprd-darwin-arm64-8"
  installation_id = cloud_deploy_runner_github_installation.app.installation_id
  repository_id   = cloud_deploy_runner_repository_binding.app.repo_id
  size            = "darwin_arm64_8"
  labels          = ["self-hosted", "darwin", "arm64", "skippr-darwin-arm64-8"]
  image           = "darwin-host"
  fork_policy     = var.pool_fork_policy
}

locals {
  oss_repos = {
    skipprd = {
      repo_id         = "321379268"
      name            = "skipprd"
      default_branch  = "main"
    }
    sde = {
      repo_id         = "1366327255"
      name            = "sde"
      default_branch  = "main"
    }
    react = {
      repo_id         = "1172767753"
      name            = "react"
      default_branch  = "main"
    }
    "skippr-ide" = {
      repo_id         = "1238139450"
      name            = "skippr-ide"
      default_branch  = "main"
    }
    skipprdb = {
      repo_id         = "1366327030"
      name            = "skipprdb"
      default_branch  = "main"
    }
    "homebrew-tap" = {
      repo_id         = "1366327136"
      name            = "homebrew-tap"
      default_branch  = "main"
    }
    "vitepress-theme" = {
      repo_id         = "1366326908"
      name            = "vitepress-theme"
      default_branch  = "main"
    }
  }
}

resource "cloud_deploy_runner_repository_binding" "oss" {
  for_each        = local.oss_repos
  repo_id         = each.value.repo_id
  installation_id = cloud_deploy_runner_github_installation.app.installation_id
  owner           = var.repo_owner
  name            = each.value.name
  default_branch  = each.value.default_branch
}

resource "cloud_deploy_runner_pool" "oss_linux" {
  for_each        = local.oss_repos
  pool_id         = "oss-${each.key}-linux-x64-16"
  installation_id = cloud_deploy_runner_github_installation.app.installation_id
  repository_id   = cloud_deploy_runner_repository_binding.oss[each.key].repo_id
  size            = "linux_x64_16"
  labels          = ["self-hosted", "linux", "x64", "skippr-linux-x64-16"]
  image           = var.pool_image
  fork_policy     = var.pool_fork_policy
}

resource "cloud_deploy_runner_pool" "oss_darwin" {
  for_each        = local.oss_repos
  pool_id         = "oss-${each.key}-darwin-arm64-8"
  installation_id = cloud_deploy_runner_github_installation.app.installation_id
  repository_id   = cloud_deploy_runner_repository_binding.oss[each.key].repo_id
  size            = "darwin_arm64_8"
  labels          = ["self-hosted", "darwin", "arm64", "skippr-darwin-arm64-8"]
  image           = "darwin-host"
  fork_policy     = var.pool_fork_policy
}
