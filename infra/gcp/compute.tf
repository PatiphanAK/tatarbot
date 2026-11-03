# One e2-micro VM on Container-Optimized OS runs the Discord bot (gateway, voice, music),
# 24/7, managed by a systemd unit that the startup script writes on every boot
# (templates/startup.sh.tftpl). The Agent Runtime runs on Cloud Run (cloudrun.tf).
#
# * Bot -> runtime: HTTPS to the private Cloud Run URL. The host mints a Google ID token
#   for that URL from the metadata server every id_token_refresh_minutes and puts it in a
#   tmpfs directory the bot mounts read-only; the bot sends it as
#   X-Serverless-Authorization, next to its AGENT_API_KEY bearer.
# * Images: the unit runs whatever /var/lib/tatarbot/images/discord names. Only deploy.sh
#   (run by the deploy workflow) changes it, so Terraform never redeploys an image.
# * Changing configuration here updates the startup-script metadata in place (no VM
#   replacement); it takes effect at the next boot or `sudo google_metadata_script_runner startup`.

locals {
  runtime_url = google_cloud_run_v2_service.runtime.uri

  # Inside the bot container (mounted read-only from /run/tatarbot/agent-token).
  id_token_mount = "/run/agent-token"

  discord_env = merge(
    {
      AGENT_BASE_URL      = local.runtime_url
      AGENT_TIMEOUT_SECS  = tostring(var.agent_timeout_secs)
      AGENT_ID_TOKEN_FILE = "${local.id_token_mount}/id-token"
      RUST_LOG            = "info,serenity=warn"
    },
    # ADR 0004 phase 3: uploads processed by the ingest worker.
    var.library_processing == "worker" ? {
      LIBRARY_PROCESSING   = "worker"
      INGEST_URL           = local.ingest_url
      INGEST_ID_TOKEN_FILE = "${local.id_token_mount}/ingest-id-token"
    } : {},
    var.library_storage == "gcs" ? {
      LIBRARY_STORAGE = "gcs"
      GCS_TOKEN_FILE  = "${local.id_token_mount}/gcs-token"
    } : {},
    var.music_library_enabled ? {
      MUSIC_S3_BUCKET      = local.music_bucket
      MUSIC_S3_PREFIX      = var.music_library_prefix
      MUSIC_S3_ENDPOINT    = "https://storage.googleapis.com"
      MUSIC_GUILD_QUOTA_MB = tostring(var.music_guild_quota_mb)
      MUSIC_UPLOAD_ROLE    = var.music_upload_role
      # SigV4 against the GCS XML API: region "auto", and no flexible-checksum headers,
      # which the AWS SDK adds by default and S3-compatible stores may reject.
      AWS_REGION                       = "auto"
      AWS_REQUEST_CHECKSUM_CALCULATION = "when_required"
      AWS_RESPONSE_CHECKSUM_VALIDATION = "when_required"
      AWS_EC2_METADATA_DISABLED        = "true"
    } : {},
  )

  # Read-only on the library prefix of the music bucket (Credential Access Boundary).
  gcs_boundary = jsonencode({
    accessBoundary = {
      accessBoundaryRules = [{
        availablePermissions = ["inRole:roles/storage.objectViewer"]
        availableResource    = "//storage.googleapis.com/projects/_/buckets/${local.music_bucket}"
        availabilityCondition = {
          # Double quotes: the JSON is passed in a single-quoted shell argument.
          expression = "resource.name.startsWith(\"projects/_/buckets/${local.music_bucket}/objects/${var.music_library_prefix}\")"
        }
      }]
    }
  })

  startup_services = {
    discord = {
      enabled   = var.discord_enabled
      memory_mb = var.discord_memory_mb
      # The bot closes its gateway connection on SIGTERM within a few seconds.
      stop_timeout = 30
      # Scratch space for uploads (download + transcode) on disk, not in RAM; the ID token,
      # read-only.
      docker_args = join(" ", [
        "--mount type=volume,src=${var.name}-discord-tmp,dst=/tmp",
        "--mount type=bind,src=/run/tatarbot/agent-token,dst=${local.id_token_mount},readonly",
      ])
      env = join("\n", [for k, v in local.discord_env : "${k}=${v}"])
      secrets = join("\n", [
        for k, s in local.enabled_secrets : "${s.env} ${s.id}" if contains(s.consumers, "discord")
      ])
    }
  }

  startup_script = templatefile("${path.module}/templates/startup.sh.tftpl", {
    registry_host            = local.registry_host
    registry_prefix          = replace(local.registry_prefix, ".", "\\.")
    docker_subnet            = var.docker_subnet
    services                 = local.startup_services
    id_token_audience        = local.runtime_url
    ingest_audience          = var.ingest_enabled ? local.ingest_url : ""
    gcs_boundary             = var.library_storage == "gcs" ? local.gcs_boundary : ""
    id_token_refresh_minutes = var.id_token_refresh_minutes
    bot_uid                  = var.bot_container_uid
  })
}

resource "google_compute_instance" "main" {
  name         = var.name
  machine_type = var.machine_type
  zone         = var.zone
  description  = "TatarBot: Discord bot container on COS (the runtime is on Cloud Run)."

  allow_stopping_for_update = true

  # Daily start/stop (schedule.tf); empty = 24/7.
  resource_policies = local.vm_scheduled ? [google_compute_resource_policy.vm_schedule[0].self_link] : []

  boot_disk {
    auto_delete = true
    initialize_params {
      image = var.cos_image
      size  = var.boot_disk_gb
      type  = var.boot_disk_type
    }
  }

  network_interface {
    subnetwork = google_compute_subnetwork.main.id
    # Ephemeral external IPv4: outbound to Discord (gateway + UDP voice), Supabase, GCS and
    # the runtime. The firewall admits nothing from the internet.
    access_config {
      network_tier = var.network_tier
    }
  }

  service_account {
    email  = google_service_account.vm.email
    scopes = ["cloud-platform"] # access is limited by IAM, not scopes
  }

  shielded_instance_config {
    enable_secure_boot          = true
    enable_vtpm                 = true
    enable_integrity_monitoring = true
  }

  metadata = {
    startup-script            = local.startup_script
    enable-oslogin            = "TRUE"
    block-project-ssh-keys    = "TRUE"
    google-logging-enabled    = "true" # COS logging agent: container stdout -> Cloud Logging
    google-monitoring-enabled = "true"
  }

  scheduling {
    automatic_restart   = true
    on_host_maintenance = "MIGRATE"
    preemptible         = false
  }

  lifecycle {
    precondition {
      condition     = alltrue([for v in values(local.discord_env) : !strcontains(v, "\n")])
      error_message = "Environment values must be single lines (they go into docker --env-file)."
    }
    # ADR 0004 phases, in order.
    precondition {
      condition     = var.library_processing == "local" || var.ingest_enabled
      error_message = "library_processing = worker needs ingest_enabled (phase 1 first)."
    }
    precondition {
      condition     = var.library_storage == "s3" || var.library_processing == "worker"
      error_message = "library_storage = gcs is read-only: it needs library_processing = worker."
    }
    precondition {
      condition     = var.music_hmac_enabled || var.library_storage == "gcs" || !var.music_library_enabled
      error_message = "music_hmac_enabled = false removes the bot's S3 credentials: set library_storage = gcs first."
    }
  }

  depends_on = [
    google_project_service.api,
    google_secret_manager_secret_iam_member.reader,
    google_artifact_registry_repository_iam_member.vm_pull,
    # The schedule can't act until its service agent may start and stop the VM.
    google_project_iam_member.vm_scheduler,
  ]
}
