
variable "private_key_pem" {
  type      = string
  sensitive = true
}

resource "tls_self_signed_cert" "main" {
  private_key_pem = var.private_key_pem

  is_ca_certificate = true

  subject {
    country             = "Multiverse"
    province            = "Galaxy"
    locality            = "Universe"
    common_name         = "Test Root CA"
    organization        = "Test Software Solutions Pvt Ltd."
    organizational_unit = "Test Root Certification Auhtority"
  }

  validity_period_hours = 365 * 24

  allowed_uses = [
    "digital_signature",
    "cert_signing",
    "crl_signing",
  ]
}

output "cert_pem"  {
  value = tls_self_signed_cert.main.cert_pem
}
