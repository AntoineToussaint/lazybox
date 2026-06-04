# lazybox feature catalog

A dev-side inventory of **everything lazybox currently does**, organized by
domain. Each entry is a short, consistent page that doubles as a
**hand-testing checklist** — the source material for focused, feature-by-feature
review and for dev onboarding.

This is deliberately an *inventory + audit*, not polished user docs. For the
*why* behind the architecture, read [`DESIGN.md`](../../DESIGN.md); for
day-to-day conventions, [`CLAUDE.md`](../../CLAUDE.md); for execution status of
the bigger bets, [`ROADMAP.md`](../../ROADMAP.md). For the user-facing
onboarding effort this catalog feeds, see issue #165.

## How to read a page

Every feature is documented as a section with the same shape:

```md
# <Feature>

**Status:** stable | beta | experimental | scaffolded-not-active
**Crate(s):** <where it lives>
**Config / flags:** <relevant config.yaml keys, CLI flags, env vars>
**Key bindings:** <if any>

## What it does
## How to use it
## How it works (brief)
## Test checklist
## Known sharp edges
```

**Status legend**

| Status | Meaning |
|---|---|
| `stable` | Works and is exercised daily; covered by tests. |
| `beta` | Works end-to-end but under-tested or rough in places. |
| `experimental` | Wired and usable, but the surface is still moving. |
| `scaffolded-not-active` | Code/infrastructure exists but the path isn't live yet. |

## Domains

| Page | Covers |
|---|---|
| [Inbox & sync](inbox-and-sync.md) | The reactive inbox, polling loop, refresh, sync status, filters, sort, read/unread, snooze |
| [Providers](providers.md) | `TaskProvider`/`ScopeSource` traits, GitHub, Linear, Slack mirror |
| [Workspaces & worktrees](workspaces-and-worktrees.md) | Workspace model, worktree manager, new/merge/archive/adopt/collapse, editors, per-repo overrides, persistence |
| [Terminals & agents](terminals-and-agents.md) | Embedded terminal, agent spawn, Work command, autonomous sessions, state detection, structured runtime, LLM proxy, snippets |
| [TUI & UX](tui-and-ux.md) | Three-pane layout, key/chord system, help, settings wizard, activity feed, reply, mouse, modals, notifications |
| [Daemon, deployment & build](daemon-and-deployment.md) | Client/daemon split, standalone daemon, remote connect, JSON API, run modes, auth chain, config reference, build |

## Feature index

Status is best-effort as of this catalog's first pass; correct it as you audit.

