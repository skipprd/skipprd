//data "aws_vpc" "main" {
//  filter {
//    name   = "tag:Name"
//    values = ["${terraform.workspace == "default" ? "Default VPC" : "${terraform.workspace}-vpc"}"]
//  }
//}

data "aws_caller_identity" "current" {}

data "aws_route53_zone" "main" {
  name         = "${var.domain}"
  private_zone = false
}

data "aws_route53_zone" "env" {
  name         = "${terraform.workspace}.${var.domain}"
  private_zone = false
}

data "aws_ecr_repository" "ecr" {
  name = "${var.project_name}"
}
