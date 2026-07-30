# GitHub App and webhook decision

## Decision

Keep PAT/`gh auth token` polling as the default architecture. Do not
ship a required hosted relay in lazybox now. Preserve GitHub App
installation tokens plus push delivery as the next architecture
option if polling cost or product requirements outgrow the governor.

The governor's deterministic quiet replay spends 14 GraphQL points per
hour, down from 84, while the notification heartbeat remains the
freshness path. A relay could save at most those 14 scheduled GraphQL
points in the quiet case. That saving does not justify adding a
publicly reachable, multi-tenant service that must retain and replay
private repository payloads. The local process is commonly behind NAT
and cannot receive GitHub webhooks directly. Those are quantified
capacity and operational reasons to defer a hosted proof of concept,
not an assumption that a second PAT creates capacity.

A second PAT for the same GitHub user is not an isolated pool. GitHub
documents user access token requests as attributed to the user.
Installation access tokens instead use an installation budget that
can scale with repositories and organization users:
[GitHub App rate limits](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/rate-limits-for-github-apps?apiVersion=2022-11-28).

## Installation-token design

The user would:

1. install the lazybox GitHub App on selected repositories;
2. give lazybox the App ID, installation ID, and a private-key
   reference, or authenticate to a trusted relay;
3. see installation visibility beside the existing PAT credential
   source and fall back to PAT for repositories outside the
   installation.

The minimum read-only installation requests:

- Metadata: read (implicit);
- Pull requests: read;
- Issues: read;
- Checks: read;
- Commit statuses: read;
- Contents: read, for default-branch and branch-state reads.

Existing write actions require explicit opt-in write permissions:
Pull requests for review/merge/update actions, Issues for comments,
labels and assignees, and Contents where GitHub requires it for branch
updates. The registration flow should request only features the user
enables. GitHub's permission-selection guidance is the source of truth:
[choosing GitHub App permissions](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/choosing-permissions-for-a-github-app?apiVersion=2022-11-28).

Only the token-minting boundary may read the App private key. It signs
a short-lived JWT, exchanges it for an installation token restricted
to the selected repositories and permissions, keeps the token in
memory, and refreshes before its one-hour expiry. The SQLite state may
store installation ID and expiry but never the private key or bearer
token. GitHub documents the exchange and restriction fields here:
[generating an installation access token](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app).

## Push delivery

The event set needed to invalidate lazybox state is:

- `pull_request`, `pull_request_review`,
  `pull_request_review_comment`;
- `issues`, `issue_comment`;
- `check_run`, `check_suite`, `status`;
- repository and installation events for access/default-branch
  invalidation.

Assignment, label, close, reopen, synchronize, and merge changes are
actions within the PR/issue events. The canonical list and payload
schemas are in [GitHub webhook events and payloads](https://docs.github.com/en/webhooks/webhook-events-and-payloads).

Two delivery modes are viable:

| Mode | Benefit | Cost/risk |
|---|---|---|
| User-hosted receiver | Repository data stays under the user's control; no lazybox service dependency | Requires a public TLS endpoint and durable queue |
| Hosted relay | Works for laptops behind NAT and can reconnect seamlessly | Requires multi-tenant authentication, encrypted retention, abuse controls, availability, deletion policy, and an operated service |

In either mode, the receiver validates GitHub's HMAC signature before
persisting, keys deduplication by `X-GitHub-Delivery`, stores an
installation/repository plus monotonic relay sequence, and acknowledges
only after durable enqueue. A reconnecting TUI sends its last
acknowledged sequence; the relay replays later envelopes and retains
them for a bounded window. GitHub's delivery guidance covers local
forwarding for development, but GitHub explicitly describes webhook
forwarding tools as development aids rather than a production relay:
[handling webhook deliveries](https://docs.github.com/en/webhooks/using-webhooks/handling-webhook-deliveries?apiVersion=2022-11-28) and
[validating deliveries](https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries).

Push remains invalidation, not the sole source of truth. The receiver
deduplicates delivery IDs and asks the existing targeted fetch path for
the current object. Persisted per-branch cursors survive reconnects.
The conditional notifications heartbeat remains a fallback, and the
hourly unwindowed reconcile repairs a dropped, reordered, or
unsupported event.

## Concrete next spike

A future spike is ready to be bounded as:

- a user-hosted receiver binary with one installation and an SQLite
  delivery queue;
- fixture tests for signature rejection, delivery-ID deduplication,
  replay after reconnect, and installation-token refresh;
- a 24-hour comparison of push-triggered targeted reads versus the
  governor report.

Shipping a hosted relay should require a separate decision that owns
service deployment, security review, retention, and operating cost.
The current repository contains none of those boundaries, so a “thin”
hosted implementation here would be misleading rather than a safe
proof of concept.
