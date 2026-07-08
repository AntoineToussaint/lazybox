//! The agent lifecycle as an explicit state machine.
//!
//! The four [`AgentState`] variants are the machine's states; a detected
//! signal — from the PTY screen-scraper ([`crate::detect`]) or a lifecycle
//! hook ([`crate::hook`]) — is an *input* the machine folds into the
//! current state under a fixed table of allowed transitions. Consolidating
//! every state change behind [`AgentStateMachine::transition`] means the
//! broadcast (and therefore displayed) status is always the result of a
//! legal move from the prior state, never an independent per-poll guess
//! that can jump between contradictory readings.
//!
//! Mapping the lifecycle onto the existing wire vocabulary:
//! - `Idle` — freshly launched / at a ready composer, no work run (covers "starting").
//! - `Working` — actively producing output or running a tool ("running").
//! - `InputNeeded` — parked on a structural prompt ("awaiting input").
//! - `Done` — finished a turn (hook-exclusive; "done").
//!
//! The transition table is deliberately permissive: an agent can go busy,
//! ask, or fall quiet from almost anywhere, because detection legitimately
//! observes all of those edges (a permission gate mid-work, a Ctrl-C that
//! ends a turn without a `Stop` hook, …). The one structural rule is that
//! `Done` is **sticky** against a bare `Idle` — see
//! [`AgentStateMachine::transition`]. Beyond the table, the machine damps
//! the two *ambiguous* edges the PTY detector produces when Claude's status
//! line drops for a frame (see [`AgentStateMachine::on_reading`]); those are
//! the flaps this machine exists to eliminate.

use std::time::{Duration, Instant};

use lazybox_ipc::AgentState;

/// Hysteresis window for the edge that LEAVES `InputNeeded`. Claude's
/// status-bar ticker can scroll a live prompt out of the detect window for a
/// single chunk, momentarily reading as `Idle` even though Claude is still
/// waiting; without damping the `?` pill flickers off and back. Longer than
/// [`WORKING_HYSTERESIS`] because a parked prompt can sit quiet far longer
/// than a spinner drops between frames.
pub const INPUT_NEEDED_HYSTERESIS: Duration = Duration::from_secs(8);

/// Hysteresis window for the edge that LEAVES `Working`. Claude paints its
/// live status line (spinner + token counter + `esc to interrupt`) only
/// while busy; that line vanishes for a chunk whenever the agent prints
/// output between spinner frames or the terminal does a full repaint,
/// leaving the detector with an ambiguous `Idle`. Shorter than the
/// `InputNeeded` window: the spinner reappears within a frame or two.
pub const WORKING_HYSTERESIS: Duration = Duration::from_secs(5);

/// One detection reading offered to the machine, tagged with the evidence
/// quality the hysteresis needs.
#[derive(Debug, Clone, Copy)]
pub struct Reading {
    /// The state this reading implies.
    pub state: AgentState,
    /// Whether the detector is *affirmatively* sure — a live `Working`
    /// status line, or an idle composer the readiness probe recognized — as
    /// opposed to the ambiguous fall-through a dropped status-line frame
    /// produces. A clear reading is honored immediately; an ambiguous one is
    /// damped within the hysteresis window.
    pub clear: bool,
}

/// Whether the machine permits a move from `from` to a *different* state
/// `to`. The lifecycle is intentionally near-complete — see the module
/// docs — with a single forbidden edge:
///
/// ```text
/// Done ─╳→ Idle
/// ```
///
/// `Done` is set by the `Stop` hook when the agent finishes its turn (#80).
/// The PTY detector keeps running and contributes a "confident idle"
/// (composer drawn), and `SessionStart`/`SessionEnd` hooks also map to
/// `Idle` — any of which would demote a freshly-set `Done` back to `Idle`
/// and drop the "completed, take a look" alert the instant it fired. So an
/// `Idle` reading never overwrites `Done`: the agent stays `Done` until it
/// works again (`Working`) or is asked for input (`InputNeeded`).
///
/// Only meaningful for `from != to`; a self-loop is not a transition (see
/// [`AgentStateMachine::transition`]).
pub fn transition_allowed(from: AgentState, to: AgentState) -> bool {
    !matches!((from, to), (AgentState::Done, AgentState::Idle))
}

/// One terminal's lifecycle state machine.
///
/// The current [`AgentState`] lives in the daemon's shared cache (one entry
/// per terminal, read by every status consumer); this type owns the
/// *transition policy* and the per-terminal timing anchors the hysteresis
/// needs. All three writers route their candidate through
/// [`AgentStateMachine::transition`] so the cache only ever holds a legal
/// successor of its prior value; the PTY pump additionally runs its noisy
/// readings through [`AgentStateMachine::on_reading`] first.
#[derive(Debug)]
pub struct AgentStateMachine {
    last_input_needed_at: Option<Instant>,
    last_working_at: Option<Instant>,
    input_hysteresis: Duration,
    working_hysteresis: Duration,
}

