terraform {
  required_providers {
    docker = {
      source  = "kreuzwerker/docker"
      version = "4.6.0"
    }
  }

  backend "local" {}
}

provider "docker" {}

variable "image" {
  description = "Immutable image reference supplied by the OciImage unit."
  type        = string
}

resource "docker_image" "web" {
  name = var.image
}

resource "docker_container" "web" {
  name  = "nyl-example-web"
  image = docker_image.web.image_id

  ports {
    internal = 80
    external = 8080
    ip       = "127.0.0.1"
  }
}

output "url" {
  value = "http://127.0.0.1:8080"
}
