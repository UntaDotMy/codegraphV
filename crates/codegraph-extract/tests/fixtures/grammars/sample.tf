variable "region" { default = "us-east-1" }
module "vpc" {
  source = "./modules/vpc"
  region = var.region
}
resource "aws_s3_bucket" "b" { bucket = "x-${var.region}" }
