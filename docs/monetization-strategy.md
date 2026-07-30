# Monetization strategy spike

_Research checked 2026-07-30. Prices are public list prices in USD, before tax,
support, discounts, and regional variation. This is a recommendation and a
model for testing demand, not a product or pricing commitment._

## Recommendation

Use the existing open-source daemon as the durable acquisition layer, then
monetize in this order:

1. **Lane 0 now:** enable GitHub Sponsors and put one restrained support link in
   the README and on lazybox.ai. Treat donations as a signal, not a business
   model.
2. **Lane 1 next:** sell a polished desktop client while keeping the daemon and
   TUI open. This is the shortest path to learning whether users will pay for
   lazybox, and [the reuse work is already scoped in #641][issue-641].
3. **Lane 2 only after paid demand:** offer hosted, cold-started execution with
   bring-your-own model keys first. Charge separately for compute and managed
   model usage. Do not offer unlimited agent runs.
4. **Lane 3 opportunistically:** ship a self-hosted team tier before a managed
   enterprise cloud if inbound teams ask for it. It reuses the daemon without
   first taking custody of every team's source and credentials.

The important sequencing rule is that an iPhone client is not the expensive
part of Lane 2. Securely running an arbitrary agent shell with source code and
credentials is. A responsive web client can validate the remote workflow now;
public hosted execution waits for the isolation gates below.

## Lane comparison

| Lane | Offer | Indicative price | Incremental cost | Main risk | Recommendation |
|---|---|---:|---:|---|---|
| 0 — support | GitHub Sponsors | User-chosen | Near zero; GitHub charges no fee on personal-account sponsorships and up to 6% on organization-account sponsorships | Low conversion | Ship for launch |
| 1 — desktop | Signed Tauri client, OSS daemon/TUI | $12/month or $99/year | Payment fees, signing, support; no hosted agent compute | Building too much parity before charging | Build the smallest paid workflow from #641 |
| 2 — cloud | Hosted daemon, worktrees, web/iPhone access | Start near $39/month BYO-key, with a compute allowance; meter overage and model usage | Roughly $10–$22 for a light/regular managed-sandbox user before support; much more for heavy or always-on users | Isolation, credentials, abuse, variable token spend | Pilot only after Lane 1 |
| 3 — team | Self-hosted team controls or managed team cloud | Test $30/seat/month with a minimum, or annual contract | Support, SSO/audit work, and Lane 2 costs if managed | Enterprise support burden | Follow demand; self-hosted first |

Lane 0 uses the fee schedule in [GitHub's Sponsors documentation][github-sponsor-fees].
The `FUNDING.yml` syntax is documented by [GitHub's repository
documentation][github-funding]. The `AntoineToussaint` Sponsors URL currently
redirects to the normal profile, so completing GitHub Sponsors enrollment is a
release prerequisite for the draft links in this change to accept money.

## Rough economics

### Lane 0: donations

There is no defensible pre-launch conversion rate. A transparent launch
scenario is more useful than a forecast:

| Launch visitors | Assumed donor conversion | Average one-off gift | Gross |
|---:|---:|---:|---:|
| 10,000 | 0.1% | $5 | $50 |
| 10,000 | 1.0% | $5 | $500 |

That range is deliberately wide. The value of Lane 0 is capturing goodwill,
learning which users care enough to pay, and making project funding visible.
Do not add donation gates, nagging UI, or donor-only core features.

### Lane 1: desktop

An initial price test of **$12/month or $99/year** keeps the local product
meaningfully cheaper than hosted compute. At 100 monthly subscribers, $1,200
gross becomes about **$1,135 after a US-card benchmark of 2.9% + $0.30 per
charge**, before tax, refunds, and support. Actual processing varies by country;
the benchmark comes from [Stripe's published pricing][stripe-pricing].

Direct distribution avoids App Store commission, but macOS signing and
notarization still require the [Apple Developer Program's $99 annual
membership][apple-program]. Prefer a subscription over a lifetime license:
lazybox tracks changing providers and agent CLIs, so maintenance is recurring.

The paid surface should be the convenience layer—native packaging, richer
visual workflows, updates, and later cloud connectivity—not artificial limits
in the MIT-licensed daemon. The MIT license means exclusivity comes from the
product, brand, hosted service, and execution quality rather than from stopping
forks.

### Lane 2: hosted compute

Two current list-price anchors bound an MVP:

- A raw Fly Machine with 2 shared CPUs and 4 GB RAM is roughly **$22–$27/month
  if left running**, depending on region, and a 20 GB volume is **$3/month**.
  Fly bills started Machines by the second and stopped root filesystems/volumes
  at $0.15/GB-month. See [Fly resource pricing][fly-pricing].
- A managed Modal Sandbox designed for untrusted code charges
  $0.00003942/physical-core-second and $0.00000667/GiB-second. One physical core
  (2 vCPU) plus 4 GiB is therefore about **$0.238/hour**; 20 GiB of volume is
  **$1.80/month**. See [Modal pricing][modal-pricing] and its
  [sandbox security model][modal-security].

Fly is a useful raw-microVM cost floor, not a complete control plane. Modal is
the more conservative pilot budget because it buys an untrusted-code boundary
and egress controls while usage is small. The product must still validate the
vendor and configure the sandbox correctly.

Using the managed-sandbox rate plus $1.80 storage and an assumed $4 per user for
the shared control plane, logs, network, and backups:

| Active sandbox time/month | Compute | Storage + shared services | Infra COGS/user |
|---:|---:|---:|---:|
| 20 hours | $4.76 | $5.80 | **$10.56** |
| 60 hours | $14.28 | $5.80 | **$20.08** |
| 160 hours | $38.07 | $5.80 | **$43.87** |
| Always on (730 hours) | $173.71 | $5.80 | **$179.51** |

This excludes model tokens, customer support, payment fees, tax, abuse, and
engineering. It makes three decisions unavoidable:

- Suspend compute when there is no shell or agent work. Keep the control plane,
  workspace metadata, and event history available so an iPhone can check status
  without waking the execution plane.
- Start with **$39/month BYO-key including about 60 sandbox hours**, then meter
  excess near $0.40/hour during the pilot. That produces only about 48% gross
  margin at the full allowance, so it is a price-discovery point, not a final
  price.
- Benchmark real workloads before reserving infrastructure. A later
  Firecracker fleet can approach the raw-VM floor, but only after scale makes
  owning the isolation and on-call burden worthwhile.

For comparison, AWS Fargate's US East example prices Linux/x86 at
$0.000011244/vCPU-second and $0.000001235/GiB-second. A 2 vCPU/4 GiB task is
about **$0.099/hour** before persistence and adjacent services. Fargate is a
compute sanity check, not the proposed arbitrary-code isolation boundary.
[AWS documents per-second billing and includes 20 GB ephemeral
storage][fargate-pricing].

### Model-token costs

BYO-key should be the default because token spend can exceed infrastructure.
If lazybox later resells model usage, it needs hard user budgets, per-run cost
attribution, and a separate usage line item.

The table below uses two representative current coding models:
[GPT-5.6 Terra at $2.50 input, $0.25 cached input, and $15 output per million
tokens][openai-pricing], and [Claude Sonnet 4.6 at $3 input, $0.30 cache hits,
and $15 output per million tokens][anthropic-pricing].

| Illustrative monthly traffic | GPT-5.6 Terra | Claude Sonnet 4.6 |
|---|---:|---:|
| 2M uncached input + 8M cached input + 0.5M output | $14.50 | $15.90 |
| 5M uncached input + 25M cached input + 2M output | $48.75 | $52.50 |
| 15M uncached input + 75M cached input + 6M output | $146.25 | $157.50 |

These rows price traffic; they do not predict how many tokens a user will
consume. Agent loops, repository size, cache-hit rate, reasoning settings, and
tool transcripts move the result substantially. A managed-key offer should
pass model cost through with roughly 20–30% overhead for payment risk and
operations, never hide it in an unlimited flat plan.

### Lane 3: teams

Test **$30/seat/month with a 10-seat minimum** for self-hosted SSO, policy,
audit, and shared-inbox features, or quote an annual contract when support and
procurement dominate. A managed team tier adds Lane 2 COGS per active user and
should be priced separately. One ten-seat team at the test price is $300 MRR;
one hour of bespoke support can erase much of that, so the minimum matters.

## Cloud security and isolation plan

### Trust boundaries

Split the system into two planes:

- The **control plane** owns identity, billing, workspace metadata, encrypted
  credential references, event history, and scheduling. It never executes
  repository code.
- The **execution plane** runs the daemon, Git, build tools, and agent CLI in a
  single-tenant sandbox. Repositories from different customers never share a
  guest kernel, filesystem, process namespace, or encryption key.

For a limited pilot, buy a sandbox service specifically designed for untrusted
code and require outbound allowlists. Before public GA, use a single-tenant
microVM boundary or a vendor with an equivalent documented boundary.
Firecracker's production guidance requires its jailer, seccomp, cgroups,
namespace isolation, dropped privileges, resource limits, patched hosts, and
one tenant per Firecracker process; follow that guidance rather than treating
“a container” as sufficient. See [Firecracker's production host
recommendations][firecracker-security].

### Execution controls

Each sandbox must have:

- an immutable, signed, vulnerability-scanned base image and an ephemeral
  writable root;
- no host mounts, Docker socket, cloud instance metadata, KVM device, or
  control-plane credentials;
- hard CPU, memory, process, file-descriptor, disk, output, wall-time, and
  concurrent-run limits;
- a per-user encrypted worktree volume that can attach to only one active guest;
- no inbound network path—the control plane brokers terminal/API traffic;
- default-deny egress, with TLS domain policies for GitHub, the lazybox model
  proxy, and explicitly approved package registries; block private, link-local,
  and metadata ranges even when DNS changes;
- bounded, redacted logs. Do not retain environment dumps or shell transcripts
  by default.

Destroy the ephemeral root after each run. On account deletion, destroy the
per-user data-encryption key and expire backups on a documented schedule.

### Credentials

- Use a GitHub App rather than collecting broad personal access tokens. Mint an
  installation token just in time for only the selected repositories and
  permissions. GitHub installation tokens expire after one hour and can be
  narrowed when minted; see [GitHub's installation-token
  documentation][github-app-token].
- Give the guest that short-lived GitHub token only for the run that needs it.
  Assume arbitrary code can read and exfiltrate it; short life and narrow scope
  are the blast-radius control.
- Store BYO model keys in an envelope-encrypted secret store. Never inject the
  provider key into the shell. Point agent CLIs at lazybox's existing
  `llm_gateway_url` support and give the guest a short-lived, user/run-scoped
  proxy credential. The proxy enforces model allowlists, budgets, and audit.
- Keep control-plane and execution-plane keys in separate KMS scopes. Rotation
  or a kill switch must be able to revoke every active run without redeploying.

### Release gates

Hosted execution does not graduate from a private pilot until all of these are
true:

1. Threat model and data-flow review cover tenant escape, malicious
   repositories, dependency scripts, SSRF, secret theft, and resource abuse.
2. Automated adversarial tests demonstrate cross-tenant filesystem/process
   denial, metadata blocking, egress policy, quota enforcement, and reliable
   teardown.
3. An independent penetration test covers the control plane and execution
   boundary; critical/high findings are closed.
4. Incident response can freeze launches, revoke GitHub/model credentials,
   isolate one tenant, and produce an audit trail.
5. Restore and deletion drills prove encrypted volume recovery and key-based
   erasure.
6. A 20-user pilot measures cold start, active hours, token spend, support
   load, and gross margin. Target a usable workspace in under 15 seconds from
   cold and do not set final pricing until p50/p95 usage is known.

## Idle and mobile behavior

The mobile “check my agents” path should query the always-available control
plane. Only a command that needs Git, a shell, or an agent wakes the execution
plane:

1. Persist normalized workspace and agent events outside the sandbox.
2. After an idle grace period, flush the daemon state, detach the encrypted
   worktree volume, and stop the guest.
3. On a new run, start a clean guest, attach the volume, mint fresh credentials,
   recover sessions, and reconnect the event stream.
4. Expire truly inactive worktrees to cheaper storage after warning the user.

Do not keep an always-on VM per subscriber. Even the raw Fly reference reaches
roughly $25–$30/user/month with storage before the control plane; managed
sandbox compute approaches $180/user/month when left on continuously.

## Local browser PoC

The JSON gateway now serves a responsive read-only client at `/`. The static
shell is public so a browser can load it; the token is entered locally and
every `/v1/*` request still sends `Authorization: Bearer …`. The client reads
the health endpoint and workspace snapshot and consumes the authenticated
NDJSON event stream. It deliberately stores the token only in page memory.

The gateway remains loopback-only. The integration tests verify that the page
loads locally, API routes still require the bearer token, and every
non-loopback listener is rejected.

This is intentionally not the cloud authentication design. Production needs
TLS, user identity, short-lived access tokens, tenant authorization, origin
policy, rate limits, and the execution controls above.

## Decision checkpoints

- **After launch:** did Sponsors produce repeated donors or useful
  conversations? Keep the links regardless; do not infer product pricing from
  one launch spike.
- **Before Lane 1 build-out:** recruit at least 10 design partners willing to
  pay for the desktop MVP described in #641.
- **Before Lane 2 pilot:** demonstrate desktop retention and recruit users who
  specifically need agents while their laptop is off.
- **Before Lane 2 GA:** pass every security gate and show at least 50% gross
  margin on base subscription infrastructure at the measured allowance.
- **Lane 3 fork:** prioritize self-hosted team controls when customer security
  requirements arrive before the hosted isolation platform is ready.

[anthropic-pricing]: https://platform.claude.com/docs/en/about-claude/pricing
[apple-program]: https://developer.apple.com/programs/whats-included/
[fargate-pricing]: https://aws.amazon.com/fargate/pricing/
[firecracker-security]: https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md
[fly-pricing]: https://fly.io/docs/about/pricing/
[github-app-token]: https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app
[github-funding]: https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/displaying-a-sponsor-button-in-your-repository
[github-sponsor-fees]: https://docs.github.com/en/sponsors/sponsoring-open-source-contributors/about-sponsorships-fees-and-taxes
[issue-641]: https://github.com/AntoineToussaint/lazybox/issues/641
[modal-pricing]: https://modal.com/pricing
[modal-security]: https://modal.com/docs/guide/sandbox-networking
[openai-pricing]: https://developers.openai.com/api/docs/models/gpt-5.6-terra
[stripe-pricing]: https://stripe.com/pricing
