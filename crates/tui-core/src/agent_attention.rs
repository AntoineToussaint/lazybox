//! Pure logic for the "agent needs my input" signal.
//!
//! When the daemon detects a Claude/Codex/Cursor session has flipped
//! into `Asking` (yes/no prompt, tool approval), the user needs to
//! know — even when pilot isn't the focused window.
//!
//! ## Why this state lives in the sidebar, not on `Workspace`
//!
//! The earlier design mutated `workspace.sessions[i].state` whenever
//! an `Event::AgentState` arrived. That worked until the next poll
//! cycle re-broadcast `WorkspaceUpserted` with the workspace freshly
//! loaded from the store — and the store doesn't carry transient
//! Asking state, so every poll silently clobbered the badge.
//! Symptom: the `?` indicator would flash on for ~1 second after
//! Claude prompted and then disappear at the next minute boundary.
//!
//! Fix: keep agent state in a sidebar-local `HashSet<SessionKey>`,
//! independent of the workspace data. Polling broadcasts can't
//! touch it. The set is fully reconstructed from `Event::AgentState`
//! deltas — the daemon is still the source of truth.
//!
//! This module owns the pure state-transition math:
//!
//! - [`apply_agent_state`] adds/removes a workspace key from the
//!   asking-set and reports the [`AttentionTransition`].
//! - [`apply_working_state`] is the companion for the third state —
//!   it tracks the disjoint working-set that drives the side panel's
//!   animated "working" spinner.
//! - [`next_asking_workspace`] picks the next asking workspace —
//!   used by the `!` jump-to-asking key.
//!
//! The three [`AgentState`]s (`Working` / `InputNeeded` / `Idle`)
//! share a single per-session UI slot, so the asking-set and
//! working-set are kept mutually exclusive: every event applies to
//! both, landing the key in at most one.

use pilot_core::{SessionKey, Workspace};
use pilot_ipc::AgentState;
use std::collections::HashSet;

/// How the workspace's overall "needs my input" status changed as a
/// result of applying an `AgentState` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionTransition {
    /// Workspace crossed not-asking → Asking. The caller should
    /// fire a one-shot desktop notification.
    NowAsking,
    /// Workspace crossed Asking → not-asking. The caller might
    /// dismiss any notification UI (no-op for now — we don't
    /// track notif IDs).
    NoLongerAsking,
    /// Repeat broadcast or unchanged state. The caller should NOT
    /// re-notify (otherwise the daemon's per-chunk emit would spam
    /// the notification center).
    NoChange,
}

/// Apply an `Event::AgentState` to the sidebar's asking-set.
///
/// Returns the [`AttentionTransition`] so the caller can decide
/// whether this warrants a desktop notification. The set is the
/// SINGLE source of truth for "is this workspace asking?" — no
/// other field in the workspace data carries that meaning.
pub fn apply_agent_state(
    asking_set: &mut HashSet<SessionKey>,
    workspace_key: &SessionKey,
    incoming: AgentState,
) -> AttentionTransition {
    let was_asking = asking_set.contains(workspace_key);
    let is_asking = matches!(incoming, AgentState::InputNeeded);
    match (was_asking, is_asking) {
        (false, true) => {
            asking_set.insert(workspace_key.clone());
            AttentionTransition::NowAsking
        }
        (true, false) => {
            asking_set.remove(workspace_key);
            AttentionTransition::NoLongerAsking
        }
        _ => AttentionTransition::NoChange,
    }
}

/// Apply an `Event::AgentState` to the sidebar's working-set — the
/// companion to [`apply_agent_state`] for the third state.
///
/// The working-set and asking-set are kept disjoint by construction:
/// each event carries exactly one [`AgentState`], so applying it to
/// both sets adds the key to at most one of them and removes it from
/// the other. The side panel renders both in a single per-session
/// slot (working spinner vs `?` pill), so they must never both hold
/// the same key.
///
/// Returns `true` when membership changed (caller should re-render),
/// `false` on a repeat/no-op broadcast. Unlike asking, entering the
/// working state warrants no desktop notification — it's not
/// something the user needs to act on — so this returns a plain
/// bool rather than an [`AttentionTransition`].
pub fn apply_working_state(
    working_set: &mut HashSet<SessionKey>,
    workspace_key: &SessionKey,
    incoming: AgentState,
) -> bool {
    let was_working = working_set.contains(workspace_key);
    let is_working = matches!(incoming, AgentState::Working);
    match (was_working, is_working) {
        (false, true) => {
            working_set.insert(workspace_key.clone());
            true
        }
        (true, false) => {
            working_set.remove(workspace_key);
            true
        }
        _ => false,
    }
}

