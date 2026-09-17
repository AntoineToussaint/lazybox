# AGENTS.md

Guidance for any coding agent working in this repository (Claude Code, Codex,
Cursor, …). This file is the canonical source; `CLAUDE.md` points here.

Depth lives closer to the code: each area carries its own `AGENTS.md` and the
one nearest the file you are editing wins, like `.gitignore`. Procedures live
as skills under `.agents/skills/` and load only when you need them. The index
is at the bottom.

## What is lazybox?

A reactive PR inbox TUI. Instead of checking GitHub, events flow to you — new
comments, CI failures, review requests surface automatically with read/unread
tracking. Each task becomes a session with an embedded terminal running an
agent or a shell in a git worktree. Source-agnostic: GitHub is one provider;
Linear, Jira and Slack plug in the same way.

## How to behave

These rules outrank convenience, and they outrank finishing fast.

**A gap in the tooling is a bug in the tooling — never a reason to reach
around it.** If `lazybox`, the `Makefile`, a script or a crate API cannot
express what you need, the deliverable is the missing capability or a precise
issue asking for it. Not as a "workaround", not "just this once", not "until
the real fix lands".

**Never hack. Always provide the best fix, even when it spans repos.** The
right fix living in someone else's repository is not a reason to work around
it in this one — open the PR there. If it genuinely cannot be fixed now, ship
a precise issue against the owner *plus* an explicitly-labelled stopgap. Never
an unlabelled one.

**Classify every change that makes something work**, in the PR body: a *fix*
at the place that owns the behaviour, or a *hack*. A hack does not become a
fix by working, by being small, by being local, or by the real fix belonging
somewhere else.

**Never hardcode what the system resolves.** Daemon socket and state paths
come from `lazybox_core::paths`, not string literals under `~/.lazybox`; ports
are bound, not picked; credentials come from the `lazybox-auth` chain, not
from another component's config; worktree locations come from the configured
worktree root. If you are typing such a value in, you are encoding something
true only on your machine for the next ten minutes — and it will keep working
just long enough to be believed.

**Diagnose, do not pattern-match.** "It started working when I changed X" is
not a diagnosis: set X back and confirm it breaks. Do not trust an error
message before checking it — this codebase has had a stale-build symptom
present as a wire bug, and a full disk present as a cargo compile error.

**Say what you did not verify.** Unverified is not the same as working. If you
could not exercise a path — no credentials, no second machine, a test you
skipped — the PR body says so, explicitly.

## Boundaries

**The layering is enforced, not aspirational.** `crates/core/tests/dep_rules.rs`
pins the entire internal `lazybox-*` dependency graph against an allowlist:
`core` and `auth` depend on no internal crate, `store` may depend on `core`,
provider crates stay within `core + auth`, and the UI library `tui` may reach
only `{ipc, tui-core, tui-term, config, core}` — a `use lazybox_store::…`
there is a compile error, and the daemon/provider/store wiring belongs in the
`tui-boot` binary. Adding or removing an edge fails the test until you update
the allowlist deliberately. That edit is an architectural decision; make it
one, in the PR body.

**The tracker record is the workspace.** A GitHub issue or PR, a Linear or
Jira ticket, is worked in the one workspace it already has — an issue and the
PR that closes it share that row. New work starts by filing the record
(`gh issue create --repo <owner/repo>`, under an epic `--parent <url>`), never
by opening a second workspace beside it. Named workspaces are repo-less
scratch only.

**Some GitHub labels are live coordination state**, not metadata: `working` /
`lazybox:w:…` (a running agent owns this task), `no-auto-fix` /
`do-not-lazybox` (auto-fix opt-out), and `role:<…>` (orchestration role).
Stripping one makes the fleet double-spawn or unrole a session.

**`@lazybox` in an issue or PR comment spawns an agent.** You post as the
lazybox user, so never write that literal unless you mean to start one.

## Build, run, test

