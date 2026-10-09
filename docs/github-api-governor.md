# GitHub API budget governor

lazybox uses one governor for scheduled GitHub reads, interactive
provider actions, and retries. Its default policy permits background
work to use at most 55% of each observed primary resource budget. The
remaining 45% is reserved for merge/reply actions, `gh`, spawned agents,
and unexpected bursts.

Configure the background share only when the default is inappropriate:

```yaml
providers:
  github:
    background_budget_share: 0.55
```

Finite values are clamped to 5–90%. A manual `Shift-R` requests a full
sweep with an allowance drawn from the remaining non-reserved window,
rather than a single tick’s grant. The scheduler checks that allowance
before starting; every page still respects pacing and the action reserve.
Targeted interactive requests and mutations can use that reserve, including
the last 100 emergency points,
provided their forecast fits the remaining quota. Actual primary
exhaustion still blocks requests until reset.

## A separate budget for the sweep

The share above divides **one** budget. Registering a GitHub App
(`providers.github.app`) instead gives the status sweep an installation
credential with a 5,000/hour of its own, so agent traffic and scheduled
polling stop competing at all — the reserve then protects interactive
actions from the sweep rather than the sweep from agents.

Two governors then exist, one per client, each observing its own limit
headers. `PollState::polling_gh_client()` names the one the sweep is running
on; `cached_gh_client()` stays the user's, for mutations and reads. Without
an App, both are the same client and the share above is the whole story.

That splits the per-client gates too: two clients means two eight-request
concurrency gates (up to sixteen in flight to GitHub) and two secondary-limit
circuit breakers that do not observe each other's backoff. They are separate
actors to GitHub — the installation and the user each have their own primary
and secondary limits — so the split is correct, but a reader reasoning about
total in-flight requests must count both.

What the installation reaches moves onto the App budget; what it does not
stays on the user token, in the same tick. Repo-first discovery is a
per-member fan-out, so the roster partitions cleanly by credential, and every
read is routed to the token that can see its repo — a credential missing a
repo does not fail, it returns fewer rows and no error.

Two shapes cannot be partitioned and still put the whole sweep back on the
user token with a notice naming the reason: `include_accessible_repos`, whose
roster is open-ended, and a sweep with no roster at all — that one is a single
global `involves:` search, and there is nothing to split.

Because a split makes the user client do scheduled work of its own, it also
gets its own per-tick governor pass; a budget that never begins a tick never
clears its per-tick accounting, and would eventually refuse everything
scheduled.

## Admission and accounting

Every clone of *one* `GhClient` shares that client's governor,
eight-request concurrency gate, and mutation mutex. Parallel search,
notification, detail, and mutation branches therefore cannot each spend the full
observed budget.

GitHub's secondary (abuse) limit keys on burst rate and concurrency
rather than the primary budget, so a sweep with plenty of primary
headroom can still trip it. Beyond the concurrency gate, request
*starts* are spaced by a minimum gap (500 ms baseline) so a sweep
cannot fire its whole allowance at once. The gap adapts: it widens
while a secondary limit is recent, and widens with the measured
external burn on the shared token so the daemon leaves inter-request
headroom when interactive `gh`/agents are busy. An idle period never
banks burst credit, and the gap is clamped to a five-second ceiling.

Primary budgets are tracked independently:

- GraphQL is admitted and reconciled in reported `rateLimit.cost`
  points, including `limit`, `remaining`, `used`, and `resetAt`.
- REST is keyed by `x-ratelimit-resource` (`core`, `search`, and any
  future bucket) and reconciled from the limit, remaining, used, and
  reset headers already returned by useful requests.
- The drop in `used`/`remaining` between observations, less lazybox's
  own reported cost, becomes the projected external burn rate. The
  next plan shrinks before the emergency floor is reached.

Admission records operation class, resource, priority, forecast cost,
and the local decision. Responses record status, conditional result,
actual cost, bytes, duration, and forecast error. The governor retains
p50/p95/p99 request latency and per-tick plus process totals. A
material GraphQL forecast miss raises that operation's conservative
forecast and emits a `gh_governor` warning. A refusal logs at most once
per operation per minute, carrying the count it swallowed: a starved
governor refuses every request of every operation for as long as it
stays starved, and one 2026-09-17 outage wrote ~35,000 identical
`repo-sweep blocked by rate budget` lines.

The tick allowance is:

1. remaining capacity above the configured reserve;
2. less projected external consumption through reset;
3. divided over the ticks remaining in the window.