impl Default for AgentStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentStateMachine {
    /// A machine with the production hysteresis windows.
    pub fn new() -> Self {
        Self::with_hysteresis(INPUT_NEEDED_HYSTERESIS, WORKING_HYSTERESIS)
    }

    /// A machine with explicit hysteresis windows (tests inject short ones).
    pub fn with_hysteresis(input_hysteresis: Duration, working_hysteresis: Duration) -> Self {
        Self {
            last_input_needed_at: None,
            last_working_at: None,
            input_hysteresis,
            working_hysteresis,
        }
    }

    /// The structural transition table. Given the terminal's current state
    /// (`None` when it has never reported one) and a candidate `to`, returns
    /// `Some(to)` when the move is a legal, state-changing transition, or
    /// `None` when it is a no-op (`from == to`) or a forbidden edge
    /// ([`transition_allowed`]). This is the single choke point for every
    /// state change — the PTY pump, hook ingest, and the optimistic answer
    /// flip all commit through it.
    pub fn transition(from: Option<AgentState>, to: AgentState) -> Option<AgentState> {
        match from {
            Some(current) if current == to => None,
            Some(current) if !transition_allowed(current, to) => None,
            _ => Some(to),
        }
    }

    /// Fold a PTY detection `reading` in at time `now`, given the terminal's
    /// current cached `state`. Returns `Some(new)` on a committed
    /// transition, `None` when the reading is a no-op, a forbidden edge, or
    /// a damped ambiguous flap.
    ///
    /// The two damped edges are the flaps a dropped status-line frame
    /// produces: an ambiguous `Idle` while a prompt is genuinely still up
    /// (`InputNeeded` exit), and an ambiguous `Idle` between spinner frames
    /// (`Working` exit). A `clear` reading — a live status line, or a
    /// recognized idle composer — is the real end of that state and passes
    /// immediately; only the ambiguous fall-through is held within the
    /// window.
    pub fn on_reading(
        &mut self,
        state: Option<AgentState>,
        reading: Reading,
        now: Instant,
    ) -> Option<AgentState> {
        // Refresh the anchor on every busy / waiting reading, even ones that
        // dedupe or damp below, so the hysteresis measures time since the
        // signal was LAST seen — a transient frame that drops it still reads
        // as a recent-enough anchor.
        match reading.state {
            AgentState::InputNeeded => self.last_input_needed_at = Some(now),
            AgentState::Working => self.last_working_at = Some(now),
            _ => {}
        }
        if self.suppress_input_needed_exit(state, reading, now)
            || self.suppress_working_exit(state, reading, now)
        {
            return None;
        }
        Self::transition(state, reading.state)
    }

    /// Drop the `InputNeeded` anchor so the exit hysteresis can't hold a `?`
    /// the user just answered. Called when the daemon resets a terminal's
    /// detection buffer after an answer keystroke.
    pub fn reset_input_anchor(&mut self) {
        self.last_input_needed_at = None;
    }

    /// Whether to damp the edge leaving `InputNeeded`. Suppressed only when
    /// the new reading is the ambiguous fall-through (`!clear`) and the last
    /// `InputNeeded` reading is still within the window — a clear signal is
    /// honored immediately, so a wrong `InputNeeded` can't stick once Claude
    /// is visibly streaming or idle.
    fn suppress_input_needed_exit(
        &self,
        state: Option<AgentState>,
        reading: Reading,
        now: Instant,
    ) -> bool {
        state == Some(AgentState::InputNeeded)
            && reading.state != AgentState::InputNeeded
            && !reading.clear
            && self
                .last_input_needed_at
                .is_some_and(|t| now.duration_since(t) < self.input_hysteresis)
    }

