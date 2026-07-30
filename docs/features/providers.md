# Providers

lazybox is source-agnostic: the inbox doesn't care whether a row is a GitHub PR,
a GitHub issue, a Linear ticket, or a Slack thread. Sources plug in behind two
traits in `lazybox-core`. This page covers those traits and the three shipping
providers.

See [`DESIGN.md` § Sources](../../DESIGN.md) for the rationale and the planned
filter grammar, and [Inbox & sync](inbox-and-sync.md) for how provider output
becomes inbox rows.

---

## Provider + Scope traits

**Status:** stable
**Crate(s):** `core` (`src/provider.rs`, `src/scope.rs`)
**Config / flags:** —
**Key bindings:** —

### What it does
Defines the contract every source implements. `TaskProvider` fetches a
`Vec<Task>` and optionally performs mutations (merge, request reviewers, add
labels/assignees). `ScopeSource` lists the orgs/repos/projects a provider can
be scoped to, powering the setup wizard's scope picker.

### How to use it
You don't call these directly — they're the extension points. To add a source,
implement `TaskProvider` (+ `ScopeSource` for setup) in a new
`crates/<x>-provider/` crate and wire its poller into `crates/server/`.
See [`CLAUDE.md` § Adding a new provider](../../CLAUDE.md).

### How it works (brief)
`TaskProvider` (`crates/core/src/provider.rs`) has `name()`,
`async fetch_tasks()`, an optional `username()`, and mutation methods that
default to `Unsupported`. Tasks are normalized into the source-agnostic `Task`
model (`crates/core/src/task.rs`): `TaskState`, `TaskRole`, `CiStatus`,
`ReviewStatus`, `Mergeable`, labels, reviewers, assignees, and merged
`Activity`. `ScopeSource` (`crates/core/src/scope.rs`) returns a tree of
`Scope { id, label, kind, parent }`.

### Test checklist
- [ ] A provider returning tasks produces inbox rows with the right state/role chips.
- [ ] A mutation a provider doesn't support returns `Unsupported`, not a panic.
- [ ] `ScopeSource::list_scopes` / `list_children` populate the setup scope picker.

### Known sharp edges
- Only sources that carry a `branch` get a worktree; branch-less tasks open to activity only.
- The `Task` model is the lowest common denominator; provider-specific fields are flattened into neutral enums.

---

## GitHub provider

**Status:** stable
**Crate(s):** `gh-provider`
**Config / flags:** `setup.scopes` (orgs/repos), `providers.github.poll_interval`; auth via `LAZYBOX_GITHUB_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN` / `gh auth token`; `ui.browser` picks the browser for `g o` / right-clicked links (`"Google Chrome"` → `open -a` on macOS, an executable on Linux; default = OS default browser)
**Key bindings:** GitHub action group — `g m` merge, `g g` auto-merge on green, `g r` reviewers, `g a` assignees, `g l` labels, `g o` open in browser

### What it does
The primary source: GitHub **PRs and Issues** in one query, with labels, CI
status (rolled up across checks), review status, mergeability, base-branch
behind-ness, merge-queue / auto-merge flags, and `#N` cross-references (so an
issue and the PR that closes it can collapse into one row).

### How to use it
Configure scopes in the setup wizard (`,`). Credentials resolve automatically
from `gh auth token` if you've run `gh auth login`. PRs and issues you author,
review, are assigned, or are mentioned on appear in the inbox. Act on them with
the GitHub action group (`g` leader); its which-key popup shows every continuation.

GitHub's rate budget belongs to the token, so the default token is shared by
lazybox, interactive `gh`, and spawned agents. Set `LAZYBOX_GITHUB_TOKEN` when
starting lazybox to give its daemon a dedicated token; `gh` does not consume
that lazybox-specific variable.

### How it works (brief)
`GhClient` (`crates/gh-provider/src/client.rs`) fetches PRs and issues via
GraphQL; the polling loop itself lives daemon-side in
`crates/server/src/polling/mod.rs`, which schedules repos round-robin
(stalest-first), diffs against the previous snapshot, and emits fine-grained
events (state change, CI change, review change, new activity). CI is a
`CiStatus` rolled up from individual `CheckRun`s. `GhClient` also implements
`ScopeSource` for the setup flow. Credential chain:
`LAZYBOX_GITHUB_TOKEN` env → `GH_TOKEN` env → `GITHUB_TOKEN` env →
`gh auth token` (`crates/gh-provider/src/lib.rs`).

