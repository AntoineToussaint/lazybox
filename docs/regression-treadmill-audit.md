# Regression-treadmill audit (#410)

Why the same bugs kept coming back: a per-subsystem audit of the four
repeat offenders — design verdict, how many mechanisms implement each
concept, which invariants hold and where they are enforced, and where
coverage validates a toy shape instead of the real path. Companion
documents: [regression-ledger.md](regression-ledger.md) maps each
historically-recurring bug to the test that now guards it (enforced by
`crates/core/tests/regression_ledger.rs`);
`crates/server/tests/e2e_real_paths.rs` is the integration tier this
audit concluded was the biggest missing lever.

The cross-cutting findings, up front:

1. **Green CI ≠ works** was structural, not accidental: every recurring
   bug had a passing test that deliberately skipped the breaking path
   (mock backend, pre-seeded worktree, `cwd:` override, skip-as-pass
   tmux gate, synthetic detector strings). The fix is a tier whose tests
   run the real subprocesses and *fail loudly* when their prerequisites
   are missing (`LAZYBOX_E2E_REQUIRE=1`), plus ledger discipline: a
   "fixed again" bug lands with a test that reproduces the real shape.
2. **Consolidation mostly already happened** — the #371 pattern (one
   owner type, typed outcomes, forbidden-edge tables) now governs
   scrolling and agent state. The remaining true divergence is
   scrollback *sourcing* (live ring vs tmux `capture-pane`, #393).
3. **The stale-build guard was dormant for the only case that ever
   bit** (a dev binary running after `git pull`, #391) — fixed in this
   change; the guard now fires for both build kinds.

---

## 1. Terminal scrollback / rendering

**Design verdict: sound at the viewport layer, split at the source
layer.** Viewport mutation was consolidated by #371 into a single owner
(`TerminalVt::scroll`, `crates/tui/src/components/terminal_stack.rs`)
returning a typed `ScrollOutcome`, so a scroll can never silently
no-op. That layer has not regressed since.

**Mechanism count: two** for the *source* of scrollback bytes, one for
storage:

- **Live path** — the attach PTY's raw byte stream, pushed into the
  per-terminal `ReplayRing` (2 MiB, `crates/server/src/pty.rs`) and
  broadcast; the client's libghostty grid (10 000-row scrollback) is
  the render surface. tmux is kept off the alt screen (`smcup@/rmcup@`)
  so scrolled-out lines land in the client grid.
- **Restart path** — after a daemon restart, `TmuxBackend` re-seeds the
  ring from `tmux capture-pane -e -S -10000`
  (`crates/server/src/backend/tmux.rs`), i.e. *rendered cells with
  re-synthesized SGR*, not the original byte stream.

**Where they diverge** (#393's finding, confirmed): escape fidelity
(OSC 8 hyperlinks survive live, are dropped by capture), soft-wrap
geometry (capture without `-J` flattens wrapped lines), and budget
units (live = 2 MiB of *bytes*; restart = 10 000 *lines*; client grid =
10 000 *rows* — three independent constants no type reconciles).

**Invariants and enforcement.** Enforced: single scroll owner + typed
outcomes (#371 tests), ring arithmetic (`pty.rs` unit tests), capture
depth == tmux `history-limit`, restart re-seed against a real tmux
(`tmux_restart.rs`). Previously unenforced, now covered: the restart
test could skip-as-pass on runners without tmux (closed —
`LAZYBOX_E2E_REQUIRE=1` fails instead), and nothing exercised
restart-recovery through the *serve loop* a TUI actually reconnects to
(closed — `e2e_serve_loop_restart_recovers_session_with_deep_scrollback`).

**Consolidation verdict — landed (PR #395, 2026-07-20):** live sessions
now read the same capture-pane history as the restart path — the first
upward scroll of a visit sends `Command::FetchScrollback` and the
client rebuilds its grid from the reply, preserving DEC modes and the
viewport's distance from the bottom. The e2e tier pins the whole wire
path against real tmux with no restart
(`e2e_live_scroll_fetch_serves_deep_history_without_restart`), and a
libghostty-level test pins the invariant that makes parking in deep
scrollback usable at all (a streamed chunk never snaps a scrolled-up
viewport to the bottom). Remaining divergence, deliberately accepted
for now: capture fidelity (OSC 8 hyperlinks, soft-wrap geometry) and
byte↔line budget parity are not equivalence-tested — depth is.

**Operational note:** #395 merged the same day this audit closed. Any
"scrolling is STILL broken" report should first check the running
binary's build (`lazybox --version` vs `git log`) — the dormant
dev-build staleness guard this PR fixes is exactly why a pre-#395
binary could run for days with zero signal.

---

## 2. Agent state machine + needs-input detection

**Design verdict: healthy — this subsystem already had its #371-style
consolidation** (#357/#374/#397/#399) and is the model the others
should follow. Five reading sources (per-chunk byte-flow, quiet-screen
classifier, Codex current-chunk fast path, Claude lifecycle hooks,
optimistic answer-flip) all commit through **one** choke point
(`AgentStateMachine::transition` behind
`transition_and_broadcast_agent_state`), with an explicit forbidden-edge
table (`Working↛Idle`, `Done↛Idle`, `Exited` absorbing) and an explicit
hooks-vs-scrape precedence gate (`pty_reading_allowed`: fresh hooks own
Working↔Idle, PTY owns `InputNeeded`, stale hooks fall back with
affirmative-evidence demotion). The TUI mirrors daemon state verbatim
and holds no transition logic.

**Mechanism count: five sources, one owner** — plural sources here are
justified (hooks don't exist for Codex; dialogs block Claude's hook
stream), because precedence is codified and the commit path is single.

**Invariants and enforcement.** The transition table and damping rules
are directly unit-tested (`crates/agents/src/state_machine.rs`), the
detector runs against *captured real PTY byte corpora* for both Claude
and Codex (`detect_fixtures.rs`, `codex_fixtures.rs` — including the
#399 live-repaint round trip), and the pump-level ordered-sequence
tests cover the serve-side flow. What #399 exposed was the last gap:
none of that ever ran the *shipped binary*. Closed by the live tier:
`e2e_real_claude_boots_to_a_detected_ready_state` /
`e2e_real_codex_boots_to_a_detected_ready_state` boot the real CLIs in
real tmux through the serve loop and require a detected ready/asking
state (codex's fresh-cwd trust chooser must surface as `?`).

**Residual gaps (accepted, low risk):** Cursor has no Working detector
and no fixture corpus (relies on the quiet-settle path); the
hook-freshness gate is unit-tested but not driven as a full pump
timeline. Neither has a recurrence history.

---

## 3. Session lifecycle & issue→PR transfer

**Design verdict: the architecture is right; the risk was concentrated
in test shape.** Identity is clean — `SessionKey` derives from
`WorkspaceKey`, so transfer = rewrite `session.workspace_key` + rebadge
every `terminal_meta` entry — and all four entry points (poll-driven
merge, TUI confirm, manual `x j` collapse, adopt) funnel into **one
commit owner** (`commit_workspace_move`), which persists the PR upsert,
issue deletes, and terminal-KV rewrites in a single store transaction
before broadcasting `TerminalsRebadged`. State emitters resolve the
*current* `terminal_meta` key, so a rebadged terminal's later events
land on the PR.

**Mechanism count: one** (four entries, one transaction owner). Good.

**Coverage, corrected.** Issue #404's premise ("the ONE rebadge test")
undersold `polling.rs`, which already covered multi-issue/multi-session
merges, event ordering, failed-batch atomicity, and live-terminal
stalling. The *real* gaps were shape gaps, all now closed:

- every transfer test hardcoded `claude` → hookless-codex rebadge now
  covered (`codex_terminal_survives_issue_to_pr_collapse`);
- every test seeded the PR up front → the natural lifecycle (work an
  issue first, PR appears on a later poll, gate prompts, user confirms)
  now covered
  (`pr_arriving_after_live_spawn_prompts_then_confirmed_merge_rebadges`);
- every test opted out of provisioning (`cwd:` override or pre-seeded
  `worktree_path`) → a spawn that runs REAL `git worktree add` and a
  collapse that migrates that real worktree now covered
  (`e2e_spawn_provisions_a_real_worktree_and_collapse_carries_it_to_the_pr`).

**Residual gap (accepted):** `on_main` terminals aren't persisted as
sessions, so their behavior across a collapse is undefined-by-design;
worth a decision if on-main usage grows.

---

## 4. Worktree provisioning

**Design verdict: crash-safe but not size-safe.** The `.partial` +
rename clone scheme, health-probe-gated reuse, and repo-locked
concurrency are solid; failure handling deliberately degrades to an
empty-dir session with a retryable error rather than wedging (bounded
by the 600 s clone cap, `kill_on_drop`). Two real defects remain open:

- **#405** — the bare clone is a full `--bare` clone with **no
  `--filter=blob:none`**, so a large repo burns the whole 600 s cap and
  can never succeed (the cap is flat, not size-aware — #403). The
  blobless filter is a production behavior change with its own
  blast radius (promisor-remote fetches at checkout time) and stays in
  #403/#405 scope; this audit's contribution is that the e2e
  provisioning test pins the *success* contract (a worktree holding the
  upstream's files — the empty-dir fallback fails it), so the fix lands
  against a test that means it.
- Nothing exercises the clone-timeout → empty-dir fallback path
  server-side; ledger marks it as an open gap under #403.

**Mechanism count: one** (all spawn paths — isolated, on-main,
standalone — go through `provision_worktree` → `WorktreeManager`).

**Test reality:** git-ops tests are exemplary (real git against local
upstreams, poisoned-clone recovery, stale-base escalation) — the gap
was that *server-side* provisioning was never driven end-to-end
(every server test bypassed it), which is exactly where #403/#405
manifest. The e2e tier now drives it with the network swapped for a
local upstream.

---

## 5. Stale-build masking (#391)

The guard machinery existed (`crates/tui/src/build_guard.rs` counts
`BUILD_GIT_SHA..origin/main` from baked provenance) but was gated to
release builds only — and release builds have no comparison basis yet,
so **no shipped configuration could ever warn**. The one scenario with
a real history of burning debugging hours — a dev binary running after
its checkout was pulled forward — got zero signal.

**Changed in this PR:** the gate is removed; dev builds count commits
behind `origin/main` too, with the fix wording matched to provenance
(`rebuild & restart` for dev, `update & restart` for installer-managed
release). The guard's git query now has a real-repository regression
test (`counts_commits_behind_in_a_real_checkout`) instead of only
string-parsing units. The remaining #391 scope (startup modal,
release-tag comparison, dismiss memory) stays in #391.

---

## The tier that makes this stick

`crates/server/tests/e2e_real_paths.rs` + `.github/workflows/nightly.yml`:

| level | what runs | when |
|---|---|---|
| real git | provisioning + collapse against a local upstream | every PR (`cargo test` / nextest, ~1 s) |
| real tmux | serve-loop restart recovery with deep scrollback | every PR where tmux exists; **required** (no skip-as-pass) in nightly via `LAZYBOX_E2E_REQUIRE=1` |
| live agents | real `claude` / `codex` boot → detected state | `#[ignore]`, opt-in `LAZYBOX_E2E_LIVE_AGENTS=1` (nightly variable, or locally: `LAZYBOX_E2E_LIVE_AGENTS=1 cargo nextest run -p lazybox-server --run-ignored only -E 'test(/^e2e_/)'`) |

Rules of the tier: tests are named `e2e_*` (wider nextest timeout rides
that prefix), assert **user-visible outcomes** (files in the worktree,
scrollback content in the replay a TUI would render, a `?` the user
would see), and prerequisites are either present or the test fails
loudly under the lane that promises them.
