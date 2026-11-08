terraform {
  backend "local" {}
}

variable "name" {
  type = string
}

# A stand-in for a managed database: only the built-in terraform_data resource,
# so no provider is downloaded. The revision is part of the host, so a new
# revision changes the output the Release consumes.
resource "terraform_data" "database" {
  input = "${var.name}.r%REVISION%.db.internal"
}

output "connection" {
  value = {host = terraform_data.database.output, port = 5432}
}