### Test checklist
- [ ] PRs and issues both appear, with correct state (open/draft/merged/closed).
- [ ] CI status reflects the rolled-up check state (success/failure/mixed/pending).
- [ ] An issue and a PR that closes it (`Closes #N`) collapse appropriately.
- [ ] `g m` merges a green, approved, conflict-free PR.
- [ ] `g r` / `g a` / `g l` mutate reviewers / assignees / labels and the change reflects on next poll.
- [ ] `g o` opens the PR/issue in the browser.
- [ ] With no GitHub token environment variable, creds fall back to `gh auth token`.
- [ ] `LAZYBOX_GITHUB_TOKEN` takes precedence over the shared `gh` token.

### Known sharp edges
- `mergeable: UNKNOWN` from GitHub triggers a fast re-poll (~5s) to resolve; brief flicker is expected.
- A single very large scope can pressure the rate budget (see [polling sharp edges](inbox-and-sync.md#provider-polling--sync-loop)).

---

## Linear provider

**Status:** beta
**Crate(s):** `linear-provider`
**Config / flags:** `LINEAR_API_KEY` env
**Key bindings:** —

### What it does
Surfaces Linear issues as inbox rows alongside GitHub, using the same row model,
filters, and search.

### How to use it
Set `LINEAR_API_KEY` in the daemon's environment, enable the Linear provider in
the setup wizard, and pick projects to scope. Issues that aren't completed or
canceled appear in the inbox.

### How it works (brief)
`LinearClient` (`crates/linear-provider/src/lib.rs`) POSTs GraphQL to
`https://api.linear.app/graphql` with the API key in the `Authorization` header
(no `Bearer` prefix — Linear's convention). `fetch_all()` pages (50/page, up to
20 pages), filters out completed/canceled server-side, and converts to `Task`.
Errors map to `ProviderError` (auth → `Auth`, timeout/5xx/rate-limit →
`Retryable`).

### Test checklist
- [ ] With `LINEAR_API_KEY` set, Linear issues appear in the inbox.
- [ ] Completed/canceled issues are excluded.
- [ ] A bad key surfaces an auth error in the sync-status window, not a crash.
- [ ] Paging fetches more than 50 issues when present.

### Known sharp edges
- Issue *fetching* is implemented; provider mutations (comment/reply, state changes) are not fully wired — verify before relying on them.
- Linear tickets without a linked branch open to activity only (no worktree).

---

## Slack mirror

**Status:** beta
**Crate(s):** `slack-provider`
**Config / flags:** `slack.bot_token` (`xoxb-…` / `$SLACK_BOT_TOKEN`), `slack.app_token` (`xapp-…` / `$SLACK_APP_TOKEN`), `slack.anchor_channel`, `slack.channel_prefix`, `slack.per_workspace_channels`
**Key bindings:** —

### What it does
Optionally mirrors workspace activity into per-workspace Slack channels and
routes `@lazybox`-mentioned replies in those channels back into the running
agent's stdin — so you can watch and nudge long-running Claude/Codex sessions
from your phone.

### How to use it
Create a Slack app from the manifest in [`docs/slack-setup.md`](../slack-setup.md),
set `slack.bot_token` and `slack.app_token` in config (or the env vars), and
enable per-workspace channels. lazybox creates a channel per workspace; mention
`@lazybox` in one to send text to that workspace's agent. Smoke tests:
[`docs/dev/slack-testing.md`](../dev/slack-testing.md) and
[`docs/dev/bidirectional-slack-testing.md`](../dev/bidirectional-slack-testing.md).

### How it works (brief)
Outbound: workspace/agent events → `chat.postMessage` via the HTTP client
(`crates/slack-provider/src/api.rs`). Inbound: a Socket Mode WebSocket
(`crates/slack-provider/src/socket.rs`) opened via `apps.connections.open`
streams `app_mention` / `message.*` events; the chat router
(`crates/server/src/chat.rs`) either answers a status query (`status` /
`state` / `ls` / `list`) or forwards the text verbatim into the terminal
routed to that channel. Inbound Slack can **not** spawn an agent — a channel
with no routed terminal is ignored. Channels are named from the slugified
workspace key with an optional prefix.

### Test checklist
- [ ] With both tokens set, `auth.test` succeeds and the bot name shows in logs.
- [ ] Enabling `per_workspace_channels` creates a channel per workspace.
- [ ] Workspace activity is mirrored into its channel.
- [ ] An `@lazybox` mention in a workspace channel reaches that workspace's agent stdin.
- [ ] Socket Mode reconnects after a `disconnect` frame.

### Known sharp edges
- Requires Socket Mode (the `xapp-…` app token), not just a bot token.
- Bidirectional routing is newer; exercise both directions with the dev smoke tests before depending on it.
