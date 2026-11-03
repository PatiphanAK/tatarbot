# One Docker repository per service, immutable tags (sha-<commit>), and a cleanup policy
# that keeps the newest registry_keep_images tagged images (the rollback window).

locals {
  services      = toset(["runtime", "discord"])
  registry_host = "${var.region}-docker.pkg.dev"
}

resource "google_artifact_registry_repository" "service" {
  for_each = local.services

  repository_id = "${var.name}-${each.key}"
  location      = var.region
  format        = "DOCKER"
  description   = "TatarBot ${each.key} images (linux/amd64), tagged sha-<commit>."

  docker_config {
    immutable_tags = true
  }

  cleanup_policy_dry_run = false

  cleanup_policies {
    id     = "keep-newest"
    action = "KEEP"
    most_recent_versions {
      keep_count = var.registry_keep_images
    }
  }

  cleanup_policies {
    id     = "delete-older"
    action = "DELETE"
    condition {
      tag_state  = "ANY"
      older_than = "604800s" # 7 days; the KEEP policy above wins for the newest images
    }
  }

  depends_on = [google_project_service.api]
}

locals {
  # <host>/<project>/<repo>; the deploy script only accepts images under this prefix.
  registry_prefix = "${local.registry_host}/${var.project_id}"
  image_repo      = { for k, r in google_artifact_registry_repository.service : k => "${local.registry_prefix}/${r.repository_id}" }
}
