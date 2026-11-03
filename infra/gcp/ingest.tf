# Ingest worker (ADR 0004): the music library's upload processing on Cloud Run, fed by
# Cloud Tasks. Only with ingest_enabled.
#
# Identities (least privilege):
# * tatarbot-ingest (the worker): writes objects in the music bucket, reads DATABASE_URL,
#   enqueues into its own queue, and may attach the tasks-invoker's OIDC token to tasks.
# * tatarbot-tasks (tasks invoker): only signs requests from Cloud Tasks, Cloud Scheduler
#   and the budget push subscription to the worker. Nothing else.
# * The VM may invoke the worker (the bot submits jobs).
#
# Caller identity is checked twice: Cloud Run IAM (run.invoker, service-level) and the
# worker itself, which verifies every Google ID token and maps its email to a route
# (internal callers) or a registered producer (ingest.producers).

locals {
  ingest_service_name = "${var.name}-ingest"
  # Deterministic run.app URL, known before the service exists: the token audience, the
  # Cloud Tasks target, and what the VM calls. Internal ingress requires the run.app URL.
  ingest_url = "https://${local.ingest_service_name}-${data.google_project.current.number}.${var.region}.run.app"
  ingest_n   = var.ingest_enabled ? 1 : 0
  # Both readers of a budget-driven brake must exist.
  budget_brake = var.ingest_enabled && var.budget_brake_enabled && var.billing_account != ""
}

resource "google_service_account" "ingest" {
  count        = local.ingest_n
  account_id   = "${var.name}-ingest"
  display_name = "TatarBot ingest worker (Cloud Run)"

  depends_on = [google_project_service.api]
}

resource "google_service_account" "tasks" {
  count        = local.ingest_n
  account_id   = "${var.name}-tasks"
  display_name = "TatarBot tasks invoker (Cloud Tasks, Scheduler, budget push -> worker)"

  depends_on = [google_project_service.api]
}

resource "google_cloud_tasks_queue" "ingest" {
  count    = local.ingest_n
  name     = "${var.name}-ingest"
  location = var.region

  # One upload at a time: replaces the bot's in-process semaphore and caps cost.
  rate_limits {
    max_dispatches_per_second = 1
    max_concurrent_dispatches = 1
  }

  # Retries of *deliveries*. Exactly-once effects come from the database (leases, fences).
  retry_config {
    max_attempts       = 4
    max_retry_duration = "3600s"
    min_backoff        = "15s"
    max_backoff        = "300s"
    max_doublings      = 3
  }

  stackdriver_logging_config {
    sampling_ratio = 1.0
  }

  depends_on = [google_project_service.api]
}

resource "google_cloud_run_v2_service" "ingest" {
  count                = local.ingest_n
  name                 = local.ingest_service_name
  location             = var.region
  description          = "TatarBot ingest worker: private; Cloud Tasks, Scheduler and the bot VM only."
  ingress              = "INGRESS_TRAFFIC_INTERNAL_ONLY"
  invoker_iam_disabled = false
  deletion_protection  = false # stateless; state is in Postgres and GCS

  template {
    service_account                  = google_service_account.ingest[0].email
    timeout                          = "${var.ingest_request_timeout_seconds}s"
    max_instance_request_concurrency = 1 # one untrusted file per instance
    # Full Linux semantics for the ffmpeg rlimits (gen1's gVisor differs).
    execution_environment = "EXECUTION_ENVIRONMENT_GEN2"

    scaling {
      min_instance_count = 0
      max_instance_count = var.ingest_max_instances
    }

    containers {
      name  = "worker"
      image = local.runtime_bootstrap_image

      ports {
        container_port = 8080
      }

      resources {
        limits = {
          cpu    = var.ingest_cpu
          memory = var.ingest_memory
        }
        cpu_idle          = true # request-based billing
        startup_cpu_boost = true
      }

      dynamic "env" {
        for_each = {
          RUST_LOG                = "info"
          DATABASE_POOL_SIZE      = "3"
          INGEST_PUBLIC_URL       = local.ingest_url
          INGEST_AUDIENCES        = local.ingest_url
          INGEST_INTERNAL_CALLERS = google_service_account.tasks[0].email
          INGEST_QUEUE            = google_cloud_tasks_queue.ingest[0].id
          INGEST_TASKS_INVOKER_SA = google_service_account.tasks[0].email
          MUSIC_BUCKET            = local.music_bucket
          MUSIC_PREFIX            = var.music_library_prefix
          MUSIC_GUILD_QUOTA_MB    = tostring(var.music_guild_quota_mb)
        }
        content {
          name  = env.key
          value = env.value
        }
      }

      env {
        name = "DATABASE_URL"
        value_source {
          secret_key_ref {
            secret  = google_secret_manager_secret.this["database_url"].secret_id
            version = "latest"
          }
        }
      }

      startup_probe {
        tcp_socket {
          port = 8080
        }
        period_seconds    = 2
        timeout_seconds   = 1
        failure_threshold = 30
      }
    }
  }

  lifecycle {
    ignore_changes = [
      template[0].containers[0].image,
      client,
      client_version,
    ]

    precondition {
      condition     = var.music_library_enabled
      error_message = "ingest_enabled needs music_library_enabled (bucket and DATABASE_URL)."
    }
    precondition {
      condition     = var.ingest_max_instances >= 1 && var.ingest_max_instances <= 3
      error_message = "ingest_max_instances must be 1..3 (it is the cost ceiling)."
    }
    precondition {
      condition     = var.ingest_request_timeout_seconds >= 300 && var.ingest_request_timeout_seconds < 720
      error_message = "ingest_request_timeout_seconds must be 300..719: the 12-minute lease must outlive one attempt."
    }
  }

  depends_on = [
    google_project_service.api,
    google_secret_manager_secret_iam_member.reader,
  ]
}

