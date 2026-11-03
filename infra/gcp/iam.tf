# Service accounts and Workload Identity Federation.
#
# * tatarbot-vm: the VM's identity. Pulls the bot image, reads the bot's secrets, mints ID
#   tokens for the runtime (run.invoker on that one service), writes logs and metrics. The
#   bot container can't use it (metadata server blocked, templates/startup.sh.tftpl); it
#   only ever sees an ID token whose audience is the runtime's URL.
# * tatarbot-runtime (cloudrun.tf): the Cloud Run service's identity. Reads the runtime's
#   secrets, nothing else.
# * tatarbot-deploy: assumed by GitHub Actions through WIF, only from the production
#   environment of this repository. Pushes images, runs deploy.sh on the VM over IAP SSH,
#   and rolls new revisions of the runtime service.
# * tatarbot-music (storage.tf): owns the HMAC key for the bucket.

data "google_project" "current" {
  project_id = var.project_id

  depends_on = [google_project_service.api]
}

# ---- VM

resource "google_service_account" "vm" {
  account_id   = "${var.name}-vm"
  display_name = "TatarBot VM (image pulls, secrets, logs)"

  depends_on = [google_project_service.api]
}

resource "google_project_iam_member" "vm" {
  for_each = toset(["roles/logging.logWriter", "roles/monitoring.metricWriter"])

  project = var.project_id
  role    = each.value
  member  = google_service_account.vm.member
}

# Only the bot's repository; Cloud Run pulls the runtime image with its own service agent.
resource "google_artifact_registry_repository_iam_member" "vm_pull" {
  location   = google_artifact_registry_repository.service["discord"].location
  repository = google_artifact_registry_repository.service["discord"].name
  role       = "roles/artifactregistry.reader"
  member     = google_service_account.vm.member
}

# ---- GitHub Actions

resource "google_iam_workload_identity_pool" "github" {
  workload_identity_pool_id = "github"
  display_name              = "GitHub Actions"
  description               = "OIDC tokens from GitHub Actions; see the provider's condition."

  depends_on = [google_project_service.api]
}

resource "google_iam_workload_identity_pool_provider" "github" {
  workload_identity_pool_id          = google_iam_workload_identity_pool.github.workload_identity_pool_id
  workload_identity_pool_provider_id = "github-actions"
  display_name                       = "GitHub Actions OIDC"

  oidc {
    issuer_uri = "https://token.actions.githubusercontent.com"
  }

  attribute_mapping = {
    "google.subject"         = "assertion.sub"
    "attribute.repository"   = "assertion.repository"
    "attribute.environment"  = "assertion.environment"
    "attribute.ref"          = "assertion.ref"
    "attribute.workflow_ref" = "assertion.job_workflow_ref"
  }

  # Only jobs in this repository's production environment can exchange a token at all.
  attribute_condition = "assertion.repository == '${var.github_repository}' && assertion.environment == '${var.github_environment}'"
}

resource "google_service_account" "deploy" {
  account_id   = "${var.name}-deploy"
  display_name = "TatarBot deploys from GitHub Actions"

  depends_on = [google_project_service.api]
}

resource "google_service_account_iam_member" "deploy_wif" {
  service_account_id = google_service_account.deploy.name
  role               = "roles/iam.workloadIdentityUser"
  member             = "principalSet://iam.googleapis.com/${google_iam_workload_identity_pool.github.name}/attribute.repository/${var.github_repository}"
}

resource "google_artifact_registry_repository_iam_member" "deploy_push" {
  for_each = google_artifact_registry_repository.service

  location   = each.value.location
  repository = each.value.name
  role       = "roles/artifactregistry.writer"
  member     = google_service_account.deploy.member
}

# gcloud compute ssh reads the instance and the project before connecting.
resource "google_project_iam_custom_role" "deploy_ssh" {
  role_id     = "${var.name}DeploySsh"
  title       = "TatarBot deploy: find the VM"
  description = "Read-only lookups gcloud compute ssh needs."
  permissions = ["compute.instances.get", "compute.projects.get"]
}

resource "google_project_iam_member" "deploy_ssh" {
  project = var.project_id
  role    = google_project_iam_custom_role.deploy_ssh.id
  member  = google_service_account.deploy.member
}

# OS Login with sudo, and IAP tunnelling, on this one instance only.
resource "google_compute_instance_iam_member" "deploy_os_admin" {
  instance_name = google_compute_instance.main.name
  zone          = google_compute_instance.main.zone
  role          = "roles/compute.osAdminLogin"
  member        = google_service_account.deploy.member
}

resource "google_iap_tunnel_instance_iam_member" "deploy" {
  instance = google_compute_instance.main.name
  zone     = google_compute_instance.main.zone
  role     = "roles/iap.tunnelResourceAccessor"
  member   = google_service_account.deploy.member
}

# OS Login on a VM that has a service account requires actAs on that account.
resource "google_service_account_iam_member" "deploy_act_as_vm" {
  service_account_id = google_service_account.vm.name
  role               = "roles/iam.serviceAccountUser"
  member             = google_service_account.deploy.member
}

# Roll new runtime revisions (gcloud run services update --image) on this service only.
resource "google_cloud_run_v2_service_iam_member" "deploy_runtime" {
  name     = google_cloud_run_v2_service.runtime.name
  location = var.region
  role     = "roles/run.developer"
  member   = google_service_account.deploy.member
}

# A new revision runs as tatarbot-runtime, so the deployer needs actAs on it.
resource "google_service_account_iam_member" "deploy_act_as_runtime" {
  service_account_id = google_service_account.runtime.name
  role               = "roles/iam.serviceAccountUser"
  member             = google_service_account.deploy.member
}
