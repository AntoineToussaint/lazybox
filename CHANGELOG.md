# Changelog

All notable user-facing changes to lazybox are documented here. The project
uses [Semantic Versioning](https://semver.org/) while pre-1.0 releases may still
contain explicitly documented compatibility changes.

## [Unreleased]

- A hot-target batch the server rejects no longer fails the whole poll tick.
  GitHub Enterprise Server 3.18 answers lazybox's batched `nodes(ids:)` hot
  queries with "Something went wrong while executing your query" (a PR node
  lookup selecting both `mergeStateStatus` and `reviewDecision` trips it); the
  targeted refresh now falls back to per-target fetches, and after three
  consecutive rejections skips the batch until the next Shift-R refresh.

## [0.1.14] - 2026-09-01

The cost-visibility, scale, and provider-reach release. Lazybox now *prices*
the agents it runs — per session and per Space — and surfaces usage as a
day/week deep-dive, so a fleet is legible instead of a mystery bill. It reaches
further, too: GitHub Enterprise Server and a read-only Jira provider join the
inbox. Triage scales to many repos through an attention ladder, search
qualifiers, and saved views, and the liveness work continues — agents resume
through auth expiry and context resets, and spawns never dead-end.

### Highlights

- **LLM cost metering.** Per-session cost is priced, attributed, and persisted
  behind a safe canary opt-in; Space-tier metering meters a whole Space rather
  than one workspace at a time; and the stale rate card that metered Opus at 3×
  is corrected. Coverage and persistence gaps from the initial rollout are
  closed.
- **Usage stats.** A usage-stats accumulator drives a day/week view, an
  always-visible "today" header strip, and a sectioned day/week deep-dive — so
  spend and activity are visible at a glance and on demand.
- **Provider reach: GitHub Enterprise Server + Jira.** GHES-hosted repos work
  alongside github.com, and a new read-only Jira provider brings Jira issues
  into the inbox next to GitHub and Linear.
- **Scale & triage for many repos.** Phase 1 many-repo relief (perf + triage)
  lands, followed by Phase 2/2b: an attention ladder, search qualifiers, saved
  views, and snooze-until-event — so a large roster stays navigable instead of
  overwhelming.
- **`lazybox log`.** Stream any command's output into its own live lazybox
  window — a build, a test run, a tailable log — instead of it scrolling past
  inside an agent. The launcher is put on the agent's PATH so `lazybox log`
  resolves from spawned sessions.
- **Hopper.** History and batch actions come to the Hopper, with a confirmed
  destructive-delete and reliable cancel/delete chords.
- **Agent liveness.** Auto-wait now resumes interrupted work through an auth
  expiry and on a context reset (with a calm `AwaitingReset` status); a
  starvation-proof global sweep promotes agents stuck at Working; and a spawn
  never gets left stuck "spawning" forever.

### Also

- Reattach replay is parsed at the real pane width, ending duplicated
  scrollback on reconnect; a per-repo interactive-spawn fast lane lets a user
  spawn jump queued poll sweeps; the server retries stalled PTY enqueues so a
  CPU spike can't drop injected input; and session recovery runs in the
  background so a slow startup can't orphan live tmux sessions.
- `g m` re-checks PR status on GitHub before routing conflict/CI; `g s` sync
  discovers a repo's new issues, not just the focused item; PRs re-fetch on a
  targeted path after merge/update-branch; and `author:@me` is probed to
  surface self/agent-created PRs fast.
- GitHub pressure is reduced: working-claims sync no longer storms the rate
  budget, deferred discovery at scale is surfaced and unblocked, and
  weekly-limit / rate-limit-option blocks are detected.
- A shared-login re-auth no longer signs out every session; autonomous
  skip-permissions is gated on trigger trust; and inject/snippet delivery to a
  dead terminal surfaces a rejection instead of silently dropping.
- Archive and removal are more truthful: `x x` archive no longer resurrects
  merged-PR sessions, archived/deleted workspaces don't resurrect on restart, a
  resurrection backstop covers non-archiving removals, and the archived
  set/tombstone are re-checked under the upsert lock.
- Agent resume and prompt history re-key to the stable workspace key so they
  survive respawn; the optimistic Working flip no longer self-confirms submits;
  `x n` recovers from a branch/namespace conflict instead of dead-ending; and
  desktop-notification bursts on focus loss are coalesced.
- PR-lifecycle shortcuts, a worktree `D`/`F` fix, silent agent auto-update, a
  `designissues` cross-repo snippet, and the github/workspace leader popup gated
  by PR-vs-Issue.
- Dependency bumps (web + rust groups), refreshed docs/website and
  high-resolution demo hero, resource-awareness prompting guidance, and CI no
  longer hard-blocks on the cargo-deny yanked check.

### Install

brew tap AntoineToussaint/lazybox && brew trust AntoineToussaint/lazybox && brew install lazybox

## [0.1.13] - 2026-08-24

The rate-limit, liveness, and multi-agent release. GitHub API pressure is now
*paced* rather than refused, the UI never blocks on the fleet, agents recover
and multiply instead of dead-ending, and stale modals / footer errors stop
lingering. It also continues the state-machine cleanup so whole classes of bug
resolve at their single owner rather than through divergent paths.

### Highlights

- **GitHub rate limiting is paced, never refused.** The rate governor paces
  requests and parks per-source instead of imposing polling blackouts; a
  two-tier hot refresh probes freshness and fetches detail only on change;
  heartbeat backoff sweeps once then hot-only; interactive targeted syncs and
  secondary-cooldown handling keep you unblocked; the working-claim label REST
  burst is paced; and a truncated 2xx GraphQL body is retried instead of failing
  the whole sync.
- **Nothing is refused for capacity.** Commands queue and always deliver; the
  keystroke path is off the global-lock queue; agent spawns run at reduced
  scheduling priority so the fleet never starves the UI; agent input is never
  destroyed by backpressure; and capacity limits warn honestly instead of
  dropping work.
- **Agents recover and multiply.** Recover an agent from exhausted credit;
  `a r` resets an agent's context in place; the explicit `a c` / `a x` / `a u`
  keys start a NEW agent beside an idle one (no more one-agent-per-workspace)
  while still collapsing genuine spawn races and issue→PR handoffs; fleet claim
  ownership is crash-safe; and sessions whose PR/issue closed are reaped.
- **Stale modals and footer errors stop lingering.** Footer errors auto-fade
  instead of sitting forever (only the daemon-disconnect / build-mismatch
  banners stay); orphaned per-workspace modals dismiss when their workspace is
  removed; the merged/closed cleanup prompt respects the in-flight removal owner
  so it can't re-prompt for a workspace on its way out; every confirm defaults
  to Yes with destructive ones warning-colored; and footer alerts fade honestly
  (a re-fire counts, it doesn't reset the clock).
- **Organization: Spaces, Hopper, focus layouts.** Persistent Hopper
  workspaces; rename a Space from its header; reorder repos and Spaces in place;
  a move-to-Space picker with last-used preselection; and focus-mode two-up
  splits and a 2×2 grid over the starred roster.
- **Snippets.** SOTA review discipline with readable, details-first output;
  provenance-aware overrides with badges, compare, keep-mine, and adopt; and the
  snippet MRU is recorded when a snippet *starts* a session, not only when
  injected.
- **State-machine foundations.** Snippet delivery is encapsulated in a
  state-owning type with an honest `]N` count; footer notices route through a
  typed `NoticeKey` + one resolve primitive; the last prompt's age shows on the
  `you ▸` recap; and terminal replay/resync continue to converge on single
  owners.
- **Desktop.** Visual redesign and feature parity with the TUI; the desktop
  attaches to a running daemon instead of refusing to start.

### Also

- Agent spawns auto-switch a clean drifted worktree back to the session branch;
  shells open branch-agnostically on drifted worktrees; bare clones carry the
  fetch refspec and `unpushed()` checks the branch's own remote.
- `ui.*` settings survive restarts, attach clients, and concurrent writers;
  queued keystroke saves flush on quit.
- `g m` on an already-merged PR is a no-op success (it asks GitHub; cached soft
  state advises, never refuses); `g s` on a repo-scoped workspace discovers the
  repo's open issues + PRs; `x k` closes/deletes upstream and archives in one
  chord; hand work to a stronger model for review (headless model tiers + Critic
  escalation).
- Autonomous spawns no longer disable the user's MCP servers; typing latency
  from flow-control overflow is fixed; the selection highlight is clipped to its
  anchor tile; multi-select is clearable everywhere and bulk actions fan out
  over it.
- An outdated-build guard opens a dismissable update modal when a newer
  dev/release build exists, without ever self-updating.
- A `working` filter (`f` → working) narrows the sidebar to workspaces whose
  agent is *live-working*, keyed off the runtime agent state rather than the
  recorded-session `with-agent` predicate (which read 0 while agents ran).
- Sync robustness: watched-repo issues surface, `g s` discovers a Linear
  workspace's tickets, and a truncated GraphQL body is retried with a capped
  log line instead of failing the whole sweep.

### Install

brew tap AntoineToussaint/lazybox && brew trust AntoineToussaint/lazybox && brew install lazybox

## [0.1.11] - 2026-08-17

The data-safe reliability release. Workspace creation now has an explicit,
request-correlated outcome everywhere: success names the created workspace and
failures remain visible in both the TUI and desktop instead of disappearing.
Cleanup fails closed when local work or worktree state cannot be verified, rapid
multi-workspace prompt fan-out is ordered and bounded, and terminal recovery
stays coherent under catch-up bursts. This release also consolidates workspace,
terminal, agent-turn, output, and flow-control ownership so the same bug classes
cannot keep returning through divergent paths.

This supersedes the tagged but never-published 0.1.10, so it also carries all
changes documented under 0.1.10 below.

### Highlights

- **No silent workspace creation.** Every create request is correlated to a
  success or actionable failure, pending UI state is cleared on send failures,
  desktop calls reject daemon failures, and concurrent creates allocate distinct
  durable keys.
- **Worktree deletion is fail-closed.** Cleanup is blocked by uncommitted or
  unpushed work and by any failed store or Git-status probe; project cascades use
  the same guarded lifecycle instead of bypassing it.
- **The UI stays live under load.** Off-thread rendering owns terminal output,
  daemon bursts yield to newly arrived input, and bounded flow control reserves
  enough capacity for a complete authoritative resync.
- **Terminal recovery is deterministic.** Replay and resync share one terminal
  authority, rapid prompt fan-out is settle-gated per terminal, scrolling no
  longer duplicates wrapped lines, and crash/abort paths restore a usable shell.
- **Agent state is truthful.** Spawn feedback survives focus changes, background
  shells keep an agent working, Claude unattended trust is seeded, and asking /
  working / done transitions come from one turn-state authority.
- **Operational cleanup.** Test children are reaped as process groups, routine
  disconnects stay quiet while faults remain loud, and slow worktree reclamation
  runs outside the sync critical path.
- **Usage and providers.** Live Claude and Codex plan usage appears in the
  header; the sandbox has SDK-native GCP lifecycle and typed reauth; Linear has
  comment threads plus assign/close mutations.

### Install

brew tap AntoineToussaint/lazybox && brew trust AntoineToussaint/lazybox && brew install lazybox
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/AntoineToussaint/lazybox/releases/download/v0.1.11/lazybox-tui-installer.sh | sh

## [0.1.10] - 2026-08-12

A reliability and responsiveness patch on top of 0.1.9. Highlights: the UI no
longer freezes during heavy GitHub sync (input stays live under poll bursts and
on wake-from-sleep), multi-select actions fan out to every selected workspace,
and merge failures are humanized with actionable next steps. Linear support is
steadier — tighter scope, saner sync cadence, description rendering, and
`w w` repo routing. The desktop client gets a two-column layout and stops
auto-resizing, and CI/build hygiene improves. Numerous smaller inbox, snippet,
and terminal-rendering fixes round it out.

## [0.1.9] - 2026-07-30

This release makes lazybox a steadier control surface for long-running agent
fleets: prompts, terminals, and agent run state survive restarts more
faithfully, worktrees are safer to inspect, reclaim, and adopt, and the inbox
gains focused tools for handing off, filtering, syncing, and updating work. It
also sharpens everyday reliability — a PTY watchdog, hardened sync boundaries,
a keyboard URL picker, and clearer, self-clearing error toasts.

### Install

Homebrew (macOS or Linux):

```sh
brew tap AntoineToussaint/lazybox && brew trust AntoineToussaint/lazybox && brew install lazybox
```

Shell installer (macOS arm64/x86_64 or Linux x86_64):

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/AntoineToussaint/lazybox/releases/download/v0.1.9/lazybox-tui-installer.sh | sh
```

Then run `gh auth login` if needed and launch `lazybox`.

### Added

- `lazybox worktree list` and `lazybox worktree gc` report managed worktrees
  with their disk totals and reclaim the orphaned ones left behind by
  interrupted or crashed provisioning (`--dry-run` / `--force`) (#574).
- A dismissable startup notice detects newer source commits or published
  releases, shows a checkout- and install-channel-specific manual update
  command, and stays quiet for a previously dismissed target.
- Agent-to-agent handoff (`x s`): capture a running agent's output, edit the
  brief, and inject it into another workspace's session (#431).
- Agent-authored session conversion: fork a running session into a new one
  seeded with a role prompt — continue the work or critique it — drawn from
  the source agent's own output (#649).
- Update branch: `g u` merges the base into a behind PR's head, and `Shift-U`
  fans it out over the sidebar multi-select (#484).
- A multi-select filter menu on `f` combining state, role, and kind predicates
  with match counts and removable header chips (#443).
- Per-workspace notes (`n`) (#458).
- A scrollable markdown reader modal for long or richly formatted PR/issue
  bodies, opened by a second `d` on the description preview (#448).
- In-place import of externally created checkouts (`x i`, backed by
  `ScanCheckouts`/`CheckoutsDiscovered`), completing the `lazybox scan`
  discovery flow (#452).
- A keyboard URL picker (`]]u`): scan the visible agent terminal for links and
  open the chosen one in the browser — emulator-independent, and soft-wrapped
  URLs are stitched back so a click on any row of a wrapped link resolves the
  whole URL (#596).
- Adoption of externally created companion PR worktrees, so a checkout made for
  an existing PR outside lazybox attaches to its workspace instead of
  provisioning a duplicate (#617).
- A per-session prompt-history picker (`]]h`): every prompt sent to the agent,
  persisted across restarts, re-sendable with Enter (#523).
- A track-main arm keeping scratch workspaces tied to the repo's default
  branch (#535).
- Per-workspace sync (`g s`): a targeted re-poll of just the focused
  workspace's PR/issue instead of the global sweep (#456).
- Activity-pane cycling (`Shift-P`): full → one-line summary → hidden,
  remembered per workspace with a `ui.activity_pane_default` starting mode
  (#487).

### Changed

- Engaged GitHub workspaces stay prominent, while multi-axis filters make it
  practical to narrow a busy inbox by state, role, and task kind.
- The GitHub PR sweep force-includes every repo backing a workspace with a
  persisted session — any session kind, live or not — and always fetches the
  focused repo, so a refresh no longer starves scoped repos or skips the PR
  you're on (#585).
- Contextual `w w` launches, explicit agent launches, and priority/model-tier
  selection now resolve consistently and keep their chosen model visible.
- Terminal prompt history, scrollback, and reconnect replay now share the same
  persisted lifecycle, including restored sessions after a restart.
- Action-failure toasts (merge, close, update, delete) lead with the reason
  instead of the `owner/repo#NNN` label and fade on their own rather than
  pinning the footer until dismissed (#588).
- Clicking a terminal tile focuses it, the clickable workspace title carries a
  visible ↗ affordance, and the current-workspace marker is easier to spot
  (#599, #590, #610).
- Tour hints are derived from the action catalog so they track the live keymap
  (#602).
- GitHub rate-limit waits are surfaced: the poller reports the wait and backs
  off until the limit resets instead of erroring against a throttled API
  (#678).
- Ask Lazybox gains a thinking spinner, a follow-up-versus-new-question
  distinction, and a stickier conversation loop (#643).
- The editor launcher (`e`) detects more macOS GUI editors (#676), and the
  terminal footer hint bar is decluttered to the keys that matter in context
  (#665).
- The docs, install paths, support forms, provider setup, and mobile homepage
  have been audited against the current CLI, config schema, and action catalog.

### Fixed

- Shell sessions now honor `shell.command` from `~/.lazybox/config.yaml`, and
  plain shells default to the account's OS login shell instead of a hard-coded
  `bash` (#598).
- Workspace deletion, merge, close, and garbage collection preserve dirty or
  unpushed work while reclaiming safe orphaned worktrees (#573, #575).
- Agent run state persists across daemon restarts, so restored sessions return
  in their real state instead of resetting (#630).
- A PTY watchdog deadline keeps a wedged terminal from hanging its session
  (#628), unsent composer drafts survive session transitions (#624), and
  trailing blank rows are trimmed from the raw-PTY scrollback seed (#589).
- Worktree-provisioning failures that surface only as a spawn error now route
  to the recovery modal with actionable text instead of an elided footer line
  (#594).
- Rendering and partial-sync boundaries are hardened against malformed or
  partial provider updates (#603).
- Issue-to-PR joins retain session ownership and surface the originating issue
  without duplicating or losing workspace state; a stale issue→PR collapse is
  released when the PR no longer closes the issue (#581).
- Terminal exits, failed starts, reconnects, mouse URL routing, and status
  transitions remain visible and ordered instead of leaving stale or
  disappearing tabs (#631).
- Provider polling and persistence paths avoid duplicate Slack delivery, stale
  GitHub state, and silent loss of malformed stored records; hyphenated GitHub
  project labels parse correctly (#638).
- All agent badges render in the sidebar (#621); tour cards stay within the
  modal height and Ask Lazybox help lists actions in the correct order (#600,
  #601).
- Clicking a desktop notification focuses the workspace that triggered it
  (#674).
- Stale branch holders are reclaimed, so a branch pinned by a defunct session
  can be freed and reused (#652).
- Terminal recovery is hardened against file-descriptor exhaustion, so a burst
  of spawns can't wedge the daemon (#653).
- Release binaries no longer inherit a false `-dirty` version suffix from
  cargo-dist's generated manifest output, and the pre-split shell-installer URL
  remains valid.

## [0.1.7] - 2026-07-19

### Added

- A persistent, pinned Ghostty/Zig source cache and a strictly offline
  `make release` path after one online `make setup`.
- Release-artifact and TUI startup smoke tests, Lighthouse/size budgets,
  Criterion regression tracking, security reporting guidance, state-recovery
  documentation, and automated dependency updates.
- A grouped `x` workspace menu for creating workspaces/projects, adopting or
  joining sessions, long snooze, archive, and issue close actions.
- Exact Rust 1.88 MSRV verification in CI and a pinned stable contributor
  toolchain.
- An accessible custom 404 page and resilient no-JavaScript homepage content.
- Ask Lazybox as the primary `?` surface, with live effective-keymap search,
  conversational workflow help, and searchable terminal-leader commands.
- `lazybox scan` for read-only discovery of externally created checkouts
  (in-place import shipped later — see the Unreleased `x i` entry).
- A unified policies menu for per-PR and per-issue automation controls, plus a
  configurable tabs-first layout for newly opened terminals.
- Explicit terminal `Exited` state so clean exits, crashes, and agents that
  fail during startup remain distinguishable.

### Changed

- The HTTP command endpoint now waits for command handling to finish, enforces
  body/time/connection limits, and requires an explicit opt-in for plaintext
  non-loopback binding.
- The `lb` command is a small exec alias instead of a second full TUI binary.
- Default contextual work is now the deterministic `w w` chord; the previous
  timed single-`w` fallback and its 600 ms delay were removed.
- The workspace uses Rust edition 2024 and the newest dependency releases that
  preserve the Rust 1.88 compiler floor.
- IPC serialization now uses bincode 2 with explicit frame limits and
  trailing-byte rejection. Older daemon/client pairs must be upgraded
  together.
- Workspace-management operations are grouped under `x`; consult the generated
  keybinding reference or Ask Lazybox for the complete mapping.
- The shortcut system now follows five named menus (`w` work, `a` agent, `b`
  main branch, `g` GitHub, `x` workspace), with a compact secondary index and
  `g r` as the mnemonic reviewer chord (replacing `g v`).
- Terminal scrolling, replay recovery, and input now pass through one owner
  with sequenced resynchronization and isolated ordered command lanes.
- IPC protocol version 11 adds bounded admission and explicit lifecycle
  contracts; daemon and client must be upgraded together.

### Fixed

- Unreadable persisted records are preserved and surfaced through the TUI
  event stream and JSON API instead of silently disappearing.
- Production shared-state locks no longer poison after a thread panic, and
  process-spawn logs no longer print raw argument values.
- Unknown or stale workspace spawn requests can no longer launch an agent in
  the daemon's current directory.
- Failure to open persistent state is now fatal and visible instead of silently
  falling back to an empty in-memory database.
- Slack setup reads bot and app tokens with terminal echo disabled.
- `terminal.escape_char` is honored and legacy configuration remains readable.
- Terminal help now documents the real `]]q` exit and `]]s<key>` snippet
  chords, including when the terminal escape character is remapped.
- Homepage contrast, accessible navigation names, reduced-motion behavior,
  video fallback loading, and no-JavaScript rendering.
- Mouse-wheel input now scrolls the terminal tile under the pointer, including
  local scrollback on the primary screen, without fighting the focused tile.
- Terminal output survives detach/reconnect gaps without stale cursors,
  duplicate bytes, lost tails, or unbounded replay loops.
- Agent crashes and immediate startup failures remain visible, while cleanly
  exited agent terminals close automatically without deleting the workspace.
- The last agent prompt survives restart, `InputNeeded` remains sticky across
  ambiguous scrapes, long Ask Lazybox input wraps, and which-key menus support
  arrow and `j`/`k` navigation.
- Workspace persistence is atomic and truthful, workspace/terminal moves are
  serialized, and failed commits can no longer leave memory and SQLite out of
  sync.
- IPC connections, background tasks, terminal writes, and command queues now
  have explicit capacity and shutdown bounds instead of growing or hanging
  indefinitely under load.

[Unreleased]: https://github.com/AntoineToussaint/lazybox/compare/v0.1.14...HEAD
[0.1.14]: https://github.com/AntoineToussaint/lazybox/compare/v0.1.13...v0.1.14
[0.1.13]: https://github.com/AntoineToussaint/lazybox/compare/v0.1.11...v0.1.13
[0.1.11]: https://github.com/AntoineToussaint/lazybox/compare/v0.1.10...v0.1.11
[0.1.10]: https://github.com/AntoineToussaint/lazybox/compare/v0.1.9...v0.1.10
[0.1.9]: https://github.com/AntoineToussaint/lazybox/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/AntoineToussaint/lazybox/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/AntoineToussaint/lazybox/compare/v0.1.6...v0.1.7