```bash
make build                     # cargo build -p lazybox-tui-boot (pinned zig)
make run                       # build and run; `gh auth token` is picked up
make test                      # cargo nextest, workspace, 10s per-test deadline
make lint                      # clippy with the workspace lint config
make pre-commit                # the full local gate: fmt + clippy + rustdoc
```

The `lazybox` binary lives in `lazybox-tui-boot`, not `lazybox-tui` (a
library-only crate). Logs go to `/tmp/lazybox.log`; state persists in
`~/.lazybox/v2/state.db`. First build compiles SQLite and the vendored
Ghostty VT — run `make setup` once.

**This box is shared with other agents.** Before you compile or test, sample
the load (`uptime` against the core count) and back off: throttle with
`CARGO_BUILD_JOBS`, or scope to the crate you touched (`cargo test -p <crate>`)
while iterating. Blindly grabbing every core when fifteen other agents do the
same is what pins the machine. Throttling changes *how hard* you compile,
never *whether* the full gate runs before you push — scoped runs miss
cross-crate gates. Details: [`docs/agent-resource-awareness.md`](docs/agent-resource-awareness.md).

Run the checks before pushing — see the `run-local-checks` skill for the gate
set CI actually enforces, which is wider than build-and-test.

## Conventions

- `thiserror` for errors in library crates, `anyhow` in binaries.
- No `unwrap()` in library crates.
- Every public function has a test. Visually complex TUI components carry
  insta render snapshots; the rest get ratatui `TestBackend` render tests.
- Every bug fix lands with a regression test that fails without the fix.
- Conventional Commits for commit messages and PR titles.
- Docs are code: the PR that changes a process updates the file that
  describes it. A human survives a stale doc; an agent reading it on every
  request is poisoned by one.

## Where things live

```
crates/
  core/ auth/ store/ config/     shared libraries — Task, Session, keys,
  git-ops/ identity/ entitlement/  credentials, SQLite store, worktrees
  gh-provider/ linear-provider/  task sources (poll → Vec<Task>)
  jira-provider/ slack-provider/
  ipc/ agents/ server/ sandbox/  daemon side — wire types, agent registry,
  relay/ e2e-channel/              PTYs, polling, API gateway, remote boxes
  tui-core/ tui/ tui-term/       client side — action catalog, realm UI,
  tui-boot/ libghostty-vt*/        embedded terminal, the binary
apps/desktop/                    Tauri shell (its own workspace and lockfile)
web/                             lazybox.ai — some pages are generated
```

## Going deeper

Nested area files — read the one next to what you are editing:

- [`crates/server/AGENTS.md`](crates/server/AGENTS.md) — the daemon: PTY
  ownership, polling tiers, merge and cost trailers, auto-merge, the
  coordination MCP server.
- [`crates/gh-provider/AGENTS.md`](crates/gh-provider/AGENTS.md) — repo-first
  discovery, role derivation, what the filters drop.
- [`crates/agents/AGENTS.md`](crates/agents/AGENTS.md) — the `Agent` trait,
  model tiers, gateway injection, the session briefing.
- [`crates/tui/AGENTS.md`](crates/tui/AGENTS.md) — realm model, panes, modals,
  and the VT/PTY size contract.
- [`crates/tui-core/AGENTS.md`](crates/tui-core/AGENTS.md) — the action
  catalog: chords, sections, guards, and the generated keymap reference.

Skills carry the procedures: `run-local-checks`, `add-a-provider`,
`add-an-agent`, `regenerate-wire-contracts`. They live in `.agents/skills/`
rather than `.claude/skills/` because that is the shared root every agent
lazybox spawns reads — `.claude/skills` is Claude Code's alone
(`crates/config/src/skills.rs`), and a repo whose guidance is agent-agnostic
should not hide its procedures behind one agent.

Design notes live in [`DESIGN.md`](DESIGN.md), [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
and [`docs/`](docs/); contributor process in [`CONTRIBUTING.md`](CONTRIBUTING.md).
