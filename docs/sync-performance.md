# GitHub sync performance

This documents the per-poll cost instrumentation added for issue #12
and the first measured trace from a real account. The resource-aware
budget policy and reproducible four-scenario after report live in
[github-api-governor.md](github-api-governor.md).

## Capturing a trace

The PR-fetch path emits structured metrics under the `gh_sync_metrics`
tracing target — off by default, so production logs are unchanged.
Turn it on with `RUST_LOG=gh_sync_metrics=debug` (the daemon already
honours `RUST_LOG`; logs land in `/tmp/lazybox.log`).

To capture a clean, isolated trace without the full TUI, run the
ignored integration harness against your own account:

```sh
# token resolves from $LAZYBOX_GITHUB_TOKEN, $GH_TOKEN, $GITHUB_TOKEN, or `gh auth token`
LAZYBOX_WATCH=owner/repo-a,owner/repo-b \
  cargo test -p lazybox-gh --test sync_trace capture_fetch_all_prs_trace \
    -- --ignored --nocapture
```

`capture_fetch_all_prs_trace` measures only the main poll. The
`capture_prefetch_trace` test in the same file measures the second
phase the server runs every tick — `prefetch_top_pr_details` — by
scoring the just-polled PRs with the same logic as the handler and
firing the top-5 `fetch_pr_details` calls (3 concurrent), each
emitting a `branch="pr-details"` line:

```sh
LAZYBOX_WATCH=owner/repo-a,owner/repo-b \
  cargo test -p lazybox-gh --test sync_trace capture_prefetch_trace \
    -- --ignored --nocapture
```

Each branch emits one line when it finishes:

```
branch fetch complete  branch="involves-main" elapsed_ms=13844 requests=2 prs=49 graphql_cost=2 resp_bytes=143620
```

and `fetch_all_prs` emits a union-level breakdown after dedup:

```
fetch_all_prs union breakdown  elapsed_ms=13845 total_fetched=75 unique=74 duplicates=1 dedup_pct="1" main=49 reviewer=1 merged=25 watched=0 watched_repos=0
```

Fields:

- `elapsed_ms` — wall-clock for that branch (entry to all pages parsed).
- `requests` — HTTP round-trips (pages for the paginated search; 1 otherwise).
- `prs` — PRs the branch returned **before** cross-branch dedup.
- `graphql_cost` — sum of GitHub's reported `rateLimit.cost` for the branch.
- `resp_bytes` — raw response bytes deserialized.
- `duplicates` / `dedup_pct` — PRs fetched by more than one branch and thrown away at union.

## Measured trace

Account: `AntoineToussaint`, 74–83 open/recent PRs in the inbox.
Two runs: one with no watched repos, one with 7 watched repos. The
4 branches run concurrently (`tokio::join!`), so total wall-clock is
the **slowest** branch, not the sum.

### Run A — 0 watched repos (total 13.8s)

| Branch           | elapsed | requests | PRs | cost | bytes  |
|------------------|--------:|---------:|----:|-----:|-------:|
| review-requested |   1.2 s |        1 |   1 |    1 |   5.7 KB |
| merged-sweep     |   1.8 s |        1 |  25 |    1 |  58.8 KB |
| **involves-main**| **13.8 s** |     2 |  49 |    2 | 143.6 KB |
| **union**        | **13.8 s** |        |  75 fetched → 74 unique (**1% dup**) |

### Run B — 7 watched repos (total 16.2s)

| Branch              | elapsed | requests | PRs | cost | bytes  |
|---------------------|--------:|---------:|----:|-----:|-------:|
| review-requested    |   1.4 s |        1 |   1 |    1 |   5.7 KB |
| merged-sweep        |   2.0 s |        1 |  25 |    1 |  58.8 KB |
| watched-repo ×6     | 0.3–0.7 s each | 1 | 0–2 | 1 | 0.2–5.4 KB |
| watched-repo (busy) |   6.3 s |        1 |  18 |    1 |  89.6 KB |
| **involves-main**   | **16.2 s** |     2 |  49 |    2 | 143.6 KB |
| **union**           | **16.2 s** |        | 100 fetched → 83 unique (**17% dup**) |