/// True iff the workspace's key is in the working-set. Mirror of
/// [`workspace_is_asking`] for the "actively working" signal.
pub fn workspace_is_working(workspace: &Workspace, working_set: &HashSet<SessionKey>) -> bool {
    let key = SessionKey::from(&workspace.key);
    working_set.contains(&key)
}

/// True iff the workspace's key is in the asking-set. Single
/// source of truth for the workspace-level needs-input check
/// (sidebar header counter, row pill, `!` jump predicate).
pub fn workspace_is_asking(workspace: &Workspace, asking_set: &HashSet<SessionKey>) -> bool {
    let key = SessionKey::from(&workspace.key);
    asking_set.contains(&key)
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
    asking_set: &HashSet<SessionKey>,
    keys_order: &[SessionKey],
    current: Option<&SessionKey>,
) -> Option<SessionKey> {
    if keys_order.is_empty() || asking_set.is_empty() {
        return None;
    }
    let start_idx = current
        .and_then(|c| keys_order.iter().position(|k| k == c))
        .map(|i| i + 1)
        .unwrap_or(0);
    for offset in 0..keys_order.len() {
        let idx = (start_idx + offset) % keys_order.len();
        let key = &keys_order[idx];
        if asking_set.contains(key) {
            return Some(key.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilot_core::WorkspaceKey;

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

    // ── apply_agent_state ─────────────────────────────────────────

    #[test]
    fn first_asking_for_a_key_reports_now_asking() {
        let mut set = HashSet::new();
        let t = apply_agent_state(&mut set, &ws_key(1), AgentState::InputNeeded);
        assert_eq!(t, AttentionTransition::NowAsking);
        assert!(set.contains(&ws_key(1)));
    }

    #[test]
    fn asking_to_active_reports_no_longer_asking() {
        let mut set = HashSet::new();
        set.insert(ws_key(1));
        let t = apply_agent_state(&mut set, &ws_key(1), AgentState::Idle);
        assert_eq!(t, AttentionTransition::NoLongerAsking);
        assert!(!set.contains(&ws_key(1)));
    }

    #[test]
    fn repeat_asking_broadcast_is_no_change() {
        // Daemon re-emits the same state on every output chunk.
        // Must not re-notify or we'd spam the OS notification
        // center every second a Claude prompt is on screen.
        let mut set = HashSet::new();
        apply_agent_state(&mut set, &ws_key(1), AgentState::InputNeeded);
        let t = apply_agent_state(&mut set, &ws_key(1), AgentState::InputNeeded);
        assert_eq!(t, AttentionTransition::NoChange);
        assert!(set.contains(&ws_key(1)));
    }

    #[test]
    fn active_to_active_is_no_change() {
        let mut set = HashSet::new();
        let t = apply_agent_state(&mut set, &ws_key(1), AgentState::Idle);
        assert_eq!(t, AttentionTransition::NoChange);
        assert!(set.is_empty());
    }

    #[test]
    fn keys_are_independent() {
        // Asking on workspace A must not affect workspace B's state.
        let mut set = HashSet::new();
        apply_agent_state(&mut set, &ws_key(1), AgentState::InputNeeded);
        apply_agent_state(&mut set, &ws_key(2), AgentState::InputNeeded);
        assert!(set.contains(&ws_key(1)));
        assert!(set.contains(&ws_key(2)));
        apply_agent_state(&mut set, &ws_key(1), AgentState::Idle);
        assert!(!set.contains(&ws_key(1)));
        assert!(set.contains(&ws_key(2)));
    }

    // ── workspace_is_asking ───────────────────────────────────────

    #[test]
    fn workspace_is_asking_reads_set() {
        let mut set = HashSet::new();
        let ws = sample_workspace(1);
        assert!(!workspace_is_asking(&ws, &set));
        set.insert(SessionKey::from(&ws.key));
        assert!(workspace_is_asking(&ws, &set));
    }

    // ── apply_working_state / workspace_is_working ────────────────

    #[test]
    fn first_working_for_a_key_changes_set() {
        let mut set = HashSet::new();
        assert!(apply_working_state(
            &mut set,
            &ws_key(1),
            AgentState::Working
        ));
        assert!(set.contains(&ws_key(1)));
    }

    #[test]
    fn working_to_idle_clears_set() {
        let mut set = HashSet::new();
        set.insert(ws_key(1));
        assert!(apply_working_state(&mut set, &ws_key(1), AgentState::Idle));
        assert!(!set.contains(&ws_key(1)));
    }

    #[test]
    fn repeat_working_broadcast_is_no_change() {
        // The daemon re-emits Working on every output chunk while the
        // agent streams. Membership doesn't change → no re-render.
        let mut set = HashSet::new();
        apply_working_state(&mut set, &ws_key(1), AgentState::Working);
        assert!(!apply_working_state(
            &mut set,
            &ws_key(1),
            AgentState::Working
        ));
        assert!(set.contains(&ws_key(1)));
    }

    #[test]
    fn input_needed_does_not_mark_working() {
        // InputNeeded clears working (and apply_agent_state would set
        // asking) — the two slots stay disjoint.
        let mut working = HashSet::new();
        working.insert(ws_key(1));
        assert!(apply_working_state(
            &mut working,
            &ws_key(1),
            AgentState::InputNeeded
        ));
        assert!(!working.contains(&ws_key(1)));
    }

    #[test]
    fn working_and_asking_sets_stay_disjoint() {
        // Drive a full Idle → Working → InputNeeded → Idle cycle
        // through both appliers and assert the key is never in both.
        let mut asking = HashSet::new();
        let mut working = HashSet::new();
        let k = ws_key(1);
        for state in [
            AgentState::Working,
            AgentState::InputNeeded,
            AgentState::Idle,
            AgentState::Working,
        ] {
            apply_agent_state(&mut asking, &k, state);
            apply_working_state(&mut working, &k, state);
            assert!(
                !(asking.contains(&k) && working.contains(&k)),
                "key must never be both asking and working (state {state:?})",
            );
        }
    }

    #[test]
    fn workspace_is_working_reads_set() {
        let mut set = HashSet::new();
        let ws = sample_workspace(1);
        assert!(!workspace_is_working(&ws, &set));
        set.insert(SessionKey::from(&ws.key));
        assert!(workspace_is_working(&ws, &set));
    }

    // ── next_asking_workspace ─────────────────────────────────────

    #[test]
    fn next_returns_none_when_set_is_empty() {
        let set = HashSet::new();
        assert_eq!(
            next_asking_workspace(&set, &[ws_key(1), ws_key(2)], None),
            None,
        );
    }

    #[test]
    fn next_returns_none_when_keys_order_is_empty() {
        let mut set = HashSet::new();
        set.insert(ws_key(1));
        assert_eq!(next_asking_workspace(&set, &[], None), None);
    }

    #[test]
    fn next_returns_only_asking_workspace() {
        let mut set = HashSet::new();
        set.insert(ws_key(1));
        assert_eq!(
            next_asking_workspace(&set, &[ws_key(1)], None),
            Some(ws_key(1)),
        );
    }

    #[test]
    fn next_skips_past_current_to_next_asking() {
        // Two asking workspaces. With cursor on #1, `!` jumps to #2.
        let mut set = HashSet::new();
        set.insert(ws_key(1));
        set.insert(ws_key(2));
        let keys = vec![ws_key(1), ws_key(2)];
        assert_eq!(
            next_asking_workspace(&set, &keys, Some(&ws_key(1))),
            Some(ws_key(2)),
        );
    }

    #[test]
    fn next_wraps_around_to_first_asking() {
        // Three workspaces — only #1 asking. From cursor on #2,
        // the sweep wraps back to #1.
        let mut set = HashSet::new();
        set.insert(ws_key(1));
        let keys = vec![ws_key(1), ws_key(2), ws_key(3)];
        assert_eq!(
            next_asking_workspace(&set, &keys, Some(&ws_key(2))),
            Some(ws_key(1)),
        );
    }

    #[test]
    fn next_from_none_starts_at_first_key() {
        // No current cursor — pick the first asking workspace in
        // `keys_order`. (#1 not asking, #2 is.)
        let mut set = HashSet::new();
        set.insert(ws_key(2));
        let keys = vec![ws_key(1), ws_key(2)];
        assert_eq!(next_asking_workspace(&set, &keys, None), Some(ws_key(2)));
    }
}
