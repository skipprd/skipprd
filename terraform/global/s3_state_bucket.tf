resource "aws_s3_bucket" "terraform_state" {
  bucket = "terraform-state-${var.project_name}"
  acl    = "private"

  versioning {
    enabled = true
  }

  lifecycle {
    prevent_destroy = true
  }

  tags {
    Name        = "terraform-state-${var.project_name}"
    Terraform   = true
    Environment = "All"
    Project     = "${var.project_name}"
  }
}