## What the numbers say

**1. Individual GraphQL operations are cheap, but cadence still
matters.** Every query costs 1–2 points. Repeating a cheap fan-out too
often can still crowd out `gh` and agents, so the governor budgets the
reported points and protects a reserve. For one poll's perceived
latency, wall-clock remains the dominant factor.

**2. `involves-main` is the poll.** It alone takes 13.8–16.2 s and
every other branch finishes while it is still running, so it sets the
floor for the whole cycle. 49 PRs paginate into 2 sequential pages
(cursor-dependent) at 25/page; each page is a ~7 s round-trip because
GitHub's GraphQL gateway is slow to resolve the heavy `SEARCH_QUERY`
connections (commits, labels, assignees, reviewRequests, comments per
PR). Optimising any *other* branch cannot move the total — only
`involves-main` can.

**3. We re-download everything every tick.** There is no incremental
cursor: each poll pulls all 49 open PRs in full (143 KB) and diffs in
memory, even on a steady inbox where nothing changed. Page 2 is almost
entirely unchanged rows poll-over-poll.

**4. Overlapping branches waste bytes and grow with scale.** Dedup
waste went from 1% (0 watched repos) to **17%** (7 watched repos):
the watched-repo fan-out re-fetches open PRs the user is already
involved in, which `involves-main` returned moments earlier. The busy
watched repo alone re-downloaded 89.6 KB of mostly-duplicate PRs. At
10+ watched repos this branch count and its overlap dominate.

## Repo-first discovery

Discovery used to be user-centric: one `involves:USER` global search
(plus `review-requested:`, a 7-day merged sweep and per-watched-repo
queries), with the user's scoped repos applied only as a post-fetch
filter, and a 3-repo round robin that rarely ran. Two failure modes
fell out of that shape on 2026-09-05:

- **Stuck manual sweep.** `Shift-R` set `force_full_sweep`, which only
  cleared when the sweep's *coverage* was complete. The unwindowed
  merged sweep paginated 10+ pages under a busy fleet, tripped GitHub's
  secondary limit (HTTP 403), marked coverage partial, and the forced,
  governor-bypassing unwindowed global sweep re-ran every minute for
  hours (~30–45 s, ~2 MB, ~40 GraphQL points per tick, then a 60 s
  pause). The `updated:>=` floor never advanced either.
- **Silent config-parse dead loop.** A newer build wrote a field this
  daemon could not parse; every tick returned early on
  `Config::load()` with no user-visible error for 88 minutes.

The daemon now discovers **per repo** whenever the user has scoped or
watched repos (the *roster*):

- **Rotation (every warm tick).** Focus + session-bearing repos, then
  the `ceil(roster / target_ticks)` stalest members, where
  `target_ticks = repo_refresh_interval / poll_interval` (defaults:
  5 min / 60 s → every member within 5 ticks). Each member costs one PR
  query and one issue query, windowed on its own persisted
  `updated:>=` floor (`SyncCursors::repo_windows`). Windowed queries
  drop `is:open` so a merge or close comes back with its new state —
  no separate merged sweep. The governor caps the slice by allowance;
  members that don't fit keep their cursor age and lead the next tick.
