//! The per-session agent status the sidebar tracks, and the pure
//! projections that turn it into UI signals.
//!
//! ## One state per session, not three parallel sets
//!
//! An agent is in exactly one [`AgentState`] at a time (`Working` /
//! `InputNeeded` / `Idle` / `Done`), so its status lives here as a
//! single `HashMap<SessionKey, AgentState>` — one value per workspace.
//! The four states share one per-session UI slot (working spinner vs
//! `?` pill vs `✓` done mark), and a single value makes the illegal
//! combinations the earlier three-`HashSet` design allowed —
//! "all-empty = no status", "the same key in two sets at once",
//! "any ordering, including `Done → Idle`, passes" — simply
//! unrepresentable (issue #327).
//!
//! Every incoming reading folds in through [`apply_agent_state`], which
//! routes it through the ONE transition validator the daemon already
//! commits its own state through
//! ([`lazybox_agents::AgentStateMachine::transition`]). Sharing that
//! function — rather than re-deriving the rules here — is what keeps the
//! client and daemon from drifting: the client only ever stores a legal
//! successor of its prior value, so a stray `Idle` can't blank a `Done`
//! and no reading can bounce the status through a contradiction. The
//! daemon remains the source of truth (it damps the ambiguous PTY
//! readings before they're ever broadcast); this map is a coherent
//! projection of the states it emits.
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
//! Fix: keep agent state in this sidebar-local map, independent of the
//! workspace data. Polling broadcasts can't touch it. The map is fully
//! reconstructed from `Event::AgentState` deltas — the daemon is still
//! the source of truth.

use lazybox_agents::AgentStateMachine;
use lazybox_core::{SessionKey, Workspace};
use lazybox_ipc::AgentState;
use std::collections::HashMap;

/// How a workspace's attention signals changed as a result of folding
/// one `AgentState` reading into the per-session state. Every field is
/// derived from the single before/after value, so the flags can never
/// contradict each other the way three independently-mutated sets could.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StateChange {
    /// The stored state moved to a new value — the caller redraws.
    /// `false` on a repeat/no-op reading or a rejected illegal edge.
    pub changed: bool,
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

