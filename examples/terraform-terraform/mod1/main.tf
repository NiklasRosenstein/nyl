
resource "tls_private_key" "main" {
  algorithm = "ED25519"
}

output "private_key_pem" {
  value = tls_private_key.main.private_key_pem
}
