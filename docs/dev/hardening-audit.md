# Hardening audit — findings & triage (#168)

The inventory pass for the hardening epic (#168), step 1: *"a single findings
doc with severity + location. No fixes yet — just the map."* It is the
checklist the rest of the phase audits against, and the source for the
per-dimension follow-up issues.

**Method.** Static sweeps across all 16 first-party crates (LOC, `unwrap`/
`expect`/`panic!`, `TODO`/`FIXME`/`HACK`, `#[allow]`, log macros, channel
capacities, crate-dep graph) plus targeted reads of the areas the issue calls
out as flaky: agent state detection, prompt injection, and the sync/polling
loop. `libghostty-vt` / `libghostty-vt-sys` are vendored and excluded.

**Severity.** **High** — blocks the phase's testability/modularity bar or hides
real bugs. **Medium** — debt that compounds; fix this phase. **Low** — polish;
fix opportunistically or ticket. **Info** — verified-good, recorded so we don't
re-audit it.

## Scoreboard

| Dimension | Verdict |
|---|---|
| Shortcuts (`TODO`/`FIXME`/`HACK`) | **Clean** — 10 hits, all deliberate Windows-port placeholders ([F-INFO-1](#info)) |
| `unwrap`/`expect`/`panic!` on real paths | **Clean** in core libs (all in tests); a few documented-poison `expect`s in `server` ([F6](#f6)) |
| Agent state detection | **Solid** — already pure & fixture-driven ([F-INFO-2](#info)) |
| Prompt injection timing | **Solid** — decoupled, 3-tier fallback, tested ([F-INFO-3](#info)) |
| Sync / polling re-entrancy & locks | **Solid** — #131/#133 fixed, regression-tested ([F-INFO-4](#info)) |
| TUI snapshot coverage | **Under-baked** — 2 of ~30 components ([F1](#f1)) |
| Module size / god-modules | **Needs work** — 9 files > 1.5k LOC ([F2](#f2)) |
| Clock injection for time-dependent logic | **Gap** ([F3](#f3)) |
| Provider-adapter fetch tests | **Gap** ([F4](#f4)) |
| Documented core-lib invariant | **Drift** — wording contradicts the dep graph ([F5](#f5)) |
| Logging hygiene | **Needs tuning** — README admits the spam ([F8](#f8)) |
| Dead code | **Minor** — 13 `#[allow(dead_code)]` to triage ([F7](#f7)) |

The headline: the three areas the issue named as chronically flaky — Claude
state detection, injection timing, polling re-entrancy — have **already been
pulled behind testable seams** by the recent #156/#158/#153 and #131/#133 work.
The remaining debt is breadth of test coverage (snapshots, provider fetch,
clock) and module size, not the named hotspots.

---

## Findings

### <a id="f1"></a>F1 — TUI render-snapshot coverage is ~7%  · High · `tui`

CLAUDE.md states *"every TUI component has a render snapshot (insta + ratatui
`TestBackend`)."* Reality: **7 snapshots covering 2 components** —
`Sidebar` (6 variants) and `WhichKey` (1) — in `crates/tui/tests/snapshots/`.

No snapshots for either of the other two panes or any modal:

- Panes: `RightPane` (`components/right_pane/mod.rs`, 1599 LOC),
  `TerminalStack` (`components/terminal_stack.rs`, 3063 LOC).
- Modals/realm components (`crates/tui/src/realm/components/`): `Choice`,
  `Confirm`, `Input`, `Textarea`, `Help`, `Error`, `Loading`, `Polling`,
  `SnippetPicker`, `Splash`, `SyncStatus`, `Tour`, `Terminals`, `Right`,
  `Sidebar`.
- Supporting components: `ActivityFeed`, `CommentRender`, `Table`, `TaskLabel`,
  `WorkspaceRow`, `VisibleRows`.

`Choice` and `Confirm` back the destructive flows (merge, remove, label,
reviewers) — exactly the surfaces where a silent render regression is most
costly. **Fix:** backfill `TestBackend` snapshots pane-by-pane; this is the
single highest-leverage testability win and prerequisite for safely splitting
the god-modules in F2.

### <a id="f2"></a>F2 — God-modules · Medium · `server`, `tui`, `gh-provider`

Files over 1.5k LOC, by size:

| File | LOC | Note |
|---|---|---|
| `server/src/polling/mod.rs` | 3925 | Already has `autofix`/`handlers`/`mutate`/`scheduler` submods; core driver (`spawn`/`run_tick_inner`/`tick_with_state`/`rescope_with_state` ~450 LOC) + `GhSource`/`LinearSource` adapters (~1400) could split into `polling/driver.rs` + `polling/sources/`. |
| `gh-provider/src/graphql.rs` | 3562 | GraphQL query/response types — splittable by query family. |
| `tui/components/terminal_stack.rs` | 3063 | VT widget + tab manager + key router in one; extract tab-bar and scrollback. |
| `gh-provider/src/client.rs` | 2974 | REST/GraphQL client surface. |
| `server/src/spawn_handler.rs` | 2355 | Spawn + state-pump + inject pipeline; pump/inject are cohesive enough to extract. |
| `tui/realm/model/mod.rs` | 1960 | Orchestrator state machine + dispatch. |
| `tui/setup_flow.rs` | 1646 | First-run wizard. |
| `tui/components/sidebar/mod.rs` | 1637 | List + filter + sort + grouping. |
| `tui/components/right_pane/mod.rs` | 1599 | Task detail + activity feed + read timer. |

**Fix:** split per-file follow-ups, each behind green snapshots (F1) so the
refactor is provably behavior-preserving. Do F1 first.

### <a id="f3"></a>F3 — No injectable clock in the polling loop · Medium · `server`

`crates/server/src/polling/mod.rs` reads wall-clock time directly (`Utc::now`,
`Instant::now`, `tokio::time::sleep`). Time-dependent behavior — snooze expiry,
auto-fix cooldown, the tiered full-sweep-vs-notifications heartbeat — is
therefore exercised only with real `sleep`s in tests, which are slow and
racy. The `TaskSource`/`Store`/`bus` seams are already injectable (proven by
`FakeSource` in `tests/polling.rs`); a clock seam would make the remaining
time logic deterministic. **Fix:** thread a `Clock` trait (real + fake) through
the tick driver.

### <a id="f4"></a>F4 — Provider fetch paths are integration-only · Medium · `server`, `gh-provider`, `linear-provider`

`tests/polling.rs` (3268 LOC) drives the tick/upsert/rescope loop thoroughly
via `FakeSource`, but the real `GhSource`/`LinearSource` fetch logic —
tiered full-sweep vs. incremental notifications, `@lazybox` mention scanning,
auto-fix queueing — has no fixture coverage; it is "run against live GitHub and
squint." **Fix:** record VCR-style HTTP fixtures and assert the adapters
produce the expected `Vec<Task>` + queued actions offline.

### <a id="f5"></a>F5 — Core-lib dependency invariant is mis-stated · Medium · docs

CLAUDE.md (twice) says the four core libs *"must NEVER depend on each other."*
The actual graph: `core` and `auth` depend on nothing; **`events` depends on
`lazybox-core`** and **`store` depends on `lazybox-core`** (both
`crates/{events,store}/Cargo.toml`). So the literal rule is already violated by
design. The real, enforceable invariant is: *`core` is the leaf; `auth`,
`events`, `store` may depend on `core` but not on each other.* **Fix:** correct
the wording in both CLAUDE.md spots and add a check (a small test over the
crate graph, or a `cargo-deny`/`cargo-machete`-style rule) so the boundary is
mechanically enforced rather than honor-system.

### <a id="f6"></a>F6 — Mutex-poison `expect`s in the polling loop · Low · `server`

Eleven `.expect("… mutex poisoned")` calls on `std::sync::Mutex` guards in
`polling/mod.rs` (e.g. `:345`, `:352`, `:384`, `:560`, `:980`, `:987`, `:1041`,
`:1345`, `:1380`, `:1399`, `:1795`). These only fire if another thread panicked
while holding the lock, and `spawn` wraps each tick in `catch_unwind`
(`:2261`) so the loop survives. Acceptable as-is; recording it so the audit
doesn't re-flag it. Two further `expect`s on genuinely-impossible conditions
(`build_pr_search_qualifiers` `:1222`, `create_empty_workspace` `:3449`) are
fine to leave.

### <a id="f7"></a>F7 — Dead-code allowances to triage · Low · multiple

Thirteen `#[allow(dead_code)]`: `gh-provider/src/graphql.rs` (5×, several
"captured for debug — not yet used"), `linear-provider/src/graphql.rs:238`,
`tui/components/right_pane/card.rs:7`, `tui/realm/model/helpers.rs` (2×),
`tui/realm/model/mod.rs:383`,
`server/src/api_gateway.rs:406`. **Fix:** per item — wire it up or delete it;
none should stay dead with a blanket allow.

### <a id="f8"></a>F8 — Log-level calibration · Low · all crates

Macro counts (non-test): `trace!` 5, `debug!` 48, **`info!` 125**, **`warn!`
186**, `error!` 41. `warn!` outnumbering `info!` and 125 info-level lines is the
"log spam in `/tmp/lazybox.log`" the README already apologizes for
(`README.md:17`, `:130`). **Fix:** demote routine lifecycle `info!`s to
`debug!` and reserve `warn!` for genuinely actionable conditions, so
`RUST_LOG=lazybox=info` is usable by default.

### <a id="info"></a>Verified-good (recorded, no action)

- **F-INFO-1 — Shortcuts are clean.** All 10 `TODO`/`FIXME`/`HACK` hits are
  deliberate Windows-port placeholders in `tui-core/src/platform.rs` and
  `ipc/src/transport.rs`. `docs/TODO.md` is an intentional deferred-decision
  log (sidebar relationship chip; macOS `.app` notification bundle), not a
  wishlist. No sweep action.
- **F-INFO-2 — Agent state detection is already a pure seam.**
  `crates/agents/src/detect.rs` operates on `&[u8]`/`&str` only — no PTY,
  Mutex, clock, or async. `crates/agents/tests/detect_fixtures.rs` drives it
  with **11 real-byte `include_bytes!` captures** (generated by
  `tests/fixtures/generate.py`) plus 50+ synthetic shapes in `tests/agents.rs`.
  #122 (conversational `?`), #142 (footer-only readiness), #153 (fresh-spawn
  version-banner false match), and #156 (chooser-recency guards) each have
  regression coverage. This is the model the rest of the phase should imitate,
  not a thing to re-architect.
- **F-INFO-3 — Prompt injection is decoupled and tested.** The state pump
  (`spawn_handler.rs`) fires a `ready_signal`; the inject task waits on a
  three-tier ladder (ready-signal → first-output+settle → 10s hard deadline as
  last resort). The hard deadline is the fallback, not the common path — the
  #153 concern is mitigated.
- **F-INFO-4 — Polling async-correctness is sound.** No lock held across
  `.await`. #131 (re-entrancy) is fixed by keeping `MergePromptMemory` in a
  separate lock; #133 (serve-loop starvation) by the checkout/restore pattern
  in `run_one_tick` plus a non-blocking `try_lock` in `set_focused_workspace`,
  both with regression tests. The broadcast bus is bounded
  (`BUS_CAPACITY = 1024`, skip-ahead on lag).
- **F-INFO-5 — `println!`/`eprintln!` are confined to CLI entry points**
  (`tui/src/main.rs` ×16, `tui/src/slack_prune.rs` ×11) — legitimate stdout for
  subcommands, not stray debug output.
- **F-INFO-6 — The clippy bar is already green.** `cargo clippy --workspace
  --all-targets` finishes clean under the workspace's `warnings = "deny"`
  (`Cargo.toml:34`). The measurable bar in #168 is met today; the job is to
  keep it green through the F2 refactors.

---

## Triage

**Fix in-phase (small, low-risk, this epic):**

- F5 — correct the invariant wording + add an enforcement check.
- F7 — dead-code triage (delete or wire up).
- F8 — log-level pass.

**Follow-up issues (each its own reviewable PR):**

1. Backfill `TestBackend` snapshots for `RightPane`, `TerminalStack`, and the
   realm modals (F1). *Do before #2.*
2. Split god-modules, one file per PR, behind the F1 snapshots (F2).
3. Introduce a `Clock` seam in the polling driver (F3).
4. VCR fixtures for `GhSource`/`LinearSource` fetch paths (F4).

**Won't-fix / leave:** F6 (poison `expect`s are correct given `catch_unwind`),
F-INFO-1…5.

## Out of scope for this pass

The feature-by-feature audit against the #167 catalog (dimension 3) needs that
inventory to exist first; once it lands, label each feature
solid/needs-work/under-baked/cut using the evidence here. The three named
flaky features already have an evidence-based verdict above (all **solid**).
