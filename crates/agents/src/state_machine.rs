//! The agent lifecycle as an explicit state machine.
//!
//! The [`AgentState`] variants are the machine's states; a detected
//! signal — from the PTY screen-scraper ([`crate::detect`]), a lifecycle
//! hook ([`crate::hook`]), or the PTY-exit teardown — is an *input* the
//! machine folds into the current state under a fixed table of allowed
//! transitions. Consolidating every state change behind
//! [`AgentStateMachine::transition`] means the broadcast (and therefore
//! displayed) status is always the result of a legal move from the prior
//! state, never an independent per-poll guess that can jump between
//! contradictory readings.
//!
//! Mapping the lifecycle onto the wire vocabulary:
//! - `Idle` — freshly launched, no work run yet ("starting").
//! - `Working` — actively producing output or running a tool ("running").
//! - `InputNeeded` — parked on a structural prompt ("awaiting input").
//! - `Done` — finished a turn ("done").
//! - `Exited` — the process ended (clean or crash); terminal.
//!
//! ## The load-bearing rule: `Working` is a one-way door
//!
//! Once an agent is `Working` the only legal exits are `Done`,
//! `InputNeeded`, and `Exited` — **never `Idle`**. A working agent that
//! comes to rest has *finished a turn* (`Done`); it has not reverted to
//! the never-worked `Idle`. Encoding that as a forbidden edge is what
//! makes "the working spinner silently blanks to no pill" impossible,
//! rather than a flap the UI has to damp:
//!
//! ```text
//! Working ─╳→ Idle        (a settled worker is Done, not un-worked)
//! Done    ─╳→ Idle        (Done is sticky until real progress)
//! ```
//!
//! Two consequences fall out of the one-way door:
//!   - The PTY quiet-classifier's resting-composer reading, when the
//!     agent is `Working`, is promoted to `Done` (see
//!     [`AgentStateMachine::on_reading`]). That is the *only* finished-turn
//!     signal a hookless agent (Codex, Cursor) can offer — it has no
//!     `Stop` hook — so this is how they reach `Done` at all.
//!   - Boot output flows as bytes and reads as an ambiguous `Working`
//!     before the agent has run anything. If that entered `Working`, the
//!     settle right after would have to leave via the forbidden edge (or
//!     promote to a false `Done`). So an ambiguous `Working` is *held*
//!     until the agent has booted — its composer drawn, or a resting
//!     screen classified (the `booted` latch).
//!
//! Beyond the table, the machine damps one *ambiguous* edge the PTY
//! detector produces when a live prompt scrolls out of the detect window
//! for a frame (the `InputNeeded` exit); that flap is the last thing this
//! machine exists to eliminate.

use std::time::{Duration, Instant};

use lazybox_ipc::AgentState;

/// Hysteresis window for the edge that LEAVES `InputNeeded`. Claude's
/// status-bar ticker can scroll a live prompt out of the detect window for a
/// single chunk, momentarily reading as `Idle` even though Claude is still
/// waiting; without damping the `?` pill flickers off and back.
pub(crate) const INPUT_NEEDED_HYSTERESIS: Duration = Duration::from_secs(8);

/// One detection reading offered to the machine, tagged with the evidence
/// quality the machine needs.
#[derive(Debug, Clone, Copy)]
pub struct Reading {
    /// The state this reading implies.
    pub state: AgentState,
    /// Whether the detector is *affirmatively* sure. The daemon's quiet
    /// classification — the resting screen read after seconds of PTY
    /// silence — is clear and honored immediately; the per-chunk
    /// byte-flow `Working` reading is inferred, not affirmed, so it
    /// arrives ambiguous. An ambiguous `Working` is held before boot
    /// (the boot latch) and can never clear `Done` (a stray repaint must
    /// not un-finish a turn); an ambiguous `Idle` exiting `InputNeeded`
    /// is damped within the hysteresis window. A clear reading is
    /// affirmative evidence and both proves boot and passes immediately.
    pub clear: bool,
}

