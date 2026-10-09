# The GitHub provider

Polls GitHub PRs and issues into `Vec<Task>` via octocrab and GraphQL. It
depends on `lazybox-core` and `lazybox-auth` only, and the dep-rules test
keeps it that way.

Read [`AGENTS.md`](../../AGENTS.md) first; this file only adds provider depth.

## Discovery is repo-first

With scoped or watched repos, the daemon sweeps every roster member with one
windowed PR query plus one issue query, on a rotation sized by
`providers.github.repo_refresh_interval`. A periodic unwindowed reconcile
sweeps the whole roster and is the only pass allowed to retire a row by its
ABSENCE — a windowed pass drops `is:open`, so it observes a close or a merge
directly and retires that row itself. The user-centric `involves:USER` global
sweep runs only when no scopes are configured.

The reconcile drains one governor-sized batch per tick, so its admission is
priced at a single roster member (`RECONCILE_ADMISSION_MEMBERS`). Pricing it
at the whole roster is what starved it — and absence-based retirement with it
— past ~25 repos (#1806).

A rotation batch is preceded by one batched freshness probe: GraphQL has no
ETag, so a watermark stands in for `If-None-Match`, and a member whose newest
item predates its window floor is completed without a query. A PR walk that
hits its page cap re-asks under `updated:<=<oldest fetched>` rather than
failing the member — which is why the PR sweep query carries
`sort:updated-desc`. A member that needed more than one window is fetched
in full but withheld from the reconcile's retirement list: the walk spans
seconds, and a PR touched mid-walk rises above every later ceiling and is
never returned, so deleting on that set would retire a live row.

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

## Two credential chains

`credential_chain(host)` is the **user's** token — env vars, `gh auth token`,
then the stored OAuth login. Everything that authors as the user resolves it,
and so does every agent session.

`poller_credential_chain(app, host)` is the daemon poller's alone: a single
`InstallationTokenProvider` (`app_auth.rs`) minting a GitHub App installation
token, which carries its own rate-limit budget so agents cannot starve the
inbox (#1802). It is deliberately NOT prepended to the user chain — a
combined chain would hand the App token to agents and mutations, putting both
back on one budget and attributing the user's comments to the bot. Each chain
has its own cache scope helper; `tests/credential_scope_pairing.rs` pins both
pairings in source.

An installation token has no user. `GET /user` and `GET /notifications` both
fail for it, and GraphQL `viewer` resolves to the App's bot — so
`from_credential_with_host_as` takes the viewer login instead of asking, and
the budget bootstrap sends a `rateLimit`-only probe.

## Rate budget

`rate_budget.rs` governs request spend against GitHub's limits. A sweep that
feels slow is usually the governor doing its job — measure before widening a
window or shortening an interval, and see
[`docs/github-api-governor.md`](../../docs/github-api-governor.md).

## A claim is a label plus a comment

"An agent is working on this" is two upstream facts, and the split is what
makes it affordable (#1922):

- **Presence** is the one stable `working` label
  (`lazybox_core::WORKING_LABEL_NAME`). It rides free in the poll payload, so
  `Task::has_working_claim()` costs no request on any tick, and attaching it
  needs repository write access — the same property
  `DeclarationScope::LabelsOnly` rests on (#1600).
- **Identity** is one sticky comment per record, marked
  `<!-- lazybox:claim -->` and carrying holder / agent / model / started /
  heartbeat / expiry (`lazybox_core::WorkingClaimNote`). `apply_working_claim`
  edits it in place on every heartbeat, so four heartbeats leave one comment,
  not four.

Read the label for presence; fetch the comment only at a decision point —
`read_working_claim_note`, from `task_status` or the lapsed-claim sweep. Never
from a poll tick.

**A claim comment counts only when lazybox authored it.** Anyone can post the
marker; only writers can label. `find_sticky_comment_body` matches on
`c.user.login == self.user` *and* the marker, and
`WorkingClaimNote::parse` anchors the marker at the start of the body so a
quoted copy is not a second claim. Both halves are the trust property — a
reader that drops either lets a drive-by comment cancel or fake a claim.

`StickyComment` descriptors (`TRAILER_STICKY`, `CLAIM_STICKY`) carry the
operation labels their REST calls are budgeted under, because
`request_profile` keys on the label and its fallback arm is `Interactive`. A
claim heartbeat on that tier would bypass the reserve and the per-tick
allowance exactly as the claim label writes did before #1218 — so the four
claim-comment operations are spelled out in the `Cold` arm, separately from
the `post issue comment` / `update issue comment` labels a user's own comment
uses.

Per-heartbeat cost, steady state: **one** request (the in-place comment edit).
`attach_label` comes from the caller's poll payload, so a label that never
moved costs nothing and a label a human stripped is re-attached. The
predecessor spent two (list the issue's labels, rename the per-claim label to
its new expiry) and minted one label per claim on the repository forever.

`lazybox:w:<device>:<session>:<expiry>` labels are legacy and read-only:
`QualifiedWorkingClaim` still parses them so a claim held by a box on an older
build is honoured, and `remove_working_claim_labels_target` retires one. The
`role:<…>` labels are a separate mechanism and untouched by any of this.