    /// Whether to damp the ambiguous `Working → Idle` edge — a single frame
    /// where Claude's spinner is absent and the composer isn't yet drawn.
    /// An affirmative idle composer (`clear`) is the real end of turn and is
    /// honored immediately; a transition into a live dialog is never damped
    /// (it isn't an `Idle` reading).
    fn suppress_working_exit(
        &self,
        state: Option<AgentState>,
        reading: Reading,
        now: Instant,
    ) -> bool {
        state == Some(AgentState::Working)
            && reading.state == AgentState::Idle
            && !reading.clear
            && self
                .last_working_at
                .is_some_and(|t| now.duration_since(t) < self.working_hysteresis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentState::{Done, Idle, InputNeeded, Working};

    const ALL: [AgentState; 4] = [Working, InputNeeded, Idle, Done];

    #[test]
    fn done_to_idle_is_the_only_forbidden_transition() {
        for from in ALL {
            for to in ALL {
                if from == to {
                    continue;
                }
                let allowed = transition_allowed(from, to);
                if (from, to) == (Done, Idle) {
                    assert!(!allowed, "Done → Idle must be forbidden (sticky)");
                } else {
                    assert!(allowed, "{from:?} → {to:?} must be allowed");
                }
            }
        }
    }

    #[test]
    fn transition_commits_legal_moves_and_rejects_the_rest() {
        // A change to a legal state commits.
        assert_eq!(
            AgentStateMachine::transition(Some(Idle), Working),
            Some(Working)
        );
        assert_eq!(
            AgentStateMachine::transition(Some(Working), InputNeeded),
            Some(InputNeeded)
        );
        assert_eq!(
            AgentStateMachine::transition(Some(Working), Done),
            Some(Done)
        );
        // Done yields to real progress or a fresh prompt, but not to Idle.
        assert_eq!(
            AgentStateMachine::transition(Some(Done), Working),
            Some(Working)
        );
        assert_eq!(
            AgentStateMachine::transition(Some(Done), InputNeeded),
            Some(InputNeeded)
        );
        assert_eq!(AgentStateMachine::transition(Some(Done), Idle), None);
        // A self-loop is not a transition.
        for s in ALL {
            assert_eq!(
                AgentStateMachine::transition(Some(s), s),
                None,
                "{s:?} self-loop"
            );
        }
        // A never-seen terminal accepts any first reading.
        for s in ALL {
            assert_eq!(
                AgentStateMachine::transition(None, s),
                Some(s),
                "None → {s:?}"
            );
        }
    }

    fn clear(state: AgentState) -> Reading {
        Reading { state, clear: true }
    }
    fn ambiguous(state: AgentState) -> Reading {
        Reading {
            state,
            clear: false,
        }
    }

    #[test]
    fn ambiguous_input_needed_exit_is_damped_within_the_window() {
        let mut m =
            AgentStateMachine::with_hysteresis(Duration::from_secs(8), Duration::from_secs(5));
        let t0 = Instant::now();
        // Enter InputNeeded (sets the anchor).
        assert_eq!(
            m.on_reading(Some(Working), clear(InputNeeded), t0),
            Some(InputNeeded)
        );
        // An ambiguous Idle one second later is damped.
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(m.on_reading(Some(InputNeeded), ambiguous(Idle), t1), None);
        // A clear Idle (recognized composer) is honored immediately.
        assert_eq!(m.on_reading(Some(InputNeeded), clear(Idle), t1), Some(Idle));
    }

    #[test]
    fn stale_input_needed_exit_passes() {
        let mut m =
            AgentStateMachine::with_hysteresis(Duration::from_secs(8), Duration::from_secs(5));
        let t0 = Instant::now();
        assert_eq!(
            m.on_reading(Some(Working), clear(InputNeeded), t0),
            Some(InputNeeded)
        );
        // Past the window, even an ambiguous Idle passes.
        let t1 = t0 + Duration::from_secs(9);
        assert_eq!(
            m.on_reading(Some(InputNeeded), ambiguous(Idle), t1),
            Some(Idle)
        );
    }

    #[test]
    fn ambiguous_working_exit_is_damped_but_a_dialog_is_not() {
        let mut m =
            AgentStateMachine::with_hysteresis(Duration::from_secs(8), Duration::from_secs(5));
        let t0 = Instant::now();
        assert_eq!(m.on_reading(Some(Idle), clear(Working), t0), Some(Working));
        // Spinner dropped for a frame → ambiguous Idle within window → damp.
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(m.on_reading(Some(Working), ambiguous(Idle), t1), None);
        // A live dialog mid-work surfaces immediately (not an Idle reading).
        assert_eq!(
            m.on_reading(Some(Working), ambiguous(InputNeeded), t1),
            Some(InputNeeded)
        );
    }

    #[test]
    fn done_survives_an_idle_reading_but_not_progress() {
        let mut m = AgentStateMachine::new();
        let now = Instant::now();
        // A bare idle reading can't clear Done.
        assert_eq!(m.on_reading(Some(Done), clear(Idle), now), None);
        assert_eq!(m.on_reading(Some(Done), ambiguous(Idle), now), None);
        // Real progress and a fresh prompt do.
        assert_eq!(m.on_reading(Some(Done), clear(Working), now), Some(Working));
        assert_eq!(
            m.on_reading(Some(Done), clear(InputNeeded), now),
            Some(InputNeeded)
        );
    }

    #[test]
    fn reset_input_anchor_lets_the_next_exit_through() {
        let mut m =
            AgentStateMachine::with_hysteresis(Duration::from_secs(8), Duration::from_secs(5));
        let t0 = Instant::now();
        assert_eq!(
            m.on_reading(Some(Working), clear(InputNeeded), t0),
            Some(InputNeeded)
        );
        // Without a reset the ambiguous exit would damp...
        m.reset_input_anchor();
        // ...but with the anchor cleared it passes even within the window.
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(
            m.on_reading(Some(InputNeeded), ambiguous(Idle), t1),
            Some(Idle)
        );
    }
}
