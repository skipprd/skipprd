output "repository_url" {
  description = "The URL of the repository."
  value       = "${data.aws_ecr_repository.ecr.repository_url}"
}
