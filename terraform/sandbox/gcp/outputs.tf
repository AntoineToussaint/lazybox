# The GcpProvider reads these back with `terraform output -json` to build
# the persisted BoxHandle. `instance_name` and `zone` are the two the
# native lifecycle ops (start/stop/describe/connect) need.

output "instance_name" {
  value = google_compute_instance.sandbox.name
}

output "zone" {
  value = google_compute_instance.sandbox.zone
}

output "service_account_email" {
  value = google_service_account.sandbox.email
}

output "network" {
  value = google_compute_network.sandbox.name
}
