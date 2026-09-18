# The GitHub provider

Polls GitHub PRs and issues into `Vec<Task>` via octocrab and GraphQL. It
depends on `lazybox-core` and `lazybox-auth` only, and the dep-rules test
keeps it that way.

Read [`AGENTS.md`](../../AGENTS.md) first; this file only adds provider depth.

## Discovery is repo-first

With scoped or watched repos, the daemon sweeps every roster member with one
windowed PR query plus one issue query, on a rotation sized by
`providers.github.repo_refresh_interval`. A periodic unwindowed reconcile
sweeps the whole roster and is the only pass allowed to retire rows. The
user-centric `involves:USER` global sweep runs only when no scopes are
configured.

A rotation batch is preceded by one batched freshness probe: GraphQL has no
ETag, so a watermark stands in for `If-None-Match`, and a member whose newest
item predates its window floor is completed without a query. A PR walk that
hits its page cap re-asks under `updated:<=<oldest fetched>` rather than
failing the member — which is why every sweep query carries
`sort:updated-desc`.

The discovery filters' role term rides each member PR query: `pr.author`
becomes `author:USER`, and two or more roles become `involves:USER` plus a
`review-requested:USER` companion — GitHub's `involves:` omits review
requests, which is why the companion query exists and must not be dropped as
redundant. Watched repos stay unscoped, and the issue query is never
role-scoped because the `@lazybox` mention scan reads it.

Rationale and measurements: [`docs/sync-performance.md`](../../docs/sync-performance.md).

## Roles, and why `Observer` exists

A PR or issue whose payload never names the viewer derives to
`TaskRole::Observer`. Observer has no key on any filter schema, so the role
gate never admits it — which is what makes `Mentioned` mean a real @-mention,
comment or review by the viewer rather than "appeared in some query".
`graphql::mark_involved` lifts a task out of `Observer` when the query that
returned it named the viewer.

## What the filters drop, silently

`ProviderConfig::default_for("github")` ships `pr.*` keys only, so
`issue_enabled()` is false on a default install: both the repo sweep's issue
half and the `involves:USER is:issue` probe are skipped.
`filter_github_tasks_with_watches` additionally drops an out-of-scope repo.

This matters when you are reasoning about agent-facing behaviour: filing an
issue does not by itself produce a row, and neither the agent nor the caller
can see that from inside. Text that promises a workspace for a filed issue is
wrong, and `crates/core/tests/agent_work_preamble.rs` enforces that it is
never written.

## Rate budget

`rate_budget.rs` governs request spend against GitHub's limits. A sweep that
feels slow is usually the governor doing its job — measure before widening a
window or shortening an interval, and see
[`docs/github-api-governor.md`](../../docs/github-api-governor.md).
