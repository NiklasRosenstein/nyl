resource "terraform_data" "vpc" {
  input = "vpc-walkthrough"
}

output "vpcId" {
  value = terraform_data.vpc.output
}
