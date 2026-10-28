variable "vpc_id" {
  type = string
}

resource "terraform_data" "database" {
  input = "db.${var.vpc_id}.internal"
}

output "host" {
  value = terraform_data.database.output
}
