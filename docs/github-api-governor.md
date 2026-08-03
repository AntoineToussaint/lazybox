# GitHub API budget governor

lazybox uses one governor for scheduled GitHub reads, interactive
provider actions, and retries. Its default policy permits background
work to use at most 55% of each observed primary resource budget. The
remaining 45% is reserved for `gh`, spawned agents, explicit refreshes,
and unexpected bursts.

Configure the background share only when the default is inappropriate:

```yaml
providers:
  github:
    background_budget_share: 0.55
```

Finite values are clamped to 5–90%. A manual `Shift-R` is interactive:
it can preempt scheduled work while still respecting GitHub's hard
primary limit, the shared secondary-limit circuit, and the local
concurrency cap.

## Admission and accounting

Every `GhClient` clone shares the same governor, eight-request
concurrency gate, and mutation mutex. Parallel search, notification,
detail, and mutation branches therefore cannot each spend the full
observed budget.

GitHub's secondary (abuse) limit keys on burst rate and concurrency
rather than the primary budget, so a sweep with plenty of primary
headroom can still trip it. Beyond the concurrency gate, request
*starts* are spaced by a minimum gap (200 ms baseline) so a sweep
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
forecast and emits a `gh_governor` warning.

The tick allowance is:

1. remaining capacity above the configured reserve;
2. less projected external consumption through reset;
3. divided over the ticks remaining in the window.

A complete fixed full-sweep unit is reserved before repository fan-out
is selected. Focused work comes first. Session-bearing repositories
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