- **Reconcile (every `FULL_SWEEP_INTERVAL`, or `Shift-R`).** Every
  member unwindowed (`is:open` plus a 7-day recent-activity query),
  queued in `TickState::reconcile_pending` and drained one
  fan-out-sized batch per warm tick (double on a manual refresh), focus
  first. Run in a single tick, a 28-repo reconcile (~84 requests)
  emptied the 30-request local bucket and left the heartbeat, detail
  prefetch and the user's own `g m` pre-check refused for ~3 minutes.
  Each batch reports `PolledScope::Reconcile { swept, roster }`: it
  retires gone rows within the members it swept this tick AND rows whose
  repo has left the roster entirely (a de-scoped repo — which can hold no
  live in-scope row), while preserving a roster member swept by a
  *different* batch (this tick never fetched it) and a member whose query
  failed. De-scope retirement was previously carried by an `Exhaustive`
  coverage report, but that only ever fired when the whole roster fit one
  batch — essentially a single-repo roster under the default cadence — so
  batching had silently stopped retiring de-scoped rows; expressing the
  authority per-batch restores it without the per-tick-complete
  assumption `Exhaustive` makes (which under batching would delete the
  other batches' live rows). The reconcile timer re-arms once the last
  batch has run, so a member that overflows its page cap cannot make the
  whole roster re-run every tick.
- **`g s`** stays the interactive "sync this repo now": the focused
  row's PR/issue plus the repo's open PRs and issues, at interactive
  priority, outside the poll loop.
- **Org scopes** are one roster member each (`org:name`), covered by a
  single query pair rather than one per repo.
- Without a roster (no scopes), the legacy `involves:USER` global sweep
  and discovered-repo round robin still run. Both paths now advance
  their floors and clear a forced refresh on *discovery* success
  (`SelectedFetchOutcome::discovery_complete`), independent of
  best-effort companions; the merged sweep is always windowed on its
  own floor and capped at 4 pages.

Hot rows are the focused row, every row with a live agent, and up to
three recent own PRs; a merely session-bearing worktree is no longer
hot (20–40 idle worktrees used to pin the 15 s cadence permanently) —
its repo is force-included in every rotation tick instead.

A config file that fails to parse now surfaces as a permanent GitHub
provider error naming the file, and `ui.keep_awake` accepts the newer
mode strings so a newer client cannot brick an older daemon.

### "GitHub rate-limited" vs. lazybox pacing itself

The wait event now carries `self_throttle`. When lazybox's own local
bucket or background allowance is what's pausing *scheduled* work while
the primary budget is healthy, the footer reads `pacing GitHub sync ·
~2m · 4053/5000 left` rather than `GitHub rate-limited`. User actions
(`g m`, `g s`, replies) are interactive and go through regardless — a
merge rejected during that state was rejected by GitHub for its own
reason (e.g. "2 of 2 required status checks are expected": CI hasn't
reported yet), which the merge notice spells out.

### Merging during a rate-limit pause

A secondary-limit cooldown used to make `g m` fail or queue for
minutes: the governor admitted one interactive request per 20 s
window, but a merge is three back to back (fresh pre-merge fetch,
merge-method lookup, mutation), so the mutation itself was refused with
the whole remaining pause as its retry hint. Now:

- interactive requests get a burst of four per window
  (`SECONDARY_INTERACTIVE_BURST`), enough for one user action;
- the repo's merge method is cached per `owner/name` after the first
  merge, so later merges are a single mutation (a rejected cached
  method is refetched and retried once when it changed);
- while the circuit is open, the merge path skips the optional fresh
  pre-check and merges from the last synced row, spending the ration
  on the mutation — GitHub still rejects a moved head or a conflict.

## Engagement tiers

The daemon overlays three engagement tiers on the discovery and
notifications paths:

- **Hot**: the focused workspace and every workspace with a live agent
  (uncapped — one batched `nodes(ids:)` query), plus open authored PRs
  updated in the last 24 hours, capped at three. While non-empty, the poll loop runs every 15 seconds, targets
  these rows before notification targets, and refreshes the whole set
  with one full-detail `nodes(ids:)` GraphQL request per pass.
- **Warm**: the remaining active inbox. These rows keep the configured
  base cadence, notifications heartbeat, round-robin searches, and
  one-time attention-ranked detail prefetch.
- **Cold**: snoozed or terminal-state rows. Cold-only repositories are
  removed from the per-repo round robin, and their notification
  targets wait for global discovery; their rows use the lean search
  payload and skip detail prefetch.

Focus changes and successful agent registration notify the sleeping
poll loop immediately. Provider rate-limit backoff remains
authoritative and can extend either cadence.

The `/v1/metrics` response reports delivery-age samples for observable
GitHub surface changes that replace an existing task:

- `hot_sync_samples`, `hot_sync_p50_ms`, `hot_sync_p95_ms`,
  `hot_sync_p99_ms`
- `warm_sync_samples`, `warm_sync_p50_ms`, `warm_sync_p95_ms`,
  `warm_sync_p99_ms`
- `cold_sync_samples`, `cold_sync_p50_ms`, `cold_sync_p95_ms`,
  `cold_sync_p99_ms`

Each histogram retains the latest 1,024 samples. When `updatedAt`
advances, the sample is the upstream event's age. GitHub does not
advance that timestamp for every CI or mergeability recomputation, so
same-timestamp surface changes use the interval since the previous
observation. First discovery is excluded.

## Detail prefetch cost (#16)

After every successful poll the server runs `prefetch_top_pr_details`:
it scores the just-polled PRs (CI failing +100, review
pending/changes-requested +50, unread +10 each capped at +50) and
fires `fetch_pr_details` for the top 5 (3 concurrent) to warm the
right pane. Issue #12 only traced `fetch_all_prs`, so this phase was
unmeasured. `capture_prefetch_trace` reproduces it.

### Measured trace

Same account (`AntoineToussaint`), 5 watched repos, immediately after
the `involves-main` poll above. All 5 prefetch slots filled (5 PRs
cleared the score threshold):

| Call (`branch="pr-details"`) | elapsed | requests | cost | bytes  |
|------------------------------|--------:|---------:|-----:|-------:|
| 1                            |   438 ms |       1 |    1 | 19.6 KB |
| 2                            |   543 ms |       1 |    1 |  4.7 KB |
| 3                            |   611 ms |       1 |    1 | 27.3 KB |
| 4                            |   458 ms |       1 |    1 |  4.5 KB |
| 5                            |   415 ms |       1 |    1 |  4.3 KB |
| **phase total (conc. 3)**    | **959 ms** |     5 |  **5** | **60.5 KB** |

### What the numbers say

**1. GraphQL cost was over-estimated 100×.** The handler comment
justified N=5 by assuming ~550 units per `fetch_pr_details` call.
GitHub's reported `rateLimit.cost` is **1** per call — a single
node-id lookup, not a paginated search. A full prefetch batch is 5
units; at one batch/minute that's 300/hr against the 5000/hr budget.
Cost is a non-issue, exactly as for the main poll.

**2. Wall-clock and bytes are modest and bounded.** ~1 s and ~60 KB
per batch — about 8% of today's ~11 s poll. Warm rows remain
self-limiting through `prefetched_pr_details`. Hot rows skip this
phase because their bounded set already receives review threads and
check contexts in the single batched targeted query.

### Decision: keep N=5

The measured worst case (a full batch) is cheap in every dimension.
Warm rows still fetch once per daemon session; the hot set uses its
separate batched refresh.

## Recommended follow-ups

Filed as separate issues so each can be sized and shipped on its own
(#14, #15, #16):

1. **Incremental `updated:>=` window on `involves-main`** (#14, highest
   leverage). **Done.** The global `involves:` PR sweep now narrows to
   `updated:>=<last sweep start>` on a steady inbox, collapsing the 2
   slow pages (~14 s, 143 KB) to a near-empty first page. The window
   floor advances each global sweep (`GhClient::record_pr_sweep_window`);
   a windowed sweep can't observe PRs that left the search without an
   `updatedAt` bump (silently closed, un-involved, transferred), so one
   sweep per `FULL_RECONCILE_INTERVAL` (1 h) — plus the first sweep
   after start and every manual refresh — drops the window and
   reconciles the whole inbox. Only that unwindowed reconcile reports
   `PolledScope::Exhaustive`, so a windowed sweep never drives deletion.
   To profile the windowed path, pass a recent `Some(updated_since)` to
   `fetch_all_prs` in the `sync_trace` harness instead of `None`.

2. **Stop the watched-repo fan-out from re-fetching `involves:` PRs** (#15,
   *done*). Each watched-repo query now carries `-involves:USER`, so
   GitHub never returns the PRs the main `involves:` branch already
   covers. The duplicate download is filtered server-side: bytes drop
   and `dedup_pct` stays low as watched-repo count grows.

3. **Quantify and reconsider per-tick detail prefetch** (#16,
   resolved). Measured: ~1 s / ~60 KB / 5 GraphQL units per batch, and
   self-limiting via the per-session dedup set. Decision is to keep
   N=5 on every tick — see [Detail prefetch cost (#16)](#detail-prefetch-cost-16)
   above.
