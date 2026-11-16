terraform {
  backend "local" {}
}

variable "vpc_id" {
  type = string
}

resource "terraform_data" "database" {
  input = "db.${var.vpc_id}.internal"
}

output "connection" {
  value = {host = terraform_data.database.output, port = 5432}
}
