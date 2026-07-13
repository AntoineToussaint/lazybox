# Deep critical review (issue #180)

> **Historical** — point-in-time review as of issue #180. Superseded;
> crate counts, file paths, and findings may no longer match the code.
> Do not act on this document.

A full pass over the brittle subsystems — the ones behind repeated
regressions and "still happening" reports — along four axes:
**correctness, modularity, debuggability, testability**. Each subsystem
was audited against the actual code (every finding cites a `file:line`
that was read, not inferred). Findings are severity-ranked; "verified"
means the path was read and the behaviour confirmed, "reported" means it
surfaced in the audit but still needs a repro before acting.

Resulting work is tracked as one child issue per subsystem so this EPIC
collapses to a triage hub rather than a single mega-PR:

| Subsystem | Child issue |
| --- | --- |
| 1. Issue→PR session merge / combine | [#186](https://github.com/AntoineToussaint/lazybox/issues/186) |
| 2. Agent state detection & broadcasting | [#187](https://github.com/AntoineToussaint/lazybox/issues/187) |
| 3. Keybindings single source of truth | [#188](https://github.com/AntoineToussaint/lazybox/issues/188) |
| 4. Performance — GH sync + terminal | [#189](https://github.com/AntoineToussaint/lazybox/issues/189) |
| 5. General cleanup | [#190](https://github.com/AntoineToussaint/lazybox/issues/190) |

## Headline

The codebase is healthier than the EPIC's framing suggests. The
merge subsystem has a regression test for every historical incident
(#78, #161, #167, #169); production `unwrap()` discipline is excellent
(8 in all library crates, every one guarded or a bare mutex-poison);
the error-type and core-4 dependency conventions are fully respected;
and `#170` (the missing `]]` footer hint) is **already fixed and
tested** — almost certainly a "still happening" report against a stale
branch.

The real residual risk is **structural and measurement-shaped**, not a
pile of live bugs:

- The two most regression-prone subsystems (merge, agent state) encode
  their invariants in *call-sequence across duplicated paths*, with only
  the consuming end asserted. They are correct by convention; they
  should be correct by construction (one owner, asserted ordering).
- There is **one genuine live bug**: a `leave_terminal` keybind override
  makes the terminal footer advertise a key that does nothing (#188).
- There are **zero benchmarks** in the repo, so every performance claim
  is reasoning, not measurement (#189).
- There is **dead code that violates a stated dependency rule**: the
  legacy `events` crate + `GhPoller` are unused, yet a provider crate
  depends on the dead bus (#190).

## Per-subsystem summary

### 1. Issue→PR session merge ([#186](https://github.com/AntoineToussaint/lazybox/issues/186))

Six event-ordering invariants govern a silent collapse (rebadge before
remove, upsert-PR before remove-issue, commit before delete, live-key on
rebadged terminals, persisted-key rewrite, merged-trails-removed). All
six are asserted *somewhere*, but only at the TUI-consumption end — the
daemon hand-feeds the correct order to the TUI tests, so a refactor that
reordered `finalize_issue_merges` vs `commit_upsert` would break an
invariant and fail **no** daemon test.

- **[medium]** No daemon-side bus-order test (`polling/mod.rs:2952-2967`).
- **[medium / modularity]** Four call sites independently sequence
  `absorb → migrate → commit → delete → broadcast`; two duplicate the
  emission. Extract a single `commit_merge` owner.
- **[low]** Silent empty-rebadge (`rebadge_terminals`, mod.rs:3727) — add
  a trace; `closed_ids.dedup()` on an unsorted Vec; `merge_follow_from`
  single-`Option` drop on mid-burst navigation.
- *Missing tests:* daemon bus-order assertion; property/fuzz over
  N issues × M sessions × {live, dead, none} terminals asserting
  session-count conservation; restart-after-rebadge recovery.

### 2. Agent state detection & broadcasting ([#187](https://github.com/AntoineToussaint/lazybox/issues/187))

Three `Event::AgentState` emitters (output pump `spawn_handler.rs:925`,
optimistic flip `:2651`, hook `:3242`) resolve the live owning session
key three different ways — only the pump uses the canonical
`live_session_key` helper; the others inline the lookup with different
miss-handling. This is the #167 stale-key bug class, and the EPIC's
"three emitters is itself a smell" is justified.

- **[high / modularity]** Unify behind one
  `broadcast_agent_state(.., source: StateSource)` reusing
  `live_session_key`. Fixes the divergence and the three-different-log-
  strings debuggability gap at once.
- **[high / testability]** No transition-table test — every fixture in
  `agents/tests/detect_fixtures.rs` is a single-state snapshot, but the
  bugs live in *transitions under a key*. Add an ordered-chunk sequence
  test.
- **[medium]** No test pins flip/hook to the pump's post-rebadge key
  resolution; `maybe_emit_state_change` is a 260-line / 15-arg inline fn
  that's untestable as a unit.

### 3. Keybindings single source of truth ([#188](https://github.com/AntoineToussaint/lazybox/issues/188))

`#170` is fixed (the `]]` hint is the first footer entry, with regression
tests). The real residual:

- **[high / correctness — the one live bug]** A `leave_terminal` rebind
  makes the footer **lie**: dispatch (`keys.rs:280`) keys off
  `terminal_escape_char`, but the footer (`terminal_stack.rs:1735`)
  renders the overridable `leave_terminal` catalog chord. Remap it and
  the footer shows "Esc exit to sidebar" while Esc does nothing.
- **[medium]** No test asserts every footer hint is catalog-backed;
  the Tour hardcodes ~20 key hints as prose (only a subset is checked);
  the help panel hardcodes `]]<key>` ignoring the escape char.

The dispatch core *is* genuinely catalog-driven and guarded by build-time
collision detectors — the drift is confined to the hand-curated footer /
tour / help duplicates.

### 4. Performance — GH sync + terminal ([#189](https://github.com/AntoineToussaint/lazybox/issues/189))

- **[high]** Zero benchmarks exist; the `commit_upsert` no-change
  short-circuit (the single most load-bearing poll optimization) has no
  test, so a future volatile field would silently defeat it.
- **[high]** Off-screen agent terminals pay full VT-parse cost on every
  output chunk (`terminal_stack.rs:1552` feeds every terminal; only
  *render* is gated to the visible slot) — scales by hidden-agent count
  on the UI thread.
- **[medium]** Per-task upserts are strictly sequential with an inline
  git op; the windowed-sweep perf win is asserted but never measured;
  the Clean-path blit clones every cell individually.
- *Missing:* criterion benches for the feed path, the render-diff path,
  and a full-inbox poll tick.

### 5. General cleanup ([#190](https://github.com/AntoineToussaint/lazybox/issues/190))

- **[medium]** The `events` crate + `GhPoller` (213 LOC) are fully dead
  (`GhPoller::` never constructed), and `gh-provider` depends on the dead
  bus — violating "provider crates depend on core + auth only." Delete
  both; drop the unused dep from `gh-provider` and `server`.
- **[medium]** Markdown/card render + sidebar render/handlers have no
  direct tests; only 8 snapshots exist repo-wide.
- **[low]** Three `slack.rs` `.lock().unwrap()` break the
  `.expect("… poisoned")` convention.

## Method

Five auditors (one per subsystem) read the relevant paths and produced
severity-ranked findings; the highest-value and most surprising claims
(dead-code reachability, the `#170` status, the `leave_terminal`
dispatch, the three emitters) were then re-verified by hand before
filing. No production code was changed in this pass — the deliverable is
the findings above plus the five child issues.
