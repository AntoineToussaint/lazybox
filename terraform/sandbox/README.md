# lazybox sandbox — Terraform modules + deployments

A **generic remote dev-box** lazybox can start / stop / connect, split on
two axes (see epic #885, issue #931):

- **`gcp/`** — the provider module (**GCP first**). One IAP-only GCE
  instance + dedicated service account + optional Cloud NAT + a startup
  script. `terraform apply`/`destroy` are the create/tear-down half; the
  fast lifecycle (`start`/`stop`/`status`/`connect`) uses `gcloud`/IAP
  directly, never Terraform. Driven by `GcpProvider` in the
  `lazybox-sandbox` crate. A later `azure/` module lands as its own
  provider.

- **`deployments/`** — **what is on the box**, pluggable. `default.yaml`
  is the generic recipe (base toolchain, blank workspace) that lazybox
  ships and embeds. A project **overrides** it with a thin overlay
  deep-merged on top — `obin.yaml` is obin's (its stack repo, `dev up`,
  extra cores, and cross-project SA grants). Only the changed keys need
  to appear in the overlay.

## Provider module inputs

Only `project` and `instance_name` are required; every other variable has
a default, so a `terraform destroy` driven from a persisted `BoxHandle`
(which knows only project/region/zone/instance_name) resolves without the
full deployment recipe. The deployment overlay's keys map 1:1 to the
remaining variables (`machine_type`, `image_family`, `workload_ports`,
`service_account_roles`, `repo`, `bringup`, …).

## Authentication

The provider carries its **own** credentials (#1047): configure them once and
`ensure`/`wake`/`sleep`/`status`/`connect`/`destroy` — and the TUI's `r`-spawn
— authenticate in the background. There is **no** `gcloud auth login` step, and
lazybox never touches your own `~/.config/gcloud`; credentials are injected
explicitly into every `gcloud`/`terraform` call under a provider-scoped
`CLOUDSDK_CONFIG`.

```yaml
sandbox:
  project: my-proj
  auth:
    # Headless / CI / SaaS: a service-account key (or any
    # GOOGLE_APPLICATION_CREDENTIALS-compatible credential file).
    service_account_key: ~/.lazybox/gcp-sa.json
    # Hosted tier: impersonate a service account (base creds — the key above,
    # else ambient — mint tokens for it).
    impersonate_service_account: deploy@my-proj.iam.gserviceaccount.com
```

Each field is also a per-command flag (`--service-account-key`,
`--impersonate-service-account`, `--gcloud-config-dir`). With no `auth` block
the provider falls back to whatever ambient credentials the machine has (the
legacy path). A preflight verifies the credentials before the first op and
fails with a fix hint rather than a raw `gcloud` error.

## Manual run

```bash
terraform -chdir=gcp init
terraform -chdir=gcp apply \
  -var project=my-proj -var instance_name=lazybox-sbx-demo \
  -var machine_type=e2-standard-8
```

`GcpProvider::ensure` runs the same `apply` with the deployment's `-var`s;
`destroy` runs `terraform destroy` with just the identity vars.
