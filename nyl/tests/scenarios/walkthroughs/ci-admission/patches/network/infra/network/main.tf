resource "terraform_data" "network" {
  input = "network-b"
}

output "vpcId" {
  value = terraform_data.network.output
}
