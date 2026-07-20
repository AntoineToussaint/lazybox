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

## Scrolling / scrollback (#306, #321, #360, #362, #371, #393)

| guard | shape it covers |
|---|---|
| `crates/tui-term/tests/viewport_scroll.rs::rendered_frame_follows_delta_scroll` | #306 — a delta scroll must move the *rendered frame*, against real libghostty |
| `crates/tui/src/components/terminal_stack.rs::shift_pageup_moves_viewport_and_shift_end_returns` | #321/#371 — keyboard scroll bindings through the single scroll owner, typed outcomes |
| `crates/tui/src/components/terminal_stack.rs::fresh_agent_keyboard_scroll_bindings_move_the_viewport` | #360 — the freshly-spawned-session shape that regressed after #321's fix |
| `crates/tui/src/components/terminal_stack.rs::wheel_in_left_tile_scrolls_left_while_right_is_focused` | #362 — wheel targets the tile under the cursor, not the focused tile |
| `crates/server/tests/replay_scrollback.rs::recovered_session_retains_meaningful_scrollback` | ring-budget depth under churn-heavy streams (live path, unit) |
| `crates/server/tests/tmux_restart.rs::restarted_backend_seeds_scrollback_from_tmux_history` | #306 restart half — capture-pane re-seed against a **real tmux server**; skip-as-pass is forbidden under `LAZYBOX_E2E_REQUIRE=1` |
| `crates/server/tests/e2e_real_paths.rs::e2e_serve_loop_restart_recovers_session_with_deep_scrollback` | #393 restart half, **real shape** — full serve loop, real tmux, daemon torn down, reconnecting client's snapshot reaches deep history |

**OPEN GAP (#393):** live-vs-restart *equivalence* — same output through
the live byte stream and through capture-pane must reconstruct the same
grid (hyperlinks, soft wraps, depth). Lands with the #393 unification.

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
| `crates/tui/src/build_guard.rs::counts_commits_behind_in_a_real_checkout` | the guard's actual `rev-list` query against a **real repository** laid out like the #391 incident |
| `crates/tui/src/build_guard.rs::message_pluralizes_and_names_the_fix` | dev builds name the dev fix (`rebuild & restart`) |
| `crates/tui/src/realm/model/tests.rs::outdated_build_raises_persistent_warning` | the persistent banner + sidebar flag, no longer provenance-gated |

**Remaining #391 scope** (startup modal, release-tag comparison for
installed builds, dismiss memory) is tracked in #391 itself.