/// Whether the machine permits a move from `from` to a *different* state
/// `to`. The lifecycle is near-complete — an agent can go busy, ask, or
/// exit from almost anywhere, because detection legitimately observes all
/// of those edges — with exactly two forbidden edges, both landing on the
/// never-worked `Idle`:
///
/// ```text
/// Working ─╳→ Idle
/// Done    ─╳→ Idle
/// ```
///
/// `Idle` means "spawned, hasn't worked yet." An agent that has been
/// `Working` (or finished, `Done`) can never truthfully be back there: a
/// settled worker is `Done`, and `Done` stays put until the agent makes
/// real progress (`Working`) or is asked for input (`InputNeeded`). Every
/// other edge — including any state → `Exited` (the process can die at any
/// moment) — is allowed.
///
/// Only meaningful for `from != to`; a self-loop is not a transition (see
/// [`AgentStateMachine::transition`]).
pub(crate) fn transition_allowed(from: AgentState, to: AgentState) -> bool {
    !matches!(
        (from, to),
        (AgentState::Done, AgentState::Idle) | (AgentState::Working, AgentState::Idle)
    )
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
    /// A structurally forbidden edge held the state (`Working → Idle` or
    /// `Done → Idle` — neither settles back to the never-worked `Idle`).
    Rejected,
    /// An ambiguous reading was held: a boot-time `Working` before the
    /// agent booted, a `?`-exit flap damped within the hysteresis window,
    /// or a byte-flow `Working` that may not clear `Done`. The prior
    /// state stands.
    Damped,
}

/// One terminal's lifecycle state machine.
///
/// The current [`AgentState`] lives in the daemon's shared cache (one entry
/// per terminal, read by every status consumer); this type owns the
/// *transition policy* and the per-terminal timing anchor the hysteresis
/// needs. All writers route their candidate through
/// [`AgentStateMachine::transition`] so the cache only ever holds a legal
/// successor of its prior value; the PTY pump additionally runs its noisy
/// readings through [`AgentStateMachine::on_reading`] first.
#[derive(Debug)]
pub struct AgentStateMachine {
    last_input_needed_at: Option<Instant>,
    input_hysteresis: Duration,
    /// Latched once the agent has finished booting: its composer has been
    /// drawn (via [`AgentStateMachine::mark_booted`]) or a resting screen
    /// has been classified (any clear reading). Until it latches, an
    /// ambiguous byte-flow `Working` is held so boot output can't enter
    /// `Working` and force the forbidden `Working → Idle` settle (or a
    /// false `Done`). See the module docs.
    booted: bool,
}

impl Default for AgentStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentStateMachine {
    /// A machine with the production hysteresis window.
    pub fn new() -> Self {
        Self::with_input_hysteresis(INPUT_NEEDED_HYSTERESIS)
    }

    /// A machine with an explicit `InputNeeded`-exit hysteresis window
    /// (tests inject a short one).
    pub fn with_input_hysteresis(input_hysteresis: Duration) -> Self {
        Self {
            last_input_needed_at: None,
            input_hysteresis,
            booted: false,
        }
    }

    /// Mark the agent as booted: its input composer has been drawn at
    /// least once, so an ambiguous `Working` reading now reflects real
    /// work rather than boot chrome. Idempotent. The daemon calls this
    /// the first time the agent reports "ready for a prompt", which
    /// covers the autonomous-spawn case where the work prompt is injected
    /// during boot (before the first quiet classification could latch it).
    pub fn mark_booted(&mut self) {
        self.booted = true;
    }

