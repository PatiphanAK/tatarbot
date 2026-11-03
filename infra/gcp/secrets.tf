# Secret Manager secrets: Terraform creates the empty secrets and who may read them; the
# values (versions) are added by hand with gcloud (README), so they never touch Terraform
# variables, state or the repository. The host reads them at every container start.

locals {
  secrets = {
    discord_token = { id = "${var.name}-discord-token", env = "DISCORD_TOKEN", services = ["discord"], enabled = var.discord_enabled }
    agent_api_key = { id = "${var.name}-agent-api-key", env = "AGENT_API_KEY", services = ["discord", "runtime"], enabled = true }
    model_api_key = { id = "${var.name}-model-api-key", env = "MODEL_API_KEY", services = ["runtime"], enabled = var.runtime_enabled }
    # The runtime enables web search only when this key is set.
    tavily_api_key = { id = "${var.name}-tavily-api-key", env = "TAVILY_API_KEY", services = ["runtime"], enabled = var.runtime_enabled && var.web_search_enabled }
    # Supabase via Supavisor session mode, verified TLS (discord_bot/README.md).
    database_url = { id = "${var.name}-database-url", env = "DATABASE_URL", services = ["discord"], enabled = var.discord_enabled && var.music_library_enabled }
    # HMAC key of the music service account: the bot's S3 SDK talks to GCS's XML API.
    music_hmac_access_id = { id = "${var.name}-music-hmac-access-id", env = "AWS_ACCESS_KEY_ID", services = ["discord"], enabled = var.discord_enabled && var.music_library_enabled }
    music_hmac_secret    = { id = "${var.name}-music-hmac-secret", env = "AWS_SECRET_ACCESS_KEY", services = ["discord"], enabled = var.discord_enabled && var.music_library_enabled }
  }
  enabled_secrets = { for k, s in local.secrets : k => s if s.enabled }
}

resource "google_secret_manager_secret" "this" {
  for_each = local.enabled_secrets

  secret_id = each.value.id

  replication {
    user_managed {
      replicas {
        location = var.region
      }
    }
  }

  depends_on = [google_project_service.api]
}

# Per-secret access for the VM's service account only (no project-wide accessor role).
resource "google_secret_manager_secret_iam_member" "vm" {
  for_each = local.enabled_secrets

  secret_id = google_secret_manager_secret.this[each.key].id
  role      = "roles/secretmanager.secretAccessor"
  member    = google_service_account.vm.member
}