A small burst of `Focused` requests per 30 s may pass a **self-imposed**
refusal — an empty local token bucket or a spent tick allowance — so a
targeted refresh of the row the user is looking at still returns current
state while the background sweep is paced out (#1803). The allowance is a
burst, not a single request, because one refresh is not one call: the hot
fetch is a freshness probe followed by a detail fetch for whatever moved,
and admitting only the probe refreshes the row exactly when nothing
changed. It never passes the
gates GitHub itself imposes: remaining-low, the action reserve, and an
open circuit still refuse it, because the reserve exists so the user's
own merges and replies fit.

A complete fixed full-sweep unit is reserved before repository fan-out
is selected. That unit is priced at the batch the sweep will actually
run — a repo-first reconcile drains one governor-sized batch per tick,
so its admission costs one roster member, not the roster. Pricing it at
the roster made the sweep unadmittable past ~25 repositories, which took
row retirement with it (#1806). Focused work comes first. Session-bearing repositories
then rotate stale-first; if all cannot fit, the ones not selected keep
their old cursor and lead a later tick. Recently active repositories
use the remaining round-robin slots. Cold repositories leave the
per-repository fan-out but remain covered by the hourly unwindowed
reconcile. Nothing is removed from a tier because of budget pressure.

## Limit protocol

The response classifier distinguishes primary exhaustion from
secondary/abuse limits:

- `Retry-After` is authoritative.
- A zero primary remainder opens the shared circuit until the REST
  reset header or GraphQL `resetAt`, plus one second.
- A secondary response without `Retry-After` starts with a global
  60-second pause, then bounded exponential backoff with jitter up to
  15 minutes.
- REST and GraphQL consult the same circuit. In-call retry never runs
  for a limit response, and every transient retry re-enters admission.
- Dropping the polling future cancels waits normally; there is no
  detached sleeper.

This follows GitHub's guidance to inspect response headers instead of
polling `GET /rate_limit`, pause for at least a minute when a secondary
response has no retry header, and avoid continuing while limited:
[REST rate limits](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api?apiVersion=2022-11-28),
[REST best practices](https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api),
and [GraphQL rate limits](https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api).

## Request shaping and reconciliation

The conditional authenticated `/notifications` heartbeat remains the
fast path and treats `X-Poll-Interval` as a hard minimum. Changed
notifications and focused/hot rows drive targeted node fetches.
Broad PR and issue searches use the lean list payload; review threads,
check detail, and comments stay in targeted or bounded detail fetches.

Successful branch watermarks are serialized in the existing SQLite
key/value store under `github:sync-cursors:v1:<viewer>`. A branch only
advances its cursor after it succeeds. On restart, the next window is
derived from the persisted wall-clock watermark. Cross-branch union
deduplication is counted by the governor.

Each optimization has an explicit coverage closure:

| Optimization | Coverage gap | Reconciliation |
|---|---|---|
| Conditional notifications | GitHub may omit an event or return no task URL | Scheduled broad sweep |
| `updated:>=` search windows | A row may leave a search without a useful update timestamp | Hourly unwindowed sweep |
| Notification-targeted details | CI/mergeability can change without the task timestamp moving | Hot-target refresh and bounded detail prefetch |
| Cold-repo fan-out removal | No per-repo query while the repo stays cold | Hourly global unwindowed sweep |
| Cross-branch deduplication | None after results arrive; it cannot undo bytes already transferred | Query exclusions reduce overlap before the request |

## Observability

Every completed GitHub tick emits a `gh_governor` snapshot and sends
the same compact summary to the TUI. `Shift-D` shows:

- background share and per-resource remaining/limit;
- reserve and this tick's allowance/spend;
- projected external burn per hour;
- request count, GraphQL and REST points, bytes, p95 latency;
- global retry/reset time when the circuit is open.

`/v1/metrics` exposes hot, warm, and cold freshness histograms with
p50/p95/p99 values. The governor log target contains the per-request
records needed to aggregate status, cache hits, forecast errors, and
operation costs.

## Reproducible baseline and after replay

The deterministic one-hour replay is:

```sh
cargo test -p lazybox-server --test github_governor_report -- --nocapture
```

It fixes the topology at 30 scoped repositories and 10
session-bearing repositories. “Current main” replays the former
10-minute broad sweep with 10 repository queries. “Governor” replays
the 30-minute sweep, a three-repository fair slice, the required
60-second notification heartbeat, and the captured 13.8-second broad
query latency from [sync-performance.md](sync-performance.md).
Notification targets use the captured GraphQL response shape under
`crates/gh-provider/tests/fixtures`.

| One-hour scenario | Version | REST requests | GraphQL requests / points | Response bytes | Request p95 | Notification freshness p95 | Reconcile max age |
|---|---|---:|---:|---:|---:|---:|---:|
| Quiet | current main | 60 | 84 | 960,000 | 13.8 s | 60 s | 60 min |
|  | governor | 60 | 14 | 306,000 | 1.8 s | 60 s | 60 min |
| 6 sparse updates | current main | 60 | 90 | 1,032,000 | 13.8 s | 60 s | 60 min |
|  | governor | 60 | 20 | 378,000 | 1.8 s | 60 s | 60 min |
| 12-update burst | current main | 60 | 96 | 1,104,000 | 13.8 s | 60 s | 60 min |
|  | governor | 60 | 26 | 450,000 | 1.8 s | 60 s | 60 min |
| External consumer drains 2,800 points/min | current main | 60 | 84 attempted | 960,000 projected | 13.8 s | 60 s | unbounded at exhaustion |
|  | governor | 60 | 0 scheduled | 0 GraphQL bytes | n/a | 60 s | ≤61 min after one reset |

Quiet GraphQL point consumption falls from 84 to 14, an **83.3%**
reduction. Total HTTP request count falls by 48.6%, not 75%, because
the required conditional notification heartbeat is a hard floor of
60 requests/hour; even eliminating every GraphQL request could reduce
144 total requests by only 58.3%. Conditional `304` heartbeats are
recorded as requests and bytes but do not spend a REST primary point.
GraphQL points are therefore the correct steady-state budget measure.

The replay is deliberately deterministic rather than a claim about
internet latency. Re-run the ignored live trace in
[sync-performance.md](sync-performance.md) when tuning payload or
page-size defaults against GitHub.
