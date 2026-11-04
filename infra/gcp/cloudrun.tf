# The Agent Runtime as a private Cloud Run service in the same region as the VM.
#
# * Private twice over: ingress is internal (only VPC networks of this project, which
#   includes the VM even though it has an external IP), and invocation needs IAM
#   (roles/run.invoker, granted to the VM's service account only). No Serverless VPC
#   Access connector or NAT: the runtime's egress (Qwen, Tavily, Supabase) goes out
#   through Cloud Run's own internet access.
# * Request-based billing (cpu_idle): CPU is allocated and billed only while a request
#   runs. Nothing may run between requests: the runtime keeps history in Postgres and has
#   no background work unless the knowledge base is enabled (README).
# * Scale to zero by default. The first message after ~15 idle minutes pays a cold start
#   (README, "Cold starts"); history survives it (CONVERSATION_STORE=postgres).
# * Terraform owns the configuration; the deploy workflow owns the image (ignore_changes).
#   The first apply runs Google's public sample image until the first deploy.

locals {
  runtime_service_name = "${var.name}-runtime"
  # Placeholder for the first apply only (listens on $PORT=8080).
  runtime_bootstrap_image = "us-docker.pkg.dev/cloudrun/container/hello"

  # The active model provider. Alibaba (Qwen): model_base_url / model_name, thinking left
  # at the runtime default (false). OpenAI: its API, no Qwen thinking field (OpenAI rejects
  # unknown fields), and the key from the separate openai-api-key secret.
  model = var.model_provider == "openai" ? {
    base_url   = "https://api.openai.com/v1"
    name       = var.openai_model_name
    timeout    = var.openai_model_timeout_seconds
    thinking   = "omit"
    secret_key = "openai_api_key"
    } : {
    base_url   = var.model_base_url
    name       = var.model_name
    timeout    = var.model_timeout_seconds
    thinking   = null
    secret_key = "model_api_key"
  }
  model_secret_keys = ["model_api_key", "openai_api_key"]

  runtime_env = merge(var.runtime_extra_environment, {
    UVICORN_HOST          = "0.0.0.0"
    UVICORN_PORT          = tostring(var.runtime_port)
    MODEL_BASE_URL        = local.model.base_url
    MODEL_NAME            = local.model.name
    MODEL_TIMEOUT_SECONDS = tostring(local.model.timeout)
    # Whole turn incl. web searches. Leaves the bot 15 s for a cold start and to report a
    # timeout itself.
    TURN_TIMEOUT_SECONDS = tostring(var.agent_timeout_secs - 15)
    CONVERSATION_STORE   = var.runtime_history_store
    }, local.history_in_postgres ? {
    CONVERSATION_RETENTION_DAYS = tostring(var.conversation_retention_days)
    # One instance at most; Supabase's session pooler has a small per-user client limit
    # that the bot shares.
    DATABASE_POOL_SIZE = "2"
    } : {}, local.model.thinking == null ? {} : {
    MODEL_ENABLE_THINKING = local.model.thinking
  })

  # Env name -> secret id. Of the model keys, only the active provider's is injected (both
  # stay readable by the runtime, so switching back needs no IAM change).
  runtime_secrets = merge(
    {
      for k, s in local.enabled_secrets : s.env => s.id
      if contains(s.consumers, "runtime") && !contains(local.model_secret_keys, k)
    },
    { MODEL_API_KEY = local.secrets[local.model.secret_key].id },
  )
}

resource "google_service_account" "runtime" {
  account_id   = "${var.name}-runtime"
  display_name = "TatarBot Agent Runtime (Cloud Run)"

  depends_on = [google_project_service.api]
}

resource "google_cloud_run_v2_service" "runtime" {
  name                = local.runtime_service_name
  location            = var.region
  description         = "TatarBot Agent Runtime: private, called only by the Discord bot VM."
  ingress             = "INGRESS_TRAFFIC_INTERNAL_ONLY"
  deletion_protection = false # stateless; the image and config are re-deployable

  template {
    service_account                  = google_service_account.runtime.email
    timeout                          = "${var.runtime_request_timeout_seconds}s"
    max_instance_request_concurrency = var.runtime_concurrency

    scaling {
      min_instance_count = var.runtime_min_instances
      max_instance_count = var.runtime_max_instances
    }

    containers {
      name  = "runtime"
      image = local.runtime_bootstrap_image

      ports {
        container_port = var.runtime_port
      }

      resources {
        limits = {
          cpu    = var.runtime_cpu
          memory = var.runtime_memory
        }
        cpu_idle          = true # request-based billing
        startup_cpu_boost = true # more CPU during the cold start, at no extra charge
      }

      dynamic "env" {
        for_each = local.runtime_env
        content {
          name  = env.key
          value = env.value
        }
      }

      dynamic "env" {
        for_each = local.runtime_secrets
        content {
          name = env.key
          value_source {
            secret_key_ref {
              secret  = env.value
              version = "latest" # resolved at each instance start
            }
          }
        }
      }

      # uvicorn listens only after the app's startup completed. A revision that never
      # listens never gets traffic, so a broken deploy leaves the previous one serving.
      startup_probe {
        tcp_socket {
          port = var.runtime_port
        }
        period_seconds    = 2
        timeout_seconds   = 1
        failure_threshold = 30
      }
    }
  }

  lifecycle {
    # The deploy workflow sets the image; Terraform changes keep the running one.
    ignore_changes = [
      template[0].containers[0].image,
      client,
      client_version,
    ]

    precondition {
      condition     = var.runtime_request_timeout_seconds > var.agent_timeout_secs
      error_message = "runtime_request_timeout_seconds must exceed agent_timeout_secs."
    }
    precondition {
      condition     = local.model.timeout < var.agent_timeout_secs - 15
      error_message = "The model timeout (model_timeout_seconds or openai_model_timeout_seconds) must be below the turn timeout (agent_timeout_secs - 15)."
    }
    precondition {
      condition     = var.model_provider != "openai" || var.openai_enabled
      error_message = "model_provider = \"openai\" needs openai_enabled = true (its secret), applied and given a version first."
    }
    precondition {
      condition     = var.runtime_max_instances >= 1 && var.runtime_min_instances <= var.runtime_max_instances
      error_message = "Need 0 <= runtime_min_instances <= runtime_max_instances, max >= 1."
    }
    precondition {
      condition     = var.runtime_history_store == "postgres" || var.runtime_min_instances >= 1
      error_message = "With runtime_history_store = memory, scale-to-zero loses history: set runtime_min_instances = 1 or use postgres."
    }
  }

  depends_on = [
    google_project_service.api,
    google_secret_manager_secret_iam_member.reader,
  ]
}

# Only the VM's identity may invoke the runtime.
resource "google_cloud_run_v2_service_iam_member" "vm_invoker" {
  name     = google_cloud_run_v2_service.runtime.name
  location = var.region
  role     = "roles/run.invoker"
  member   = google_service_account.vm.member
}
