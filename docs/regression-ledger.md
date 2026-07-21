# Regression ledger (#410)

Every historically-recurring bug, the specific test that now guards it,
and the shape that test actually covers — so "is the real path covered,
or a toy?" is answerable at a glance.

**This file is machine-checked.** `crates/core/tests/regression_ledger.rs`
parses every backticked *file-path::test-name* reference below and
fails the build if the file or the named test function disappears — a
guard listed here cannot silently rot. Entries marked **OPEN GAP** name
the missing coverage on purpose; close the gap, then move the entry.

Conventions: *real shape* = real subprocesses / captured real bytes /
full serve loop; *unit* = mocked or synthetic but structurally honest
about it.

## Scrolling / scrollback (#306, #321, #360, #362, #371, #393, #420)

| guard | shape it covers |
|---|---|
| `crates/tui-term/tests/viewport_scroll.rs::rendered_frame_follows_delta_scroll` | #306 — a delta scroll must move the *rendered frame*, against real libghostty |
| `crates/tui/src/components/terminal_stack.rs::shift_pageup_moves_viewport_and_shift_end_returns` | #321/#371 — keyboard scroll bindings through the single scroll owner, typed outcomes |
| `crates/tui/src/components/terminal_stack.rs::fresh_agent_keyboard_scroll_bindings_move_the_viewport` | #360 — the freshly-spawned-session shape that regressed after #321's fix |
| `crates/tui/src/components/terminal_stack.rs::wheel_in_left_tile_scrolls_left_while_right_is_focused` | #362 — wheel targets the tile under the cursor, not the focused tile |
| `crates/server/tests/replay_scrollback.rs::recovered_session_retains_meaningful_scrollback` | ring-budget depth under churn-heavy streams (live path, unit) |
| `crates/server/tests/tmux_restart.rs::restarted_backend_seeds_scrollback_from_tmux_history` | #306 restart half — capture-pane re-seed against a **real tmux server**; skip-as-pass is forbidden under `LAZYBOX_E2E_REQUIRE=1` |
| `crates/server/tests/e2e_real_paths.rs::e2e_serve_loop_restart_recovers_session_with_deep_scrollback` | #393 restart half, **real shape** — full serve loop, real tmux, daemon torn down, reconnecting client's snapshot reaches deep history |
| `crates/server/tests/tmux_restart.rs::live_backend_serves_deep_scrollback_without_restart` | #393 live half (PR #395) — the backend serves capture-pane history to a never-restarted session |
| `crates/server/tests/spawn_handler.rs::fetch_scrollback_round_trips_backend_history` | #393 — the FetchScrollback → TerminalScrollback wire round trip (mock backend) |
| `crates/tui/src/components/terminal_stack.rs::scroll_up_arms_one_fetch_per_visit` | #393 — the client trigger fires on the first upward scroll, even with zero local scrollback |
| `crates/tui/src/components/terminal_stack.rs::apply_scrollback_keeps_viewport_distance_from_bottom` | #393 — the deep rebuild doesn't yank the user out of their scroll position |
| `crates/server/tests/e2e_real_paths.rs::e2e_live_scroll_fetch_serves_deep_history_without_restart` | #393, **real shape** — the scroll-triggered fetch through the full serve loop against real tmux, no restart |
| `crates/libghostty-vt/tests/feed_while_scrolled.rs::viewport_stays_anchored_while_output_streams_in` | the invariant that makes live deep scrollback usable at all: streamed output must not snap a scrolled-up viewport to the bottom |
| `crates/server/src/pty.rs::seed_survives_ring_churn_past_capacity` | #420 — the reattach seed lives outside the evictable replay ring; a snapshot taken after live churn wraps the ring capacity must still replay the seed first (unit, real PTY child) |
| `crates/server/src/pty.rs::large_seed_leaves_full_ring_budget_for_live_output` | #420 — a near-capacity seed can't consume the ring's live-byte budget: snapshots stay `complete` (resync-servable) with the full seed leading |
| `crates/server/src/pty.rs::seed_survives_while_child_holds_the_alt_screen` | #420 follow-up — no screen mode bypasses the durable slot: with the child parked on the **alternate screen** the seed still leads every snapshot and it stays resync-servable (pane-level alt-screen denial is #393/PR #427) |
| `crates/server/tests/tmux_restart.rs::alt_screen_request_is_denied_so_agent_history_is_retained` | the root cause behind "scrollback dead on Claude": Claude ≥2.1 takes the pane to the alternate screen, which retains ZERO tmux history — the conf now denies it (real tmux, Claude-shaped smcup + output) |
| `crates/server/tests/tmux_restart.rs::alt_screen_pane_serves_no_deep_scrollback` | a pane already on the alt screen (pre-fix server config) must fetch `None`, not a one-screen capture |
| `crates/tui/src/components/terminal_stack.rs::shallow_capture_never_shrinks_the_grid` | "scrollbar disappears as soon as I start scrolling" — a rebuild that isn't strictly deeper than the current grid is a no-op |

