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
  cargo test -p lazybox-gh --test sync_trace -- --ignored --nocapture
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

## Recommended follow-ups

Filed as separate issues so each can be sized and shipped on its own
(#14, #15, #16):

1. **Incremental `updated:>=` window on `involves-main`** (#14, highest
   leverage). Narrow the main search to PRs changed since the last
   poll. On a steady inbox this collapses 2 slow pages (~14 s, 143 KB)
   to a near-empty first page, cutting the dominant branch — and thus
   the whole poll — by an order of magnitude. Needs a full-sweep
   fallback (the notifications heartbeat already has the scaffolding).

2. **Stop the watched-repo fan-out from re-fetching `involves:` PRs** (#15,
   *done*). Each watched-repo query now carries `-involves:USER`, so
   GitHub never returns the PRs the main `involves:` branch already
   covers. The duplicate download is filtered server-side: bytes drop
   and `dedup_pct` stays low as watched-repo count grows.

3. **Quantify and reconsider per-tick detail prefetch** (#16). `fetch_pr_details`
   is now instrumented (`branch="pr-details"`); capture its share under
   a real prefetch cycle and decide whether the top-5 prefetch every
   tick earns its cost once incremental sync lands.
