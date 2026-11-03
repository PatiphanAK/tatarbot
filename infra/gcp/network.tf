# A dedicated VPC with one subnet. Nothing accepts traffic from the internet: the only
# ingress rule admits Google's IAP TCP-forwarding range on port 22 (shell and deploys).
# Egress uses the VM's ephemeral external IPv4 (Discord voice needs IPv4); no Cloud NAT.

resource "google_compute_network" "main" {
  name                    = var.name
  auto_create_subnetworks = false
  routing_mode            = "REGIONAL"

  depends_on = [google_project_service.api]
}

resource "google_compute_subnetwork" "main" {
  name                     = "${var.name}-${var.region}"
  network                  = google_compute_network.main.id
  region                   = var.region
  ip_cidr_range            = var.subnet_cidr
  private_ip_google_access = true
}

resource "google_compute_firewall" "iap_ssh" {
  name        = "${var.name}-allow-iap-ssh"
  network     = google_compute_network.main.id
  description = "SSH only from Identity-Aware Proxy TCP forwarding (gcloud compute ssh --tunnel-through-iap)."
  direction   = "INGRESS"
  priority    = 1000

  source_ranges           = ["35.235.240.0/20"]
  target_service_accounts = [google_service_account.vm.email]

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
}

# Explicit, logged-off copy of the implied deny, so the intent shows up in the console.
resource "google_compute_firewall" "deny_ingress" {
  name        = "${var.name}-deny-ingress"
  network     = google_compute_network.main.id
  description = "Everything else inbound is dropped."
  direction   = "INGRESS"
  priority    = 65000

  source_ranges = ["0.0.0.0/0"]

  deny {
    protocol = "all"
  }
}
