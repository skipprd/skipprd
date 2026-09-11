terraform {
  required_version = ">= 1.5.0"
  required_providers {
    cloud = {
      source  = "skippr/cloud"
      version = "0.1.0"
    }
  }
}

provider "cloud" {
  region = var.region
}
