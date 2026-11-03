# One e2-micro VM on Container-Optimized OS runs both containers, managed by systemd units
# that the startup script writes on every boot (templates/startup.sh.tftpl).
#
# * Bot -> runtime: both on the Docker network "tatarbot"; the bot calls
#   http://tatarbot-runtime:8080. No port is published on the host.
# * Images: the unit runs whatever /var/lib/tatarbot/images/<svc> names. Only deploy.sh
#   (run by the deploy workflow) changes it, so Terraform never redeploys an image.
# * Changing configuration here updates the startup-script metadata in place (no VM
#   replacement); it takes effect at the next boot or `sudo google_metadata_script_runner startup`.

locals {
  runtime_url = "http://${var.name}-runtime:${var.runtime_port}"

  service_env = {
    runtime = merge(var.runtime_extra_environment, {
      UVICORN_HOST          = "0.0.0.0"
      UVICORN_PORT          = tostring(var.runtime_port)
      MODEL_BASE_URL        = var.model_base_url
      MODEL_NAME            = var.model_name
      MODEL_TIMEOUT_SECONDS = tostring(var.model_timeout_seconds)
      # Whole turn incl. web searches; leaves the bot time to report a timeout.
      TURN_TIMEOUT_SECONDS = tostring(var.agent_timeout_secs - 10)
    })
    discord = merge(
      {
        AGENT_BASE_URL     = local.runtime_url
        AGENT_TIMEOUT_SECS = tostring(var.agent_timeout_secs)
        RUST_LOG           = "info,serenity=warn"
      },
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
  }

  services_config = {
    runtime = {
      enabled   = var.runtime_enabled
      memory_mb = var.runtime_memory_mb
      # In-flight requests may take up to MODEL_TIMEOUT_SECONDS; uvicorn drains on SIGTERM.
      stop_timeout = 120
      # The image's HEALTHCHECK (/health) gives the container its health status.
      docker_args = ""
    }
    discord = {
      enabled   = var.discord_enabled
      memory_mb = var.discord_memory_mb
      # The bot closes its gateway connection on SIGTERM within a few seconds.
      stop_timeout = 30
      # Writable scratch space for uploads (download + transcode) on disk, not in RAM.
      docker_args = "--mount type=volume,src=${var.name}-discord-tmp,dst=/tmp"
    }
  }

  startup_services = {
    for svc, c in local.services_config : svc => merge(c, {
      env = join("\n", [for k, v in local.service_env[svc] : "${k}=${v}"])
      secrets = join("\n", [
        for k, s in local.enabled_secrets : "${s.env} ${s.id}" if contains(s.services, svc)
      ])
    })
  }

  startup_script = templatefile("${path.module}/templates/startup.sh.tftpl", {
    registry_host   = local.registry_host
    registry_prefix = replace(local.registry_prefix, ".", "\\.")
    docker_subnet   = var.docker_subnet
    services        = local.startup_services
  })
}

resource "google_compute_instance" "main" {
  name         = var.name
  machine_type = var.machine_type
  zone         = var.zone
  description  = "TatarBot: Discord bot + Agent Runtime containers on COS."

  allow_stopping_for_update = true

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
    # Ephemeral external IPv4: outbound to Discord (gateway + UDP voice), Supabase and the
    # model API. The firewall admits nothing from the internet.
    access_config {
      network_tier = "PREMIUM"
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
      condition     = var.model_timeout_seconds < var.agent_timeout_secs
      error_message = "model_timeout_seconds must be below agent_timeout_secs."
    }
    precondition {
      condition     = alltrue([for svc in keys(local.service_env) : alltrue([for v in values(local.service_env[svc]) : !strcontains(v, "\n")])])
      error_message = "Environment values must be single lines (they go into docker --env-file)."
    }
  }

  depends_on = [
    google_project_service.api,
    google_secret_manager_secret_iam_member.vm,
    google_artifact_registry_repository_iam_member.vm_pull,
  ]
}
