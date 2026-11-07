terraform {
  backend "local" {}
}

variable "name" {
  type = string
}

# A stand-in for a managed database: only the built-in terraform_data resource,
# so no provider is downloaded.
resource "terraform_data" "database" {
  input = "${var.name}.db.internal"
}

output "connection" {
  value = {host = terraform_data.database.output, port = 5432}
}