/// Fold an `Event::AgentState` reading for `workspace_key` into the
/// per-session state map, returning how the workspace's attention
/// signals changed.
///
/// The candidate is routed through
/// [`AgentStateMachine::transition`] — the same validator the daemon
/// commits through — so the map only ever holds a legal successor of
/// its prior value. A no-op (repeat reading) or a structurally
/// forbidden edge (`Done` staying sticky against a bare `Idle`) leaves
/// the state untouched and reports [`StateChange::default`] (all-false).
pub fn apply_agent_state(
    states: &mut HashMap<SessionKey, AgentState>,
    workspace_key: &SessionKey,
    incoming: AgentState,
) -> StateChange {
    let prev = states.get(workspace_key).copied();
    let Some(next) = AgentStateMachine::transition(prev, incoming) else {
        return StateChange::default();
    };
    states.insert(workspace_key.clone(), next);
    // `transition` returns `None` on a self-loop, so a `Some(next)` that
    // is `InputNeeded` / `Done` is always a rising edge from a different
    // prior state.
    let was_asking = prev == Some(AgentState::InputNeeded);
    let is_asking = next == AgentState::InputNeeded;
    StateChange {
        changed: true,
        asking_changed: was_asking != is_asking,
        now_asking: is_asking,
        now_done: next == AgentState::Done,
    }
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
/// sweep is shared with [`next_asking_workspace`] via
/// [`next_matching_workspace`].
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

    const ALL: [AgentState; 4] = [Working, InputNeeded, Idle, Done];

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

    // ── apply_agent_state: the transition validator ───────────────

    #[test]
    fn first_reading_stores_the_state_and_reports_the_edge() {
        let mut states = HashMap::new();
        let ch = apply_agent_state(&mut states, &ws_key(1), InputNeeded);
        assert_eq!(state_of(&states, &ws_key(1)), Some(InputNeeded));
        assert!(ch.changed && ch.asking_changed && ch.now_asking && !ch.now_done);
    }

    #[test]
    fn repeat_reading_is_a_silent_no_op() {
        // The daemon re-emits the same state on every output chunk. A
        // self-loop must not re-alert or it would spam the OS
        // notification center every second a prompt is on screen.
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
        assert!(ch.changed && ch.asking_changed && !ch.now_asking);
        assert_eq!(state_of(&states, &ws_key(1)), Some(Working));
    }

    #[test]
    fn reaching_done_reports_the_done_edge_once() {
        let mut states = HashMap::new();
        apply_agent_state(&mut states, &ws_key(1), Working);
        let ch = apply_agent_state(&mut states, &ws_key(1), Done);
        assert!(ch.now_done && !ch.now_asking);
        // A repeat Done is a self-loop: no second alert.
        let again = apply_agent_state(&mut states, &ws_key(1), Done);
        assert!(!again.changed && !again.now_done);
    }

    /// The reported bug: an agent reaches `Done`, then a stray `Idle`
    /// reading arrives. The old three-set design cleared the done-set,
    /// leaving every set empty — "no status at all". The single state
    /// routed through the shared validator keeps `Done` sticky against
    /// a bare `Idle`, so the status can never blank.
    #[test]
    fn stray_idle_after_done_never_blanks_the_status() {
        let mut states = HashMap::new();
        apply_agent_state(&mut states, &ws_key(1), Working);
        apply_agent_state(&mut states, &ws_key(1), Done);
        let ch = apply_agent_state(&mut states, &ws_key(1), Idle);
        assert_eq!(ch, StateChange::default(), "Done → Idle is rejected");
        assert_eq!(state_of(&states, &ws_key(1)), Some(Done));
        let ws = sample_workspace(1);
        assert!(workspace_is_done(&ws, &states));
        assert!(!workspace_is_asking(&ws, &states));
        assert!(!workspace_is_working(&ws, &states));
    }

    /// The full reported sequence — `Done`, a `Working` reading, then
    /// `Idle` — must leave exactly one coherent state at every step,
    /// never a contradiction or a blank. (A `Working` reaching the
    /// client is already daemon-vetted as real progress/resume; the
    /// stray-`Working`-vs-`Done` damping lives on the daemon, which owns
    /// the affirmative-vs-ambiguous signal the client doesn't carry.)
    #[test]
    fn done_then_working_then_idle_stays_coherent() {
        let mut states = HashMap::new();
        let k = ws_key(1);
        let ws = sample_workspace(1);
        let mut seen = Vec::new();
        for reading in [Done, Working, Idle] {
            apply_agent_state(&mut states, &k, reading);
            let pills = [
                workspace_is_asking(&ws, &states),
                workspace_is_working(&ws, &states),
                workspace_is_done(&ws, &states),
            ];
            let lit = pills.iter().filter(|p| **p).count();
            assert!(lit <= 1, "at most one pill lit (reading {reading:?})");
            seen.push(state_of(&states, &k));
        }
        // Done held against the stray Working? No — a Working reading is
        // legitimate resume at the client boundary, so it advances; the
        // point is that each step is a single defined state.
        assert_eq!(seen, vec![Some(Done), Some(Working), Some(Idle)]);
    }

    /// Table test over every `(prior, incoming)` pair: the stored result
    /// is always a legal successor, exactly one projection is ever lit,
    /// and the only prior→incoming that fails to advance are self-loops
    /// and the forbidden `Done → Idle` edge.
    #[test]
    fn every_transition_is_defined_and_leaves_at_most_one_pill() {
        let ws = sample_workspace(1);
        for prior in ALL {
            for incoming in ALL {
                let mut states = HashMap::new();
                states.insert(ws_key(1), prior);
                let ch = apply_agent_state(&mut states, &ws_key(1), incoming);
                let now = state_of(&states, &ws_key(1)).unwrap();

                let forbidden = (prior, incoming) == (Done, Idle);
                let self_loop = prior == incoming;
                if self_loop || forbidden {
                    assert!(!ch.changed, "{prior:?} → {incoming:?} must not commit");
                    assert_eq!(now, prior, "{prior:?} → {incoming:?} holds prior");
                } else {
                    assert!(ch.changed, "{prior:?} → {incoming:?} must commit");
                    assert_eq!(now, incoming, "{prior:?} → {incoming:?} advances");
                }

                let lit = [
                    workspace_is_asking(&ws, &states),
                    workspace_is_working(&ws, &states),
                    workspace_is_done(&ws, &states),
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
