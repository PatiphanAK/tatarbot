terraform {
  required_version = ">= 1.10"

  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 8.0"
    }
  }

  # Local state for now, like infra/aws. Before sharing or automating applies, move it to a
  # GCS backend: backend "gcs" { bucket = "<state bucket>", prefix = "tatarbot" }
}

provider "google" {
  project = var.project_id
  region  = var.region
  zone    = var.zone

  default_labels = {
    project    = var.name
    managed-by = "terraform"
  }

  # Billing Budgets is a client-quota API: bill the calls to this project.
  user_project_override = true
  billing_project       = var.project_id
}
