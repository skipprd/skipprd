terraform {
  backend "s3" {
    bucket  = "terraform-state-skipprd"
    key     = "global/terraform.tfstate"
    region  = "eu-west-2"
    encrypt = true
    profile = "skippr"
  }
}

provider "aws" {
  shared_credentials_file = "${var.aws_shared_credentials_file}"
  profile                 = "${var.aws_profile}"
  region                  = "${var.aws_region}"
}