**Narrowed gap (#393):** live-vs-restart *fidelity equivalence* — the
capture path drops OSC 8 hyperlinks and flattens soft wraps relative to
the raw stream. Depth parity is now guarded (above); styling/wrap
parity is not.

## Agent state / needs-input (#225, #357, #374, #397, #399)

| guard | shape it covers |
|---|---|
| `crates/agents/src/state_machine.rs::transition_table_enforces_idle_and_exit_invariants` | #357 — forbidden edges (`Working↛Idle`, `Done↛Idle`, `Exited` absorbing) as a table, not comments |
| `crates/agents/src/state_machine.rs::working_settles_to_done_not_idle` | #357 — a settled worker is Done, never a blank pill |
| `crates/agents/src/state_machine.rs::hookless_lifecycle_never_regresses_to_idle` | #225 — the hookless (Codex/Cursor) lifecycle |
| `crates/agents/src/state_machine.rs::ambiguous_working_never_clears_input_needed` | #374 — navigation/repaint noise can't clear a parked `?` |
| `crates/server/src/spawn_handler.rs::a_repaint_scrape_never_clears_a_parked_prompt` | #374 at the pump level, over real fixture bytes |
| `crates/server/src/spawn_handler.rs::agent_state_transitions_emit_an_ordered_sequence` | #357 — the ordered daemon transition stream end-to-end (mock backend) |
| `crates/server/src/spawn_handler.rs::a_quiet_unclassifiable_screen_settles_working_to_done` | #225 — weak-detector agents still finish |
| `crates/server/src/spawn_handler.rs::pty_reading_allowed_gates_on_hook_freshness` | hooks-vs-scrape precedence (#374 family) |
| `crates/server/tests/pump_teardown.rs::exiting_agent_broadcasts_exited_not_stuck_working` | #357 — failed/exited agents can't hang on Working |
| `crates/agents/tests/agents.rs::guarded_composer_protocol_is_shared_by_claude_and_codex` | #397 — one PTY prompt protocol, not per-agent drift |
| `crates/agents/tests/codex_fixtures.rs::codex_real_approval_round_trip_drives_the_chunk_detector` | #399 — the live-repaint approval round trip over **captured real Codex bytes** |
| `crates/server/src/spawn_handler.rs::codex_approval_modal_surfaces_input_needed_immediately` | #399 — the current-chunk fast path through the pump |
| `crates/server/tests/e2e_real_paths.rs::e2e_real_claude_boots_to_a_detected_ready_state` | #397/#399, **real shape** — the shipped `claude` binary, real tmux, serve loop, detected state |
| `crates/server/tests/e2e_real_paths.rs::e2e_real_codex_boots_to_a_detected_ready_state` | #399, **real shape** — real `codex`; its fresh-cwd trust chooser must surface as `?` |

## Issue→PR session transfer (#404)

| guard | shape it covers |
|---|---|
| `crates/server/tests/spawn_handler.rs::collapse_into_pr_carries_live_terminal_to_the_pr` | the original rebadge contract (single claude terminal, pre-seeded session — the narrow shape #404 called out) |
| `crates/server/tests/polling.rs::closing_pr_transfers_live_session_durably_to_pr` | poll-driven transfer durability |
| `crates/server/tests/polling.rs::combining_multiple_issues_with_live_sessions_rebadges_every_terminal` | multi-issue × multi-terminal rebadge, in-memory + persisted |
| `crates/server/tests/polling.rs::codex_terminal_survives_issue_to_pr_collapse` | #404 — the **hookless codex** terminal shape every prior transfer test skipped |
| `crates/server/tests/polling.rs::pr_arriving_after_live_spawn_prompts_then_confirmed_merge_rebadges` | #404 — the natural lifecycle: live spawn on a bare issue, PR arrives on a *later* poll, gate prompts, confirm rebadges |
| `crates/server/tests/e2e_real_paths.rs::e2e_spawn_provisions_a_real_worktree_and_collapse_carries_it_to_the_pr` | #404, **real shape** — spawn runs REAL provisioning (`git worktree add` off a local upstream), collapse migrates that real worktree |
| `crates/server/tests/issue_pr_transfer.rs::collapse_retires_pristine_pr_stub_and_carries_wip_worktree` | #446, the worktree half — a pre-collapse PR stub session (real worktrees, dead records) is retired so the issue's WIP checkout becomes the PR's default session and a later spawn lands in it |
| `crates/server/tests/issue_pr_transfer.rs::collapse_keeps_pr_session_with_uncommitted_work` | #446 guard-rail — a PR-side worktree holding local work is NOT a stub; the collapse keeps both sessions and touches neither checkout |

## Worktree provisioning (#403, #405)

| guard | shape it covers |
|---|---|
| `crates/git-ops/tests/provision.rs::poisoned_bare_clone_is_detected_and_recloned` | broken-bare recovery with real git |
| `crates/git-ops/tests/provision.rs::stale_partial_clone_dir_is_cleaned_up` | crash-safe `.partial` scheme |
| `crates/git-ops/tests/worktree_base.rs::worktree_creation_succeeds_when_fetch_fails` | degraded-fetch provisioning |
| `crates/server/tests/e2e_real_paths.rs::e2e_spawn_provisions_a_real_worktree_and_collapse_carries_it_to_the_pr` | the server-side success contract: a provisioned worktree holds the upstream's files — the silent empty-dir fallback **fails** this test |

**OPEN GAP (#405):** no blobless filter on the bare clone — large repos
exhaust the 600 s cap and can never provision. **OPEN GAP (#403):** the
clone-timeout → empty-dir fallback path has no server-side test. Both
land with the #403/#405 fixes, against the e2e success contract above.

## Stale build (#391)

| guard | shape it covers |
|---|---|
| `crates/tui/src/build_guard.rs::source_build_detects_a_newer_checkout_head` | the source guard's git queries against a **real repository** laid out like the #391 incident |
| `crates/tui/src/build_guard.rs::source_build_uses_ahead_tracking_upstream` | a safely fast-forwardable tracking branch is named as the available source target |
| `crates/tui/src/build_guard.rs::divergent_main_does_not_mark_a_feature_build_stale` | divergent feature work does not produce a false stale-build warning |
| `crates/tui/src/build_guard.rs::release_comparison_only_reports_newer_semver` | installed releases only notify for a strictly newer published semantic version |
| `crates/tui/src/build_guard.rs::modal_copy_scopes_commands_and_names_the_install_channel` | source commands run in the baked checkout and release commands match Homebrew versus the shell installer |
| `crates/tui/src/realm/model/tests.rs::update_modal_dismissal_is_persisted_per_available_target` | explicit dismissal is remembered for one target while a later target is shown again |

## `w` targets the running agent (#224, #418)

| guard | shape it covers |
|---|---|
| `crates/tui/src/components/sidebar/tests.rs::single_running_agent_wins_over_default` | #224/#418 — one running non-default agent (Codex) is the `w` target, never the configured default |
| `crates/tui/src/realm/model/tests.rs::bare_w_targets_the_running_agent_over_default` | #224 — the same contract through catalog dispatch (`w w` → Spawn(codex)) |
| `crates/tui/src/realm/model/tests.rs::w_w_fires_default_work_immediately` | #224 — full key path: `w w` on a running Codex lands an `InjectPrompt`, not a fresh spawn |
| `crates/tui/src/components/sidebar/tests.rs::several_agents_ask_even_when_default_is_among_them` | #418 — several distinct agents resolve to `Choose`, even when the default is one of them |
| `crates/tui/src/realm/model/tests.rs::bare_w_with_several_agents_mounts_chooser_and_pick_targets_it` | #418 — the chooser modal mounts, and the pick replays the work spawn against the chosen agent |
| `crates/tui/src/realm/model/tests.rs::work_chooser_pick_injects_into_the_chosen_agent` | #418 — the pick rides the spawn→inject rewrite into the chosen agent's terminal |
| `crates/tui/src/realm/model/tests.rs::bare_w_with_no_running_agent_spawns_the_default` | #418 — the default agent is the answer only when nothing is running |
