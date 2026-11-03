# APIs this configuration uses. Disabling them on destroy is left off: other things in the
# project may need them, and re-enabling is slow.

locals {
  apis = toset([
    "artifactregistry.googleapis.com",
    "billingbudgets.googleapis.com",
    "cloudscheduler.googleapis.com",
    "cloudtasks.googleapis.com",
    "cloudresourcemanager.googleapis.com",
    "compute.googleapis.com",
    "iam.googleapis.com",
    "iamcredentials.googleapis.com",
    "iap.googleapis.com",
    "logging.googleapis.com",
    "monitoring.googleapis.com",
    "oslogin.googleapis.com",
    "pubsub.googleapis.com",
    "run.googleapis.com",
    "secretmanager.googleapis.com",
    "storage.googleapis.com",
    "sts.googleapis.com",
  ])
}

resource "google_project_service" "api" {
  for_each = local.apis

  service            = each.value
  disable_on_destroy = false
}
