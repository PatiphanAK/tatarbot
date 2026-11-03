output "vm_name" {
  value = google_compute_instance.main.name
}

output "vm_external_ip" {
  description = "Ephemeral; changes if the VM is stopped and started. Nothing connects to it."
  value       = google_compute_instance.main.network_interface[0].access_config[0].nat_ip
}

output "shell" {
  description = "Interactive shell over IAP (OS Login; your account needs roles/compute.osAdminLogin)."
  value       = "gcloud compute ssh ${google_compute_instance.main.name} --zone ${var.zone} --project ${var.project_id} --tunnel-through-iap"
}

output "image_repositories" {
  value = local.image_repo
}

output "music_bucket" {
  value = var.music_library_enabled ? google_storage_bucket.music[0].name : null
}

output "music_service_account" {
  description = "Create its HMAC key by hand (README) and store it in the two hmac secrets."
  value       = var.music_library_enabled ? google_service_account.music[0].email : null
}

output "secrets_to_fill" {
  description = "Secret Manager secrets that need a version before the services start."
  value       = { for k, s in local.enabled_secrets : s.env => s.id }
}

output "github_actions_variables" {
  description = "Set these on the GitHub 'production' environment."
  value = {
    GCP_PROJECT_ID   = var.project_id
    GCP_REGION       = var.region
    GCP_ZONE         = var.zone
    GCP_VM_NAME      = google_compute_instance.main.name
    GCP_WIF_PROVIDER = google_iam_workload_identity_pool_provider.github.name
    GCP_DEPLOY_SA    = google_service_account.deploy.email
  }
}
