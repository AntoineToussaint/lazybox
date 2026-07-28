//! The per-session agent status the sidebar tracks, and the pure
//! projections that turn it into UI signals.
//!
//! ## One projected state per session, not three parallel sets
//!
//! Each terminal is in exactly one [`AgentState`] at a time. The sidebar
//! aggregates those terminal-scoped readings into one projected value per
//! workspace for its shared UI slot (working spinner vs `?` pill vs `✓`
//! done mark). A single projected value makes the illegal combinations
//! the earlier three-`HashSet` design allowed —
//! "all-empty = no status", "the same key in two sets at once",
//! "any ordering, including `Done → Idle`, passes" — simply
//! unrepresentable (issue #327).
//!
//! The daemon is the single state machine: it commits every reading
//! through `AgentStateMachine::transition`, damps the ambiguous PTY
//! flaps, and broadcasts only legal, deduplicated transitions
//! (`crates/agents/src/state_machine.rs`). This map therefore just
//! mirrors the latest broadcast state verbatim through
//! [`apply_agent_state`] — it does NOT re-run the transition rules. A
//! second copy of the machine here would be a second cached state that
//! can desync from the daemon's on a dropped broadcast (the bus is
//! lossy and a lagging receiver skips ahead); mirroring verbatim stays
//! self-healing, folding straight to the authoritative state carried by
//! terminal snapshots and subsequent deltas.
//!
//! ## Why this state lives in the sidebar, not on `Workspace`
//!
//! The earlier design mutated `workspace.sessions[i].state` whenever
//! an `Event::AgentState` arrived. That worked until the next poll
//! cycle re-broadcast `WorkspaceUpserted` with the workspace freshly
//! loaded from the store — and the store doesn't carry transient
//! agent state, so every poll silently clobbered the badge. Symptom:
//! the `?` indicator would flash on for ~1 second after Claude
//! prompted and then disappear at the next minute boundary.
//!
//! Fix: keep agent state in sidebar-local terminal and projection maps,
//! independent of workspace data. Polling broadcasts cannot touch them;
//! snapshots and `Event::AgentState` deltas both come from the daemon.

use lazybox_core::{SessionKey, Workspace};
use lazybox_ipc::AgentState;
use std::collections::HashMap;

/// How a workspace's attention signals changed as a result of folding
/// one `AgentState` reading into the per-session state. Every field is
/// derived from the single before/after value, so the flags can never
/// contradict each other the way three independently-mutated sets could.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StateChange {
    /// The workspace's "needs input" membership flipped in either
    /// direction. Only this flip changes the visible row list (its
    /// per-repo attention counter reads asking-ness), so the caller
    /// recomputes only when it's set.
    pub asking_changed: bool,
    /// Rising edge into `InputNeeded` — fire the one-shot "needs input"
    /// alert (desktop notification + footer notice). Never `true` on a
    /// repeat broadcast, so the notification center isn't spammed.
    pub now_asking: bool,
    /// Rising edge into `Done` — fire the one-shot "finished" alert (#80),
    /// on the same terms as `now_asking`.
    pub now_done: bool,
}

/// Derive attention edges from a workspace projection changing.
///
/// The projection may disappear when its last terminal exits, so both
/// sides are optional. Terminal-aware clients use this after aggregating
/// their per-terminal source state; [`apply_agent_state`] uses it for the
/// simpler one-reading-per-workspace case.
pub fn state_change(previous: Option<AgentState>, incoming: Option<AgentState>) -> StateChange {
    if previous == incoming {
        return StateChange::default();
    }
    StateChange {
        asking_changed: (previous == Some(AgentState::InputNeeded))
            != (incoming == Some(AgentState::InputNeeded)),
        now_asking: incoming == Some(AgentState::InputNeeded),
        now_done: incoming == Some(AgentState::Done),
    }
}

