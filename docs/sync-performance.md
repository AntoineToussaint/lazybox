# GitHub sync performance

This documents the per-poll cost instrumentation added for issue #12
and the first measured trace from a real account. The goal was to
replace gut feel ("polls still feel slow") with numbers, then decide
what to change.

## Capturing a trace

The PR-fetch path emits structured metrics under the `gh_sync_metrics`
tracing target — off by default, so production logs are unchanged.
Turn it on with `RUST_LOG=gh_sync_metrics=debug` (the daemon already
honours `RUST_LOG`; logs land in `/tmp/lazybox.log`).

To capture a clean, isolated trace without the full TUI, run the
ignored integration harness against your own account:

```sh
# token resolves from $GH_TOKEN, $GITHUB_TOKEN, or `gh auth token`
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

**1. GraphQL cost is a non-issue.** Every query costs 1–2 points; a
whole poll is ~10 points against a 5000/hr GraphQL budget and the
local 30 req/min bucket. The rate-budget work is sound — cost is not
why polls feel slow. **Wall-clock is the entire story.**

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
per batch — about 8% of today's ~11 s poll. Even once #14 collapses
the main poll, this does **not** become a fixed per-tick tax: the
`prefetched_pr_details` dedup set means a PR is prefetched at most
once per daemon session, so prefetch fires only for PRs that *newly*
clear the threshold. On a steady inbox it goes quiet after warm-up;
on a churning inbox it tracks the rate of new actionable PRs, which is
exactly when warming the cache pays off.

### Decision: keep N=5, every tick

No change to cadence or N. The cadence concern in the issue —
"5 calls every tick" — is already mitigated by the per-session dedup,
and the measured worst case (a full cold batch) is cheap in every
dimension. Making it event-driven or reducing N would add complexity
to shave ~1 s off a path that is already self-limiting and only runs
when there is genuinely new detail worth pre-warming. The one fix
warranted by the data was the wrong cost number in the handler
comment, now corrected.

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
