# A per-worktree lazybox sandbox box: one GCE instance reachable only
# through IAP (no external IP), a dedicated service account, an optional
# Cloud NAT for egress, and a startup script that installs the toolchain
# and optionally clones + brings up a workload.
#
# Ported from Track A's `experiments/antoinetoussaint/obin-gce-box`
# (see docs/obin-remote-dev-scoping.md) into a reusable module: the
# generic default here, obin's specifics supplied as a deployment overlay
# and passed in as variables.

locals {
  # IAP's TCP-forwarding range — the only source allowed to reach SSH.
  iap_source_range = "35.235.240.0/20"
}

# ── Network ────────────────────────────────────────────────────────────
resource "google_compute_network" "sandbox" {
  name                    = "${var.instance_name}-net"
  auto_create_subnetworks = false
}

resource "google_compute_subnetwork" "sandbox" {
  name          = "${var.instance_name}-subnet"
  ip_cidr_range = "10.10.0.0/24"
  region        = var.region
  network       = google_compute_network.sandbox.id
}

# SSH reachable only via IAP TCP forwarding, gated by the instance tag.
resource "google_compute_firewall" "iap_ssh" {
  name          = "${var.instance_name}-iap-ssh"
  network       = google_compute_network.sandbox.id
  direction     = "INGRESS"
  source_ranges = [local.iap_source_range]
  target_tags   = var.network_tags

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
}

# ── Egress for a box with no external IP (optional Cloud NAT) ───────────
resource "google_compute_router" "sandbox" {
  count   = var.enable_nat ? 1 : 0
  name    = "${var.instance_name}-router"
  region  = var.region
  network = google_compute_network.sandbox.id
}

resource "google_compute_router_nat" "sandbox" {
  count                              = var.enable_nat ? 1 : 0
  name                               = "${var.instance_name}-nat"
  router                             = google_compute_router.sandbox[0].name
  region                             = var.region
  nat_ip_allocate_option             = "AUTO_ONLY"
  source_subnetwork_ip_ranges_to_nat = "ALL_SUBNETWORKS_ALL_IP_RANGES"
}

# ── Service account ────────────────────────────────────────────────────
resource "google_service_account" "sandbox" {
  # account_id must match [a-z]([-a-z0-9]*[a-z0-9]), 6-30 chars — so it can
  # neither exceed 30 nor end in a hyphen. Truncating "${instance_name}-sa"
  # could do both for a long name; a stable "lazybox-sbx-" + 12 hex digest
  # is always valid and still identifies the box.
  account_id   = "lazybox-sbx-${substr(md5(var.instance_name), 0, 12)}"
  display_name = "lazybox sandbox ${var.instance_name}"
}

resource "google_project_iam_member" "sandbox_roles" {
  for_each = toset(var.service_account_roles)
  project  = var.project
  role     = each.value
  member   = "serviceAccount:${google_service_account.sandbox.email}"
}

# ── Instance ───────────────────────────────────────────────────────────
resource "google_compute_instance" "sandbox" {
  name         = var.instance_name
  machine_type = var.machine_type
  zone         = var.zone
  tags         = var.network_tags

  boot_disk {
    initialize_params {
      image = "${var.image_project}/${var.image_family}"
      size  = var.disk_size_gb
    }
  }

  # No access_config block → no external IP; the box is IAP-only.
  network_interface {
    subnetwork = google_compute_subnetwork.sandbox.id
  }

  service_account {
    email  = google_service_account.sandbox.email
    scopes = ["cloud-platform"]
  }

  metadata_startup_script = templatefile("${path.module}/startup.sh.tftpl", {
    packages        = var.packages
    repo            = var.repo
    bringup         = var.bringup
    install_lazybox = var.install_lazybox
    lazybox_git_sha = var.lazybox_git_sha
  })

  # A per-worktree box is disposable; let `terraform destroy` (and the idle
  # policy) tear it down without deletion protection getting in the way.
  deletion_protection = false
}