/// Store the daemon's latest `AgentState` reading for `workspace_key`,
/// returning how the workspace's attention signals changed.
///
/// The daemon has already validated and damped the reading (see the
/// module docs), so this mirrors it verbatim — no client-side transition
/// rules — which keeps the map self-healing against a dropped broadcast.
/// A repeat of the current state is a no-op that reports
/// [`StateChange::default`] (all-false) so a streaming agent's per-chunk
/// re-broadcast neither re-alerts nor re-renders.
pub fn apply_agent_state(
    states: &mut HashMap<SessionKey, AgentState>,
    workspace_key: &SessionKey,
    incoming: AgentState,
) -> StateChange {
    let prev = states.insert(workspace_key.clone(), incoming);
    state_change(prev, Some(incoming))
}

/// Re-point a session's state entry when the daemon rebadges a terminal
/// from one session onto another (issue→PR collapse, manual adopt).
/// Moves any state stored under `from` onto `to`; returns the moved
/// state, or `None` when `from` carried none.
///
/// This is the `live_session_key`-on-the-client side of the #205
/// invariant: the daemon re-broadcasts an agent's `AgentState` only on
/// its next output chunk, but an agent parked on a prompt
/// (`InputNeeded`) produces none. Without migrating the entry here, a
/// moved agent's pill stays pinned to the now-deleted issue key and the
/// PR row shows no badge — the session reads as lost when it isn't
/// (`live_session_key` is in `crates/server/src/spawn_handler.rs`).
pub fn rebadge_attention(
    states: &mut HashMap<SessionKey, AgentState>,
    from: &SessionKey,
    to: &SessionKey,
) -> Option<AgentState> {
    let state = states.remove(from)?;
    states.insert(to.clone(), state);
    Some(state)
}

/// The state stored for `workspace`, or `None` when the daemon has
/// never reported one (treated as `Idle` by every consumer).
pub fn workspace_agent_state(
    workspace: &Workspace,
    states: &HashMap<SessionKey, AgentState>,
) -> Option<AgentState> {
    states.get(&SessionKey::from(&workspace.key)).copied()
}

/// True iff the workspace's agent is actively working (drives the
/// animated spinner).
pub fn workspace_is_working(
    workspace: &Workspace,
    states: &HashMap<SessionKey, AgentState>,
) -> bool {
    matches!(
        workspace_agent_state(workspace, states),
        Some(AgentState::Working)
    )
}

/// True iff the workspace's agent has finished its turn (drives the `✓`
/// indicator; #80).
pub fn workspace_is_done(workspace: &Workspace, states: &HashMap<SessionKey, AgentState>) -> bool {
    matches!(
        workspace_agent_state(workspace, states),
        Some(AgentState::Done)
    )
}

/// True iff the workspace's agent is waiting on input. Single source of
/// truth for the workspace-level needs-input check (sidebar header
/// counter, row pill, `!` jump predicate).
pub fn workspace_is_asking(
    workspace: &Workspace,
    states: &HashMap<SessionKey, AgentState>,
) -> bool {
    matches!(
        workspace_agent_state(workspace, states),
        Some(AgentState::InputNeeded)
    )
}

/// True iff the workspace's agent process has exited (clean or crash;
/// drives the `✗` indicator). Kept distinct from a blank/`Idle` row so a
/// dead agent reads as "the process ended, restart it" rather than
/// silently vanishing (#356/#357).
pub fn workspace_is_exited(
    workspace: &Workspace,
    states: &HashMap<SessionKey, AgentState>,
) -> bool {
    matches!(
        workspace_agent_state(workspace, states),
        Some(AgentState::Exited { .. })
    )
}

/// Pick the next workspace that needs the user's attention, starting
/// after `current` in `keys_order`. Wraps around. Returns `None`
/// when no workspace is asking.
///
/// The `keys_order` argument is the visible order from the sidebar
/// (so `!` follows the user's current sort/filter) — not the
/// underlying `HashMap` iteration order, which is non-deterministic.
///
/// "Starting after `current`" means: if the user is already focused
/// on an Asking workspace, `!` skips to the next one rather than
/// re-selecting the same row. When `current` is None (no selection)
/// we start from the top of `keys_order`.
pub fn next_asking_workspace(
    states: &HashMap<SessionKey, AgentState>,
    keys_order: &[SessionKey],
    current: Option<&SessionKey>,
) -> Option<SessionKey> {
    next_matching_workspace(keys_order, current, |k| {
        matches!(states.get(k), Some(AgentState::InputNeeded))
    })
}

