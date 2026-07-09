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
/// `WORKING_HYSTERESIS` because a parked prompt can sit quiet far longer
/// than a spinner drops between frames.
pub(crate) const INPUT_NEEDED_HYSTERESIS: Duration = Duration::from_secs(8);

/// Hysteresis window for the edge that LEAVES `Working`. Claude paints its
/// live status line (spinner + token counter + `esc to interrupt`) only
/// while busy; that line vanishes for a chunk whenever the agent prints
/// output between spinner frames or the terminal does a full repaint,
/// leaving the detector with an ambiguous `Idle`. Shorter than the
/// `InputNeeded` window: the spinner reappears within a frame or two.
pub(crate) const WORKING_HYSTERESIS: Duration = Duration::from_secs(5);

/// One detection reading offered to the machine, tagged with the evidence
/// quality the hysteresis needs.
#[derive(Debug, Clone, Copy)]
pub struct Reading {
    /// The state this reading implies.
    pub state: AgentState,
    /// Whether the detector is *affirmatively* sure. The daemon's quiet
    /// classification — the resting screen read after seconds of PTY
    /// silence — is clear and honored immediately; the per-chunk
    /// byte-flow `Working` reading is inferred, not affirmed, so it
    /// arrives ambiguous and is damped within the hysteresis window
    /// when it would exit `InputNeeded` (a brief repaint burst at a
    /// parked prompt must not flap the `?` off) and unconditionally
    /// when it would exit `Done` (see `suppress_done_exit`).
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
pub(crate) fn transition_allowed(from: AgentState, to: AgentState) -> bool {
    !matches!((from, to), (AgentState::Done, AgentState::Idle))
}

/// The result of folding a [`Reading`] into the machine via
/// [`AgentStateMachine::on_reading`]. Distinguishes a committed move from
/// the reasons a reading was held, so the caller can log the diagnostic
/// (`Damped`) cases without drowning the log in the high-frequency
/// steady-state `Unchanged` dedupe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The machine moved to this state; broadcast it.
    Committed(AgentState),
    /// The reading matched the current state — nothing to do. The common
    /// steady-state case: a streaming agent re-reads `Working` every chunk.
    Unchanged,
    /// A structurally forbidden edge held the state (only `Done → Idle`
    /// today — `Done` stickiness).
    Rejected,
    /// An ambiguous reading was held: a flap (a dropped status-line frame)
    /// damped within a hysteresis window, or a byte-flow `Working` that may
    /// not clear `Done`. The prior state stands.
    Damped,
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
    /// (`transition_allowed`). This is the single choke point for every
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
    /// `current` cached state. Returns the [`Outcome`]: a committed move, or
    /// the reason the reading was held (a no-op dedupe, a forbidden edge, or
    /// a damped ambiguous flap).
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
        current: Option<AgentState>,
        reading: Reading,
        now: Instant,
    ) -> Outcome {
        // Refresh the anchor on every busy / waiting reading, even ones that
        // dedupe or damp below, so the hysteresis measures time since the
        // signal was LAST seen — a transient frame that drops it still reads
        // as a recent-enough anchor.
        match reading.state {
            AgentState::InputNeeded => self.last_input_needed_at = Some(now),
            AgentState::Working => self.last_working_at = Some(now),
            _ => {}
        }
        if self.suppress_input_needed_exit(current, reading, now)
            || self.suppress_working_exit(current, reading, now)
            || Self::suppress_done_exit(current, reading)
        {
            return Outcome::Damped;
        }
        match Self::transition(current, reading.state) {
            Some(to) => Outcome::Committed(to),
            None if current == Some(reading.state) => Outcome::Unchanged,
            None => Outcome::Rejected,
        }
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
        current: Option<AgentState>,
        reading: Reading,
        now: Instant,
    ) -> bool {
        current == Some(AgentState::InputNeeded)
            && reading.state != AgentState::InputNeeded
            && !reading.clear
            && self
                .last_input_needed_at
                .is_some_and(|t| now.duration_since(t) < self.input_hysteresis)
    }

    /// Whether to hold an ambiguous `Working` reading against `Done`. A
    /// byte-flow `Working` — a stray repaint (pane resize, reattach
    /// redraw) or the user typing into the composer — must not clear the
    /// "finished, take a look" alert. Leaving `Done` requires affirmative
    /// evidence: a clear `Working` (a quiet-classified live status line),
    /// an `InputNeeded` (never a Working reading), or a hook (which
    /// commits through `transition` directly and skips this damper).
    /// Unlike the two windowed dampers this has no time bound — `Done`
    /// has no natural decay, so ambiguity alone can never end it.
    fn suppress_done_exit(current: Option<AgentState>, reading: Reading) -> bool {
        current == Some(AgentState::Done) && reading.state == AgentState::Working && !reading.clear
    }

    /// Whether to damp the ambiguous `Working → Idle` edge — a single frame
    /// where Claude's spinner is absent and the composer isn't yet drawn.
    /// An affirmative idle composer (`clear`) is the real end of turn and is
    /// honored immediately; a transition into a live dialog is never damped
    /// (it isn't an `Idle` reading).
    fn suppress_working_exit(
        &self,
        current: Option<AgentState>,
        reading: Reading,
        now: Instant,
    ) -> bool {
        current == Some(AgentState::Working)
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

    fn machine() -> AgentStateMachine {
        AgentStateMachine::with_hysteresis(Duration::from_secs(8), Duration::from_secs(5))
    }

    #[test]
    fn on_reading_reports_dedupe_and_sticky_distinctly() {
        let mut m = machine();
        let now = Instant::now();
        // A reading that matches the current state is a silent no-op.
        assert_eq!(
            m.on_reading(Some(Working), clear(Working), now),
            Outcome::Unchanged
        );
        // The forbidden Done→Idle edge is a structural rejection, NOT a damp
        // — so the caller can log the two differently.
        assert_eq!(
            m.on_reading(Some(Done), clear(Idle), now),
            Outcome::Rejected
        );
    }

    #[test]
    fn ambiguous_input_needed_exit_is_damped_within_the_window() {
        let mut m = machine();
        let t0 = Instant::now();
        // Enter InputNeeded (sets the anchor).
        assert_eq!(
            m.on_reading(Some(Working), clear(InputNeeded), t0),
            Outcome::Committed(InputNeeded)
        );
        // An ambiguous Idle one second later is damped.
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(
            m.on_reading(Some(InputNeeded), ambiguous(Idle), t1),
            Outcome::Damped
        );
        // A clear Idle (recognized composer) is honored immediately.
        assert_eq!(
            m.on_reading(Some(InputNeeded), clear(Idle), t1),
            Outcome::Committed(Idle)
        );
    }

    #[test]
    fn stale_input_needed_exit_passes() {
        let mut m = machine();
        let t0 = Instant::now();
        assert_eq!(
            m.on_reading(Some(Working), clear(InputNeeded), t0),
            Outcome::Committed(InputNeeded)
        );
        // Past the window, even an ambiguous Idle passes.
        let t1 = t0 + Duration::from_secs(9);
        assert_eq!(
            m.on_reading(Some(InputNeeded), ambiguous(Idle), t1),
            Outcome::Committed(Idle)
        );
    }

    #[test]
    fn ambiguous_working_exit_is_damped_but_a_dialog_is_not() {
        let mut m = machine();
        let t0 = Instant::now();
        assert_eq!(
            m.on_reading(Some(Idle), clear(Working), t0),
            Outcome::Committed(Working)
        );
        // Spinner dropped for a frame → ambiguous Idle within window → damp.
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(
            m.on_reading(Some(Working), ambiguous(Idle), t1),
            Outcome::Damped
        );
        // A live dialog mid-work surfaces immediately (not an Idle reading).
        assert_eq!(
            m.on_reading(Some(Working), ambiguous(InputNeeded), t1),
            Outcome::Committed(InputNeeded)
        );
    }

    #[test]
    fn clear_working_exit_is_honored_within_the_window() {
        // Symmetric to the InputNeeded exit: a CLEAR idle (the composer
        // footer is drawn — a real end of turn) leaves `Working` immediately
        // even while the anchor is fresh.
        let mut m = machine();
        let t0 = Instant::now();
        assert_eq!(
            m.on_reading(Some(Idle), clear(Working), t0),
            Outcome::Committed(Working)
        );
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(
            m.on_reading(Some(Working), clear(Idle), t1),
            Outcome::Committed(Idle)
        );
    }

    #[test]
    fn stale_working_exit_passes() {
        let mut m = machine();
        let t0 = Instant::now();
        assert_eq!(
            m.on_reading(Some(Idle), clear(Working), t0),
            Outcome::Committed(Working)
        );
        // Past the 5s working window, even an ambiguous Idle passes: the
        // spinner has been gone long enough that the agent really is idle.
        let t1 = t0 + Duration::from_secs(6);
        assert_eq!(
            m.on_reading(Some(Working), ambiguous(Idle), t1),
            Outcome::Committed(Idle)
        );
    }

    #[test]
    fn working_exit_with_no_anchor_passes() {
        // First frame after stale hooks open the PTY gate: the machine never
        // saw a Working reading, so there's no anchor to damp against and an
        // ambiguous Idle passes straight through.
        let mut m = machine();
        assert_eq!(
            m.on_reading(Some(Working), ambiguous(Idle), Instant::now()),
            Outcome::Committed(Idle)
        );
    }

    #[test]
    fn done_survives_an_idle_reading_but_not_progress() {
        let mut m = AgentStateMachine::new();
        let now = Instant::now();
        // A bare idle reading can't clear Done (a structural rejection).
        assert_eq!(
            m.on_reading(Some(Done), clear(Idle), now),
            Outcome::Rejected
        );
        assert_eq!(
            m.on_reading(Some(Done), ambiguous(Idle), now),
            Outcome::Rejected
        );
        // Real progress and a fresh prompt do.
        assert_eq!(
            m.on_reading(Some(Done), clear(Working), now),
            Outcome::Committed(Working)
        );
        assert_eq!(
            m.on_reading(Some(Done), clear(InputNeeded), now),
            Outcome::Committed(InputNeeded)
        );
    }

    #[test]
    fn ambiguous_working_cannot_clear_done() {
        // A byte-flow Working (a stray repaint after the hook gate has
        // gone stale) must be held: only a CLEAR Working — a live status
        // line classified on a quiet screen — is real progress.
        let mut m = AgentStateMachine::new();
        let now = Instant::now();
        assert_eq!(
            m.on_reading(Some(Done), ambiguous(Working), now),
            Outcome::Damped
        );
        // No time window: it stays held arbitrarily later.
        let later = now + Duration::from_secs(3600);
        assert_eq!(
            m.on_reading(Some(Done), ambiguous(Working), later),
            Outcome::Damped
        );
        assert_eq!(
            m.on_reading(Some(Done), clear(Working), later),
            Outcome::Committed(Working)
        );
    }

    #[test]
    fn reset_input_anchor_lets_the_next_exit_through() {
        let mut m = machine();
        let t0 = Instant::now();
        assert_eq!(
            m.on_reading(Some(Working), clear(InputNeeded), t0),
            Outcome::Committed(InputNeeded)
        );
        // Without a reset the ambiguous exit would damp...
        m.reset_input_anchor();
        // ...but with the anchor cleared it passes even within the window.
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(
            m.on_reading(Some(InputNeeded), ambiguous(Idle), t1),
            Outcome::Committed(Idle)
        );
    }
}
