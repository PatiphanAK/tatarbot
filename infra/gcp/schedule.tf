# Optional daily schedule for the bot VM (var.vm_schedule). The runtime and the ingest worker
# are on Cloud Run and scale to zero on their own; only the always-on VM needs a schedule.
#
# * Stop is a normal guest shutdown: systemd stops tatarbot-discord (docker stop -t 30 ->
#   SIGTERM), and the bot leaves its voice channels and closes the gateway (about 10 s).
# * Start is a normal boot: the startup script rewrites the units, re-adds the metadata
#   block, mints the tokens and starts the bot with the image recorded on the stateful disk.
# * While stopped: no VM or external-IP charges (the IP is ephemeral); the boot disk is
#   still billed. Deploys need the VM running (IAP SSH), so run them during online hours.

locals {
  vm_scheduled = var.vm_schedule != null
}

resource "google_compute_resource_policy" "vm_schedule" {
  count       = local.vm_scheduled ? 1 : 0
  name        = "${var.name}-vm-schedule"
  region      = var.region
  description = "TatarBot VM: start ${var.vm_schedule.start}, stop ${var.vm_schedule.stop} (${var.vm_schedule.time_zone})."

  instance_schedule_policy {
    vm_start_schedule {
      schedule = var.vm_schedule.start
    }
    vm_stop_schedule {
      schedule = var.vm_schedule.stop
    }
    time_zone = var.vm_schedule.time_zone
  }

  depends_on = [google_project_service.api]
}

# The Compute Engine service agent starts and stops scheduled VMs. Only these two
# permissions, not Compute Instance Admin.
resource "google_project_iam_custom_role" "vm_scheduler" {
  count       = local.vm_scheduled ? 1 : 0
  role_id     = "${var.name}VmScheduler"
  title       = "TatarBot VM schedule: start and stop instances"
  permissions = ["compute.instances.start", "compute.instances.stop"]
}

resource "google_project_iam_member" "vm_scheduler" {
  count   = local.vm_scheduled ? 1 : 0
  project = var.project_id
  role    = google_project_iam_custom_role.vm_scheduler[0].id
  member  = "serviceAccount:service-${data.google_project.current.number}@compute-system.iam.gserviceaccount.com"

  # The service agent exists once the Compute Engine API is enabled.
  depends_on = [google_project_service.api]
}