# ---- Who may invoke the worker (service-level; the app checks routes and producers).

resource "google_cloud_run_v2_service_iam_member" "ingest_invoker" {
  for_each = var.ingest_enabled ? {
    vm    = google_service_account.vm.member
    tasks = google_service_account.tasks[0].member
  } : {}

  name     = google_cloud_run_v2_service.ingest[0].name
  location = var.region
  role     = "roles/run.invoker"
  member   = each.value
}

# ---- The worker's own permissions.

resource "google_cloud_tasks_queue_iam_member" "ingest_enqueuer" {
  count    = local.ingest_n
  name     = google_cloud_tasks_queue.ingest[0].name
  location = var.region
  role     = "roles/cloudtasks.enqueuer"
  member   = google_service_account.ingest[0].member
}

# Creating a task that carries the tasks-invoker's OIDC token requires actAs on it.
resource "google_service_account_iam_member" "ingest_act_as_tasks" {
  count              = local.ingest_n
  service_account_id = google_service_account.tasks[0].name
  role               = "roles/iam.serviceAccountUser"
  member             = google_service_account.ingest[0].member
}

resource "google_storage_bucket_iam_member" "ingest_objects" {
  count  = var.ingest_enabled && var.music_library_enabled ? 1 : 0
  bucket = google_storage_bucket.music[0].name
  role   = "roles/storage.objectUser"
  member = google_service_account.ingest[0].member
}

# ---- Scheduled maintenance (OIDC as the tasks invoker).

resource "google_cloud_scheduler_job" "ingest" {
  for_each = var.ingest_enabled ? {
    reap      = { schedule = "*/5 * * * *", path = "/internal/maintenance/reap" }
    consumers = { schedule = "17 * * * *", path = "/internal/maintenance/consumers" }
  } : {}

  name             = "${var.name}-ingest-${each.key}"
  region           = var.region
  schedule         = each.value.schedule
  time_zone        = "Etc/UTC"
  attempt_deadline = "300s"

  retry_config {
    retry_count = 1
  }

  http_target {
    uri         = "${local.ingest_url}${each.value.path}"
    http_method = "POST"
    body        = base64encode("{}")
    headers = {
      "Content-Type" = "application/json"
    }
    oidc_token {
      service_account_email = google_service_account.tasks[0].email
      audience              = local.ingest_url
    }
  }

  depends_on = [google_project_service.api]
}

# ---- Budget brake: budget notifications -> Pub/Sub -> push to the worker, which pauses
# all ingestion (ingest.switches 'global') once spend reaches the budget. Never resumes by
# itself and never disables billing.

resource "google_pubsub_topic" "budget" {
  count = local.budget_brake ? 1 : 0
  name  = "${var.name}-budget"

  depends_on = [google_project_service.api]
}

# Cloud Billing publishes budget notifications as this Google-managed account.
resource "google_pubsub_topic_iam_member" "budget_publisher" {
  count  = local.budget_brake ? 1 : 0
  topic  = google_pubsub_topic.budget[0].name
  role   = "roles/pubsub.publisher"
  member = "serviceAccount:billing-budget-alert@system.gserviceaccount.com"
}

resource "google_pubsub_subscription" "budget_push" {
  count = local.budget_brake ? 1 : 0
  name  = "${var.name}-budget-to-ingest"
  topic = google_pubsub_topic.budget[0].id

  ack_deadline_seconds       = 30
  message_retention_duration = "86400s"

  push_config {
    push_endpoint = "${local.ingest_url}/internal/maintenance/budget"
    oidc_token {
      service_account_email = google_service_account.tasks[0].email
      audience              = local.ingest_url
    }
  }

  retry_policy {
    minimum_backoff = "30s"
    maximum_backoff = "600s"
  }
}
