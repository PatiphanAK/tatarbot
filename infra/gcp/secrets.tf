# Secret Manager secrets: Terraform creates the empty secrets and who may read them; the
# values (versions) are added by hand with gcloud (README), so they never touch Terraform
# variables, state or the repository.
#
# Consumers: "discord" = the VM's host, which hands them to the bot container at each
# start; "runtime" = Cloud Run, which injects them into the runtime at instance start.
# Each identity can read only the secrets of its own service.

locals {
  history_in_postgres = var.runtime_history_store == "postgres"
  database_consumers = concat(
    var.discord_enabled && var.music_library_enabled ? ["discord"] : [],
    local.history_in_postgres ? ["runtime"] : [],
  )

  secrets = {
    discord_token = { id = "${var.name}-discord-token", env = "DISCORD_TOKEN", consumers = ["discord"], enabled = var.discord_enabled }
    # The runtime's own bearer key, checked behind Cloud Run IAM as defence in depth.
    agent_api_key = { id = "${var.name}-agent-api-key", env = "AGENT_API_KEY", consumers = ["discord", "runtime"], enabled = true }
    model_api_key = { id = "${var.name}-model-api-key", env = "MODEL_API_KEY", consumers = ["runtime"], enabled = true }
    # The runtime enables web search only when this key is set.
    tavily_api_key = { id = "${var.name}-tavily-api-key", env = "TAVILY_API_KEY", consumers = ["runtime"], enabled = var.web_search_enabled }
    # One Supabase database (Supavisor session mode, port 5432, verified TLS) for every
    # workload, each in its own schema: the bot's music library, the runtime's knowledge
    # base (rag) and its state such as history (runtime).
    database_url = { id = "${var.name}-database-url", env = "DATABASE_URL", consumers = local.database_consumers, enabled = length(local.database_consumers) > 0 }
    # Optional: point the runtime schema at another connection string (e.g. a role limited
    # to that schema). Without it, the runtime uses DATABASE_URL.
    runtime_database_url = { id = "${var.name}-runtime-database-url", env = "RUNTIME_DATABASE_URL", consumers = ["runtime"], enabled = local.history_in_postgres && var.runtime_database_url_override }
    # HMAC key of the music service account: the bot's S3 SDK talks to GCS's XML API.
    music_hmac_access_id = { id = "${var.name}-music-hmac-access-id", env = "AWS_ACCESS_KEY_ID", consumers = ["discord"], enabled = var.discord_enabled && var.music_library_enabled }
    music_hmac_secret    = { id = "${var.name}-music-hmac-secret", env = "AWS_SECRET_ACCESS_KEY", consumers = ["discord"], enabled = var.discord_enabled && var.music_library_enabled }
  }
  enabled_secrets = { for k, s in local.secrets : k => s if s.enabled }

  secret_readers = {
    discord = google_service_account.vm.member
    runtime = google_service_account.runtime.member
  }
  secret_access = merge([
    for k, s in local.enabled_secrets : {
      for c in s.consumers : "${k}/${c}" => { secret = k, member = local.secret_readers[c] }
    }
  ]...)
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

# Per-secret access for the consuming service's identity only (no project-wide role).
resource "google_secret_manager_secret_iam_member" "reader" {
  for_each = local.secret_access

  secret_id = google_secret_manager_secret.this[each.value.secret].id
  role      = "roles/secretmanager.secretAccessor"
  member    = each.value.member
}