    /// The structural transition table. Given the terminal's current state
    /// (`None` when it has never reported one) and a candidate `to`, returns
    /// `Some(to)` when the move is a legal, state-changing transition, or
    /// `None` when it is a no-op (`from == to`) or a forbidden edge
    /// (`transition_allowed`). This is the single choke point for every
    /// state change — the PTY pump, hook ingest, the optimistic answer
    /// flip, and the PTY-exit teardown all commit through it.
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
    /// Three policies live here, on top of the structural table:
    ///   - **Boot gate** — an ambiguous `Working` before the agent has
    ///     booted is held, so boot output never enters `Working`.
    ///   - **Settle promotion** — a clear `Idle` (resting composer) while
    ///     the agent is `Working` is a finished turn, so it's promoted to
    ///     `Done`. This is a hookless agent's only path to `Done`.
    ///   - **`?`-exit damping** — an ambiguous `Idle` while a prompt is
    ///     genuinely still up is held within the hysteresis window.
    pub fn on_reading(
        &mut self,
        current: Option<AgentState>,
        reading: Reading,
        now: Instant,
    ) -> Outcome {
        // Refresh the anchor on every waiting reading, even ones that
        // dedupe or damp below, so the hysteresis measures time since the
        // signal was LAST seen — a transient frame that drops it still reads
        // as a recent-enough anchor.
        if reading.state == AgentState::InputNeeded {
            self.last_input_needed_at = Some(now);
        }
        // Boot gate: hold an ambiguous byte-flow `Working` from a *fresh*
        // session (no state reported yet) until the agent has booted, so
        // boot chrome can't enter `Working` and force the settle right after
        // to leave via the forbidden `Working → Idle` edge (or a false
        // `Done`). Once any state is established — a hook, a classified
        // resting screen, a prompt — the session is past boot and the gate
        // is open. Positive `Working` signals (a hook, the user pressing
        // Enter) commit through `transition` directly and never reach here.
        if reading.state == AgentState::Working
            && !reading.clear
            && current.is_none()
            && !self.booted
        {
            return Outcome::Damped;
        }
        // Any clear reading is an affirmative on-screen classification —
        // proof the agent is past boot chrome.
        if reading.clear {
            self.booted = true;
        }
        // Settle promotion: from `Working`, a resting composer (clear
        // `Idle`) is a finished turn, not a reversion to the never-worked
        // `Idle`. Promote to `Done` so `Working → Idle` stays unreachable
        // and hookless agents can reach `Done` (#357).
        let reading = if current == Some(AgentState::Working)
            && reading.state == AgentState::Idle
            && reading.clear
        {
            Reading {
                state: AgentState::Done,
                clear: true,
            }
        } else {
            reading
        };
        if self.suppress_input_needed_exit(current, reading, now)
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
    /// honored immediately, so a wrong `InputNeeded` can't stick once the
    /// agent is visibly streaming or idle.
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
    /// an `InputNeeded`, or a hook (which commits through `transition`
    /// directly and skips this damper). Unlike the `?`-exit damper this
    /// has no time bound — `Done` has no natural decay, so ambiguity alone
    /// can never end it.
    fn suppress_done_exit(current: Option<AgentState>, reading: Reading) -> bool {
        current == Some(AgentState::Done) && reading.state == AgentState::Working && !reading.clear
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentState::{Done, Idle, InputNeeded, Working};

    const EXITED: AgentState = AgentState::Exited { code: Some(0) };
    const ALL: [AgentState; 5] = [Working, InputNeeded, Idle, Done, EXITED];

    #[test]
    fn only_settling_back_to_idle_is_forbidden() {
        for from in ALL {
            for to in ALL {
                if from == to {
                    continue;
                }
                let allowed = transition_allowed(from, to);
                let is_forbidden = matches!((from, to), (Done, Idle) | (Working, Idle));
                if is_forbidden {
                    assert!(
                        !allowed,
                        "{from:?} → Idle must be forbidden (never un-worked)"
                    );
                } else {
                    assert!(allowed, "{from:?} → {to:?} must be allowed");
                }
            }
        }
    }

    #[test]
    fn any_state_can_exit() {
        // The process can die at any moment — every state reaches `Exited`.
        for from in [Working, InputNeeded, Idle, Done] {
            assert_eq!(
                AgentStateMachine::transition(Some(from), EXITED),
                Some(EXITED),
                "{from:?} → Exited must commit",
            );
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
        // A working agent never settles back to the never-worked Idle.
        assert_eq!(AgentStateMachine::transition(Some(Working), Idle), None);
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

    /// A machine already past boot, so ambiguous `Working` readings commit
    /// straight away (most tests exercise the steady state, not the gate).
    fn machine() -> AgentStateMachine {
        let mut m = AgentStateMachine::with_input_hysteresis(Duration::from_secs(8));
        m.mark_booted();
        m
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

    // ── the boot gate ─────────────────────────────────────────────

    #[test]
    fn ambiguous_working_is_held_until_booted() {
        // A fresh machine hasn't booted: boot output flows as bytes and
        // reads as an ambiguous `Working`, which must NOT enter `Working`.
        let mut m = AgentStateMachine::new();
        let now = Instant::now();
        assert_eq!(m.on_reading(None, ambiguous(Working), now), Outcome::Damped);
        // A clear resting classification proves boot and commits Idle...
        assert_eq!(
            m.on_reading(None, clear(Idle), now),
            Outcome::Committed(Idle)
        );
        // ...after which the ambiguous byte-flow Working commits normally.
        assert_eq!(
            m.on_reading(Some(Idle), ambiguous(Working), now),
            Outcome::Committed(Working)
        );
    }

    #[test]
    fn mark_booted_opens_the_gate_for_autonomous_spawns() {
        // The autonomous flow injects the work prompt during boot, before a
        // quiet classification could latch `booted`. `mark_booted` (fired on
        // the "ready for prompt" signal) opens the gate so the first turn's
        // byte flow reads as Working.
        let mut m = AgentStateMachine::new();
        let now = Instant::now();
        assert_eq!(m.on_reading(None, ambiguous(Working), now), Outcome::Damped);
        m.mark_booted();
        assert_eq!(
            m.on_reading(None, ambiguous(Working), now),
            Outcome::Committed(Working)
        );
    }

    #[test]
    fn clear_working_commits_and_proves_boot_even_unbooted() {
        // A clear `Working` (a positive status-line classification) is
        // affirmative, so it commits pre-boot AND latches the boot flag.
        let mut m = AgentStateMachine::new();
        let now = Instant::now();
        assert_eq!(
            m.on_reading(None, clear(Working), now),
            Outcome::Committed(Working)
        );
        // The gate is now open for subsequent ambiguous Working.
        assert_eq!(
            m.on_reading(Some(Done), ambiguous(Working), now),
            Outcome::Damped, // held by suppress_done_exit, not the boot gate
        );
    }

    // ── the one-way door: Working never settles back to Idle ──────

    #[test]
    fn working_settles_to_done_not_idle() {
        // A working agent that comes to rest at its composer has finished a
        // turn: the resting-composer reading is promoted to Done. This is
        // how a hookless agent (Codex, Cursor) reaches Done at all.
        let mut m = machine();
        let now = Instant::now();
        assert_eq!(
            m.on_reading(Some(Working), clear(Idle), now),
            Outcome::Committed(Done),
        );
    }

    #[test]
    fn a_fresh_idle_settle_is_not_a_false_done() {
        // A resting composer reached from Idle/None (never worked) stays
        // Idle — the promotion only fires from Working.
        let mut m = machine();
        let now = Instant::now();
        assert_eq!(
            m.on_reading(None, clear(Idle), now),
            Outcome::Committed(Idle)
        );
        assert_eq!(
            m.on_reading(Some(Idle), clear(Idle), now),
            Outcome::Unchanged,
        );
    }

    #[test]
    fn ambiguous_idle_never_leaves_working() {
        // The only Idle reading the machine ever sees is the clear
        // quiet-classification (byte flow only ever reads Working). But even
        // a hypothetical ambiguous Idle can't demote Working: it's the
        // forbidden edge, structurally rejected.
        let mut m = machine();
        let now = Instant::now();
        assert_eq!(
            m.on_reading(Some(Working), ambiguous(Idle), now),
            Outcome::Rejected,
        );
    }

    // ── InputNeeded exit hysteresis ───────────────────────────────

    #[test]
    fn ambiguous_input_needed_exit_is_damped_within_the_window() {
        let mut m = machine();
        let t0 = Instant::now();
        assert_eq!(
            m.on_reading(Some(Working), clear(InputNeeded), t0),
            Outcome::Committed(InputNeeded)
        );
        // An ambiguous Working one second later is damped (would exit `?`).
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(
            m.on_reading(Some(InputNeeded), ambiguous(Working), t1),
            Outcome::Damped
        );
        // A clear Working (recognized live status line) is honored.
        assert_eq!(
            m.on_reading(Some(InputNeeded), clear(Working), t1),
            Outcome::Committed(Working)
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
        // Past the window, even an ambiguous Working passes.
        let t1 = t0 + Duration::from_secs(9);
        assert_eq!(
            m.on_reading(Some(InputNeeded), ambiguous(Working), t1),
            Outcome::Committed(Working)
        );
    }

    #[test]
    fn a_live_dialog_mid_work_surfaces_immediately() {
        let mut m = machine();
        let t0 = Instant::now();
        assert_eq!(
            m.on_reading(Some(Idle), clear(Working), t0),
            Outcome::Committed(Working)
        );
        assert_eq!(
            m.on_reading(Some(Working), ambiguous(InputNeeded), t0),
            Outcome::Committed(InputNeeded)
        );
    }

    // ── Done stickiness ───────────────────────────────────────────

    #[test]
    fn done_survives_an_idle_reading_but_not_progress() {
        let mut m = machine();
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
        // A byte-flow Working (a stray repaint) must be held: only a CLEAR
        // Working — a live status line classified on a quiet screen — is
        // real progress.
        let mut m = machine();
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
            m.on_reading(Some(InputNeeded), ambiguous(Working), t1),
            Outcome::Committed(Working)
        );
    }

    /// The end-to-end hookless lifecycle: boot (held) → ready → work →
    /// settle to Done → new turn → crash. No step ever lands on `Idle`
    /// after work, and no false `Done` appears at boot.
    #[test]
    fn hookless_lifecycle_never_regresses_to_idle() {
        let mut m = AgentStateMachine::new();
        let t = Instant::now();
        // Boot bytes are held.
        assert_eq!(m.on_reading(None, ambiguous(Working), t), Outcome::Damped);
        // Composer settles → Idle (boot complete, never worked).
        assert_eq!(m.on_reading(None, clear(Idle), t), Outcome::Committed(Idle));
        // A real turn begins (byte flow, now booted).
        assert_eq!(
            m.on_reading(Some(Idle), ambiguous(Working), t),
            Outcome::Committed(Working)
        );
        // The turn ends at a resting composer → Done (not Idle).
        assert_eq!(
            m.on_reading(Some(Working), clear(Idle), t),
            Outcome::Committed(Done)
        );
        // Sitting at the composer keeps Done.
        assert_eq!(m.on_reading(Some(Done), clear(Idle), t), Outcome::Rejected);
        // A new turn.
        assert_eq!(
            m.on_reading(Some(Done), ambiguous(Working), t),
            Outcome::Damped, // held against Done by suppress_done_exit
        );
        assert_eq!(
            m.on_reading(Some(Done), clear(Working), t),
            Outcome::Committed(Working)
        );
        // The process dies mid-turn — the terminal Exited state (set by the
        // teardown via `transition`, not a reading) is a legal successor.
        assert_eq!(
            AgentStateMachine::transition(Some(Working), EXITED),
            Some(EXITED)
        );
    }
}