/// Generic "advance the cursor to the next flagged row" sweep: pick
/// the next key in `keys_order` that is a member of `flagged`,
/// starting after `current` and wrapping. `None` when none match.
///
/// The `Shift-F` jump-to-failing-CI key is this motion over a
/// membership set built from the CI signal (not agent state), so the
/// sweep is shared with [`next_asking_workspace`] via the private
/// `next_matching_workspace` helper.
pub fn next_flagged_workspace(
    flagged: &std::collections::HashSet<SessionKey>,
    keys_order: &[SessionKey],
    current: Option<&SessionKey>,
) -> Option<SessionKey> {
    next_matching_workspace(keys_order, current, |k| flagged.contains(k))
}

/// The shared "next row satisfying `matches`, after `current`, wrapping"
/// sweep. Returns `None` when `keys_order` is empty or nothing matches.
fn next_matching_workspace(
    keys_order: &[SessionKey],
    current: Option<&SessionKey>,
    matches: impl Fn(&SessionKey) -> bool,
) -> Option<SessionKey> {
    if keys_order.is_empty() {
        return None;
    }
    let start_idx = current
        .and_then(|c| keys_order.iter().position(|k| k == c))
        .map(|i| i + 1)
        .unwrap_or(0);
    for offset in 0..keys_order.len() {
        let idx = (start_idx + offset) % keys_order.len();
        let key = &keys_order[idx];
        if matches(key) {
            return Some(key.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentState::{Done, Idle, InputNeeded, Working};
    use lazybox_core::WorkspaceKey;
    use std::collections::HashSet;

    const EXITED: AgentState = AgentState::Exited { code: Some(1) };
    const ALL: [AgentState; 5] = [Working, InputNeeded, Idle, Done, EXITED];

    fn ws_key(n: u32) -> SessionKey {
        SessionKey::from(&WorkspaceKey::new(format!("owner/repo#{n}")))
    }

    fn sample_workspace(n: u32) -> Workspace {
        Workspace::empty(
            WorkspaceKey::new(format!("owner/repo#{n}")),
            "main",
            chrono::Utc::now(),
        )
    }

    fn state_of(states: &HashMap<SessionKey, AgentState>, key: &SessionKey) -> Option<AgentState> {
        states.get(key).copied()
    }

    // ── apply_agent_state: the verbatim mirror ────────────────────

    #[test]
    fn first_reading_stores_the_state_and_reports_the_edge() {
        let mut states = HashMap::new();
        let ch = apply_agent_state(&mut states, &ws_key(1), InputNeeded);
        assert_eq!(state_of(&states, &ws_key(1)), Some(InputNeeded));
        assert!(ch.asking_changed && ch.now_asking && !ch.now_done);
    }

    #[test]
    fn repeat_reading_is_a_silent_no_op() {
        // The daemon re-emits the same state on every output chunk. A
        // repeat must not re-alert or it would spam the OS notification
        // center every second a prompt is on screen.
        let mut states = HashMap::new();
        apply_agent_state(&mut states, &ws_key(1), InputNeeded);
        let ch = apply_agent_state(&mut states, &ws_key(1), InputNeeded);
        assert_eq!(ch, StateChange::default());
        assert_eq!(state_of(&states, &ws_key(1)), Some(InputNeeded));
    }

    #[test]
    fn leaving_asking_flips_asking_changed_without_alerting() {
        let mut states = HashMap::new();
        apply_agent_state(&mut states, &ws_key(1), InputNeeded);
        let ch = apply_agent_state(&mut states, &ws_key(1), Working);
        assert!(ch.asking_changed && !ch.now_asking);
        assert_eq!(state_of(&states, &ws_key(1)), Some(Working));
    }

    #[test]
    fn reaching_done_reports_the_done_edge_once() {
        let mut states = HashMap::new();
        apply_agent_state(&mut states, &ws_key(1), Working);
        let ch = apply_agent_state(&mut states, &ws_key(1), Done);
        assert!(ch.now_done && !ch.now_asking);
        // A repeat Done must not re-alert.
        let again = apply_agent_state(&mut states, &ws_key(1), Done);
        assert_eq!(again, StateChange::default());
    }

    /// The client mirrors the daemon's latest validated state and does
    /// not re-stick `Done` itself. The daemon owns end-of-turn
    /// stickiness — it never broadcasts `Done → Idle` — so under normal
    /// operation `Done` holds. But if an `Idle` does reach us (a dropped
    /// intermediate broadcast, then the daemon's later state), mirroring
    /// heals to it rather than pinning a stale `✓` forever. Either way
    /// the single slot stays coherent: never a contradiction, never the
    /// "empty in all three sets = no status" blank of the old design.
    #[test]
    fn client_mirrors_daemon_and_self_heals() {
        let mut states = HashMap::new();
        let k = ws_key(1);
        apply_agent_state(&mut states, &k, Working);
        apply_agent_state(&mut states, &k, Done);
        assert_eq!(state_of(&states, &k), Some(Done));
        let ch = apply_agent_state(&mut states, &k, Idle);
        assert_eq!(state_of(&states, &k), Some(Idle), "heals to the latest");
        assert!(!ch.now_asking && !ch.now_done);
    }

    /// A `Done → Working → Idle` sequence leaves exactly one coherent
    /// state at every step — never a contradiction or a blank.
    #[test]
    fn done_then_working_then_idle_stays_coherent() {
        let mut states = HashMap::new();
        let k = ws_key(1);
        let ws = sample_workspace(1);
        let mut seen = Vec::new();
        for reading in [Done, Working, Idle] {
            apply_agent_state(&mut states, &k, reading);
            let lit = [
                workspace_is_asking(&ws, &states),
                workspace_is_working(&ws, &states),
                workspace_is_done(&ws, &states),
            ]
            .iter()
            .filter(|p| **p)
            .count();
            assert!(lit <= 1, "at most one pill lit (reading {reading:?})");
            seen.push(state_of(&states, &k));
        }
        assert_eq!(seen, vec![Some(Done), Some(Working), Some(Idle)]);
    }

    /// Table over every `(prior, incoming)` pair: the stored result is
    /// always the incoming state (the client mirrors the daemon), a
    /// repeat is a silent no-op, and no state lights more than one pill.
    #[test]
    fn every_reading_mirrors_and_leaves_at_most_one_pill() {
        let ws = sample_workspace(1);
        for prior in ALL {
            for incoming in ALL {
                let mut states = HashMap::new();
                states.insert(ws_key(1), prior);
                let ch = apply_agent_state(&mut states, &ws_key(1), incoming);
                assert_eq!(state_of(&states, &ws_key(1)), Some(incoming));
                if prior == incoming {
                    assert_eq!(ch, StateChange::default(), "{prior:?} repeat is a no-op");
                }

                let lit = [
                    workspace_is_asking(&ws, &states),
                    workspace_is_working(&ws, &states),
                    workspace_is_done(&ws, &states),
                    workspace_is_exited(&ws, &states),
                ]
                .iter()
                .filter(|p| **p)
                .count();
                assert!(lit <= 1, "{prior:?} → {incoming:?}: {lit} pills lit");
            }
        }
    }

    #[test]
    fn keys_are_independent() {
        let mut states = HashMap::new();
        apply_agent_state(&mut states, &ws_key(1), InputNeeded);
        apply_agent_state(&mut states, &ws_key(2), Working);
        assert_eq!(state_of(&states, &ws_key(1)), Some(InputNeeded));
        assert_eq!(state_of(&states, &ws_key(2)), Some(Working));
        apply_agent_state(&mut states, &ws_key(1), Done);
        assert_eq!(state_of(&states, &ws_key(1)), Some(Done));
        assert_eq!(
            state_of(&states, &ws_key(2)),
            Some(Working),
            "key 2 untouched"
        );
    }

    // ── projections ───────────────────────────────────────────────

    #[test]
    fn projections_read_the_map() {
        let mut states = HashMap::new();
        let ws = sample_workspace(1);
        assert!(!workspace_is_asking(&ws, &states));
        assert!(!workspace_is_working(&ws, &states));
        assert!(!workspace_is_done(&ws, &states));
        assert_eq!(workspace_agent_state(&ws, &states), None);

        states.insert(SessionKey::from(&ws.key), Working);
        assert!(workspace_is_working(&ws, &states));
        assert_eq!(workspace_agent_state(&ws, &states), Some(Working));
    }

    // ── rebadge_attention ─────────────────────────────────────────

    #[test]
    fn rebadge_moves_the_state_to_the_new_session() {
        let mut states = HashMap::new();
        states.insert(ws_key(1), InputNeeded);
        assert_eq!(
            rebadge_attention(&mut states, &ws_key(1), &ws_key(2)),
            Some(InputNeeded),
        );
        assert_eq!(state_of(&states, &ws_key(1)), None, "old key dropped");
        assert_eq!(state_of(&states, &ws_key(2)), Some(InputNeeded));
    }

    #[test]
    fn rebadge_of_an_untracked_key_is_a_noop() {
        let mut states = HashMap::new();
        states.insert(ws_key(3), Working);
        assert_eq!(rebadge_attention(&mut states, &ws_key(1), &ws_key(2)), None);
        assert_eq!(state_of(&states, &ws_key(2)), None);
        assert_eq!(
            state_of(&states, &ws_key(3)),
            Some(Working),
            "unrelated key untouched"
        );
    }

    // ── next_asking_workspace ─────────────────────────────────────

    #[test]
    fn next_returns_none_when_nothing_asking() {
        let mut states = HashMap::new();
        states.insert(ws_key(1), Working);
        states.insert(ws_key(2), Done);
        assert_eq!(
            next_asking_workspace(&states, &[ws_key(1), ws_key(2)], None),
            None,
        );
    }

    #[test]
    fn next_returns_none_when_keys_order_is_empty() {
        let mut states = HashMap::new();
        states.insert(ws_key(1), InputNeeded);
        assert_eq!(next_asking_workspace(&states, &[], None), None);
    }

    #[test]
    fn next_skips_past_current_then_wraps() {
        let mut states = HashMap::new();
        states.insert(ws_key(1), InputNeeded);
        states.insert(ws_key(2), InputNeeded);
        let keys = vec![ws_key(1), ws_key(2)];
        assert_eq!(
            next_asking_workspace(&states, &keys, Some(&ws_key(1))),
            Some(ws_key(2)),
        );
        assert_eq!(
            next_asking_workspace(&states, &keys, Some(&ws_key(2))),
            Some(ws_key(1)),
        );
    }

    #[test]
    fn next_from_none_starts_at_first_asking() {
        let mut states = HashMap::new();
        states.insert(ws_key(2), InputNeeded);
        let keys = vec![ws_key(1), ws_key(2)];
        assert_eq!(next_asking_workspace(&states, &keys, None), Some(ws_key(2)));
    }

    // ── next_flagged_workspace (shared sweep, CI membership) ───────

    #[test]
    fn flagged_sweep_skips_past_current_and_wraps() {
        let mut set = HashSet::new();
        set.insert(ws_key(1));
        set.insert(ws_key(3));
        let keys = vec![ws_key(1), ws_key(2), ws_key(3)];
        assert_eq!(
            next_flagged_workspace(&set, &keys, Some(&ws_key(1))),
            Some(ws_key(3)),
        );
        assert_eq!(
            next_flagged_workspace(&set, &keys, Some(&ws_key(3))),
            Some(ws_key(1)),
        );
    }

    #[test]
    fn flagged_sweep_returns_none_when_set_empty() {
        let set = HashSet::new();
        assert_eq!(
            next_flagged_workspace(&set, &[ws_key(1), ws_key(2)], None),
            None,
        );
    }
}
