# Inputs for the lazybox GCP sandbox box.
#
# Only `project` and `instance_name` are required. Every other variable
# carries a default, so a `terraform destroy` driven from a persisted
# BoxHandle (which only knows project/region/zone/instance_name) resolves
# without the full deployment recipe — the GcpProvider relies on this.

variable "project" {
  type        = string
  description = "GCP project the box lives in."
}

variable "instance_name" {
  type        = string
  description = "GCE instance name — stable per worktree so apply is idempotent."
}

variable "region" {
  type    = string
  default = "us-central1"
}

variable "zone" {
  type    = string
  default = "us-central1-a"
}

variable "machine_type" {
  type    = string
  default = "e2-standard-4"
}

variable "image_family" {
  type    = string
  default = "debian-12"
}

variable "image_project" {
  type    = string
  default = "debian-cloud"
}

variable "disk_size_gb" {
  type    = number
  default = 100
}

variable "enable_nat" {
  type        = bool
  default     = true
  description = "Attach a Cloud NAT so a box with no external IP still has egress."
}

variable "network_tags" {
  type    = list(string)
  default = ["lazybox-sandbox"]
}

variable "service_account_roles" {
  type        = list(string)
  default     = []
  description = "Project IAM roles bound to the box's service account."
}

variable "workload_ports" {
  type        = list(number)
  default     = []
  description = "Informational: ports the client forwards over IAP (no public ingress is opened)."
}

variable "packages" {
  type        = list(string)
  default     = ["git", "curl", "build-essential"]
  description = "OS packages installed on first boot before bring-up."
}

variable "repo" {
  type        = string
  default     = ""
  description = "Optional repo (owner/repo or URL) cloned on first boot."
}

variable "bringup" {
  type        = string
  default     = ""
  description = "Optional command run after clone to bring the workload up."
}

variable "install_lazybox" {
  type        = bool
  default     = true
  description = <<-EOT
    Install the lazybox toolchain + daemon on first boot (#977): build the
    daemon from `lazybox_git_sha`, install the systemd units, and arm the
    idle-stop timer, so the box runs a wire-compatible daemon unattended.
    A bring-your-own-stack deployment sets this false to manage its own.
  EOT
}

variable "lazybox_git_sha" {
  type        = string
  default     = ""
  description = <<-EOT
    Commit the box's lazybox daemon is built from — the client passes its
    own baked build SHA so the wire fingerprint matches by construction.
    Empty tracks the default branch tip (a client with no baked SHA, e.g.
    a release tarball). Only used when install_lazybox is true.
  EOT
}