### Inbox & sync
| Feature | Status | Crate(s) |
|---|---|---|
| [Reactive PR/issue inbox](inbox-and-sync.md#reactive-inbox) | stable | `tui`, `server`, `core` |
| [Provider polling / sync loop](inbox-and-sync.md#provider-polling--sync-loop) | stable | `server` |
| [Manual refresh](inbox-and-sync.md#manual-refresh) | stable | `server`, `tui` |
| [Sync-status window](inbox-and-sync.md#sync-status-window) | stable | `tui`, `server` |
| [Role filter](inbox-and-sync.md#role-filter) | stable | `tui` |
| [Sort order](inbox-and-sync.md#sort-order) | stable | `tui` |
| [Search](inbox-and-sync.md#search) | stable | `tui` |
| [Mailbox cycle](inbox-and-sync.md#mailbox-cycle) | stable | `tui` |
| [Read/unread tracking](inbox-and-sync.md#readunread-tracking) | stable | `tui`, `store` |
| [Snooze](inbox-and-sync.md#snooze) | stable | `tui`, `store` |

### Providers
| Feature | Status | Crate(s) |
|---|---|---|
| [Provider + Scope traits](providers.md#provider--scope-traits) | stable | `core` |
| [GitHub provider](providers.md#github-provider) | stable | `gh-provider` |
| [Linear provider](providers.md#linear-provider) | beta | `linear-provider` |
| [Slack mirror](providers.md#slack-mirror) | beta | `slack-provider` |

### Workspaces & worktrees
| Feature | Status | Crate(s) |
|---|---|---|
| [Workspace model & lifecycle](workspaces-and-worktrees.md#workspace-model--lifecycle) | stable | `core`, `server` |
| [Worktree manager](workspaces-and-worktrees.md#worktree-manager) | stable | `git-ops` |
| [New pre-PR workspace](workspaces-and-worktrees.md#new-pre-pr-workspace) | stable | `tui`, `git-ops` |
| [New project](workspaces-and-worktrees.md#new-project) | beta | `tui`, `core` |
| [Editor integration](workspaces-and-worktrees.md#editor-integration) | stable | `config`, `tui` |
| [Per-repo overrides](workspaces-and-worktrees.md#per-repo-overrides) | stable | `config`, `git-ops` |
| [Merge PR](workspaces-and-worktrees.md#merge-pr) | stable | `tui`, `gh-provider` |
| [Archive workspace](workspaces-and-worktrees.md#archive-workspace) | stable | `tui`, `git-ops` |
| [Adopt sessions](workspaces-and-worktrees.md#adopt-sessions) | beta | `tui` |
| [Collapse into PR](workspaces-and-worktrees.md#collapse-into-pr) | beta | `tui` |
| [State persistence](workspaces-and-worktrees.md#state-persistence) | stable | `store` |

### Terminals & agents
| Feature | Status | Crate(s) |
|---|---|---|
| [Embedded terminal](terminals-and-agents.md#embedded-terminal) | stable | `tui-term`, `libghostty-vt` |
| [Spawn shell & agents](terminals-and-agents.md#spawn-shell--agents) | stable | `agents`, `server` |
| ["Work" command](terminals-and-agents.md#work-command) | stable | `tui-core`, `core` |
| [Autonomous sessions](terminals-and-agents.md#autonomous-sessions) | beta | `server`, `agents` |
| [Agent state detection](terminals-and-agents.md#agent-state-detection) | beta | `agents` |
| [Structured agent runtime](terminals-and-agents.md#structured-agent-runtime) | experimental | `server`, `ipc` |
| [LLM proxy](terminals-and-agents.md#llm-proxy) | experimental | `llm-proxy`, `server` |
| [Snippets](terminals-and-agents.md#snippets) | stable | `config`, `tui` |
| [Terminal interaction model](terminals-and-agents.md#terminal-interaction-model) | stable | `tui` |

### TUI & UX
| Feature | Status | Crate(s) |
|---|---|---|
| [Three-pane layout & focus](tui-and-ux.md#three-pane-layout--focus) | stable | `tui` |
| [Key-binding system & chords](tui-and-ux.md#key-binding-system--chords) | stable | `tui-core`, `tui` |
| [Help overlay](tui-and-ux.md#help-overlay) | stable | `tui` |
| [Settings palette & setup wizard](tui-and-ux.md#settings-palette--setup-wizard) | stable | `tui`, `config` |
| [Guided tour](tui-and-ux.md#guided-tour) | beta | `tui` |
| [Activity feed](tui-and-ux.md#activity-feed) | stable | `tui` |
| [Reply](tui-and-ux.md#reply) | stable | `tui`, providers |
| [Mouse handling](tui-and-ux.md#mouse-handling) | stable | `tui` |
| [Pickers / modals](tui-and-ux.md#pickers--modals) | stable | `tui` |
| [Desktop notifications](tui-and-ux.md#desktop-notifications) | stable | `tui-core`, `tui` |

### Daemon, deployment & build
| Feature | Status | Crate(s) |
|---|---|---|
| [Client/daemon split](daemon-and-deployment.md#clientdaemon-split) | stable | `server`, `ipc`, `tui` |
| [Standalone daemon](daemon-and-deployment.md#standalone-daemon) | stable | `tui`, `server` |
| [Remote connect](daemon-and-deployment.md#remote-connect) | beta | `ipc`, `tui` |
| [JSON HTTP API gateway](daemon-and-deployment.md#json-http-api-gateway) | experimental | `server` |
| [Run modes / flags](daemon-and-deployment.md#run-modes--flags) | stable | `tui`, `core` |
| [Auth / credential chain](daemon-and-deployment.md#auth--credential-chain) | stable | `auth`, providers |
| [Config reference](daemon-and-deployment.md#config-reference) | stable | `config` |
| [Build & install](daemon-and-deployment.md#build--install) | stable / scaffolded | build scripts |

## Auditing this catalog

When you fill in or verify a page's **Test checklist**, file follow-up issues
for any behavior that turns out to be broken or unverified — that's the
"focused work on testing each feature" this catalog is meant to drive. Keep
status labels honest: downgrade a `stable` to `beta` the moment a checklist
item fails to hold.
