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

output "runtime_url" {
  description = "Private Cloud Run URL (internal ingress + IAM): reachable from the VM only."
  value       = google_cloud_run_v2_service.runtime.uri
}

output "image_repositories" {
  value = local.image_repo
}

output "music_bucket" {
  value = var.music_library_enabled ? google_storage_bucket.music[0].name : null
}

output "music_service_account" {
  description = "Create its HMAC key by hand (README) and store it in the two hmac secrets."
  value       = length(google_service_account.music) > 0 ? google_service_account.music[0].email : null
}

output "secrets_to_fill" {
  description = "Secret Manager secrets that need a version before the services start, by consumer."
  value = {
    for c in ["discord", "runtime"] : c => {
      for k, s in local.enabled_secrets : s.env => s.id if contains(s.consumers, c)
    }
  }
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
    # Read by the Cloud Run deploy workflows.
    GCP_RUNTIME_SERVICE = google_cloud_run_v2_service.runtime.name
    GCP_INGEST_SERVICE  = var.ingest_enabled ? google_cloud_run_v2_service.ingest[0].name : ""
  }
}

output "ingest" {
  description = "ADR 0004 ingest worker (null unless ingest_enabled)."
  value = var.ingest_enabled ? {
    url                    = local.ingest_url
    worker_service_account = google_service_account.ingest[0].email
    tasks_invoker          = google_service_account.tasks[0].email
    queue                  = google_cloud_tasks_queue.ingest[0].id
    # Run once after the first deploy (from a machine with DATABASE_URL), see the README.
    register_producer = join(" ", [
      "ingest-worker register-producer --id tatarbot",
      "--service-account ${google_service_account.vm.email}",
      "--tenant-prefix discord: --pipelines audio.opus-v1 --source-kinds https_url",
      "--hosts cdn.discordapp.com,media.discordapp.net --max-bytes 26214400",
    ])
  } : null
}
