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
//! ## The load-bearing rules: one-way work and an absorbing exit
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
//! Exited  ─╳→ *           (the process is gone; late signals are stale)
//! ```
//!
//! Two consequences fall out of the one-way door:
//!   - A `Working` agent that comes to rest — its stream gone quiet — has
//!     finished a turn, so any *clear* quiet-classification that isn't a
//!     live prompt is promoted to `Done` (see
//!     [`AgentStateMachine::on_reading`]). That covers the resting
//!     composer, the stale-but-still-painted status line, and — via a bare
//!     `Done` reading the quiet timer surfaces — a screen that doesn't
//!     classify at all (weak Codex/Cursor detection, #225). It is the
//!     *only* finished-turn signal a hookless agent can offer.
//!   - Boot output flows as bytes and reads as an ambiguous `Working`
//!     before the agent has run anything. If that entered `Working`, the
//!     settle right after would have to leave via the forbidden edge (or
//!     promote to a false `Done`). So an ambiguous `Working` is *held*
//!     until the agent has booted — its composer drawn, or a resting
//!     screen classified (the `booted` latch).
//!
//! ## The companion rule: `InputNeeded` only leaves on evidence
//!
//! `InputNeeded` is sticky the same way `Done` is. An agent parked on a
//! prompt produces no output (#205), so the only readings that arrive
//! while it waits are incidental re-scrapes a click, focus, or redraw
//! triggers — and those are *ambiguous* (`!clear`) byte-flow `Working`
//! readings. An ambiguous reading is never evidence the prompt was
//! answered, so it can never leave `InputNeeded` (no time bound, exactly
//! like an ambiguous `Working` alone can never clear `Done`). Navigation
//! must not change whether an agent is asking (#374). A genuine resolution
//! leaves it on affirmative evidence only: the user submitting input (the
//! optimistic flip commits `Working` through `transition` directly), an
//! authoritative hook (likewise), or a *clear* quiet-classification of a
//! live-again screen.
//!
//! `Done` has one extra affirmative exit `InputNeeded` doesn't: a streak
//! of *progressing* readings ([`Reading::progress`] — chunks that each
//! changed the meaningful screen content, not repaint churn) re-commits
//! `Working` (#398). A resumed turn streams heavily and never pauses, so
//! the quiet classifier can't be the one to notice it; the content
//! fingerprint moving several chunks in a row is the resume event. A
//! parked prompt, by contrast, emits nothing, so `InputNeeded` never
//! sees such a streak and keeps its stricter contract.

use lazybox_ipc::AgentState;

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
    /// (the boot latch), can never clear `Done` (a stray repaint must not
    /// un-finish a turn), and can never clear `InputNeeded` (a stray
    /// repaint must not un-ask a parked prompt, #374). A clear reading is
    /// affirmative evidence: it proves boot, settles a quiet `Working`
    /// turn to `Done`, and is the only screen-scrape that may leave
    /// `Done`/`InputNeeded`.
    pub clear: bool,
    /// Whether the chunk behind this reading *changed the meaningful
    /// content* of the screen (the daemon's letters-only fingerprint
    /// moved — see `detect::content_fingerprint`), as opposed to
    /// repaint churn: a spinner frame, a counter tick, a cursor blink.
    /// A streak of progressing readings is the affirmative "the agent
    /// resumed real output" event that may leave `Done` (#398) — the
    /// one exit a busy stream can take, since the quiet classifier
    /// only ever runs at a pause the stream never offers. Meaningless
    /// on clear readings (the classification itself is the evidence).
    pub progress: bool,
}

/// How many progressing readings ([`Reading::progress`]) in a row —
/// while `Done`, uninterrupted by a clear classification — count as a
/// resumed turn and re-open `Working`. One or two is the shape of a
/// stray full-screen redraw fragmenting into distinct chunks; a
/// genuinely resumed agent streams past three within a second. A false
/// trip self-corrects at the next quiet classification (the settle
/// promotion re-lands `Done`), whereas a `Done` pinned on a visibly
/// streaming agent has no other exit at all.
pub(crate) const DONE_EXIT_PROGRESS_STREAK: u8 = 3;

/// Whether the machine permits a move from `from` to a *different* state
/// `to`. The lifecycle is near-complete — an agent can go busy, ask, or
/// exit from almost anywhere, because detection legitimately observes all
/// of those edges — except for the two edges landing on the never-worked
/// `Idle` and every edge leaving the terminal `Exited` state:
///
/// ```text
/// Working ─╳→ Idle
/// Done    ─╳→ Idle
/// Exited  ─╳→ *
/// ```
///
/// `Idle` means "spawned, hasn't worked yet." An agent that has been
/// `Working` (or finished, `Done`) can never truthfully be back there: a
/// settled worker is `Done`, and `Done` stays put until the agent makes
/// real progress (`Working`) or is asked for input (`InputNeeded`). Every
/// other edge — including any live state → `Exited` (the process can die at
/// any moment) — is allowed. Once the PTY is gone, delayed screen readings or
/// hooks cannot resurrect it; a new process receives a new terminal id.
///
/// Only meaningful for `from != to`; a self-loop is not a transition (see
/// [`AgentStateMachine::transition`]).
pub(crate) fn transition_allowed(from: AgentState, to: AgentState) -> bool {
    !matches!(from, AgentState::Exited { .. })
        && !matches!(
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
    /// A structurally forbidden edge held the state (`Working → Idle`, a
    /// `Done → Idle` settle, any late signal after `Exited`, or a `Done`
    /// reading offered from a state that never worked — none of which carry
    /// evidence of a valid successor).
    Rejected,
    /// An ambiguous reading was held: a boot-time `Working` before the
    /// agent booted, a byte-flow `Working` that may not clear `Done`, or a
    /// byte-flow `Working` that may not clear a parked `InputNeeded`. The
    /// prior state stands.
    Damped,
}

/// One terminal's lifecycle state machine.
///
/// The current [`AgentState`] lives in the daemon's shared cache (one entry
/// per terminal, read by every status consumer); this type owns the
/// *transition policy*. All writers route their candidate through
/// [`AgentStateMachine::transition`] so the cache only ever holds a legal
/// successor of its prior value; the PTY pump additionally runs its noisy
/// readings through [`AgentStateMachine::on_reading`] first.
#[derive(Debug)]
pub struct AgentStateMachine {
    /// Latched once the agent has finished booting: its composer has been
    /// drawn (via [`AgentStateMachine::mark_booted`]) or a resting screen
    /// has been classified (any clear reading). Until it latches, an
    /// ambiguous byte-flow `Working` is held so boot output can't enter
    /// `Working` and force the forbidden `Working → Idle` settle (or a
    /// false `Done`). See the module docs.
    booted: bool,
    /// Consecutive-while-`Done` count of progressing readings
    /// ([`Reading::progress`]). At [`DONE_EXIT_PROGRESS_STREAK`] the
    /// machine re-commits `Working`: several chunks that each changed
    /// the meaningful content are a resumed turn, not a stray repaint.
    /// Interleaved churn frames don't reset it (a busy stream mixes
    /// content and spinner chunks); a clear reading does — the screen
    /// settled and was classified authoritatively, so repaint
    /// fragments can never accumulate across rests into a false
    /// resume.
    done_progress_streak: u8,
}

impl Default for AgentStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentStateMachine {
    /// A fresh machine for a just-spawned terminal (not yet booted).
    pub fn new() -> Self {
        Self {
            booted: false,
            done_progress_streak: 0,
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

    /// Fold a PTY detection `reading` in, given the terminal's `current`
    /// cached state. Returns the [`Outcome`]: a committed move, or the
    /// reason the reading was held (a no-op dedupe, a forbidden edge, or a
    /// damped ambiguous flap).
    ///
    /// Four policies live here, on top of the structural table:
    ///   - **Boot gate** — an ambiguous `Working` before the agent has
    ///     booted is held, so boot output never enters `Working`.
    ///   - **Settle promotion** — any clear reading (a resting composer, a
    ///     wedged status line, or a bare `Done` for an unclassifiable
    ///     screen) while the agent is `Working` is a finished turn, so it's
    ///     promoted to `Done`. This is a hookless agent's only path to
    ///     `Done`.
    ///   - **`Done`-only-from-`Working`** — a `Done` reading carries no
    ///     evidence of a finished turn unless the agent was `Working`, so
    ///     from any other state it's held (never fabricates a `Done` from a
    ///     never-worked `Idle`, never clears a live `?`).
    ///   - **Sticky-exit damping** — an ambiguous reading may not leave
    ///     `Done` or `InputNeeded`; both need affirmative (`clear`) evidence
    ///     or a positive transition committed directly.
    pub fn on_reading(&mut self, current: Option<AgentState>, reading: Reading) -> Outcome {
        // The progress streak only measures consecutive-while-`Done`
        // evidence; anywhere else it must start from zero. Also covers
        // `Done` being left through the static `transition` path (a
        // hook, the user's answer flip) that this instance never sees.
        if current != Some(AgentState::Done) {
            self.done_progress_streak = 0;
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
        // proof the agent is past boot chrome, and a settled screen: the
        // progress streak restarts (see its field docs).
        if reading.clear {
            self.booted = true;
            self.done_progress_streak = 0;
        }
        // Settle promotion: from `Working`, a quiet screen is a finished
        // turn, not a reversion to the never-worked `Idle`. Any clear
        // reading that isn't a live prompt is promoted to `Done` — the
        // resting composer, the stale-painted `Working` status line (a
        // genuinely working agent repaints within the quiet window, so a
        // quiet one has stopped), and the bare `Done` the quiet timer
        // surfaces for a screen that classifies as nothing at all. Keeps
        // `Working → Idle` unreachable and lets hookless agents reach
        // `Done` (#357, #225).
        let reading = if current == Some(AgentState::Working)
            && reading.clear
            && reading.state != AgentState::InputNeeded
        {
            Reading {
                state: AgentState::Done,
                clear: true,
                progress: false,
            }
        } else {
            reading
        };
        // A `Done` reading is only ever a settled `Working` turn (the
        // promotion above, or the quiet timer's bare `Done` for an
        // unrecognized screen). Offered from any other state it is
        // evidence-free: it must not fabricate a finished turn from a
        // never-worked `Idle`, resurrect a terminal `Exited`, or clear a
        // live `?`.
        if reading.state == AgentState::Done && current != Some(AgentState::Working) {
            return if current == Some(AgentState::Done) {
                Outcome::Unchanged
            } else {
                Outcome::Rejected
            };
        }
        // The `Done` exit for a busy stream (#398). An ambiguous
        // byte-flow `Working` alone must never clear the "finished,
        // take a look" alert — a stray repaint (pane resize, reattach
        // redraw) or the user typing into the composer reads exactly
        // like it. But a *streak* of progressing readings — several
        // chunks that each changed the meaningful content — is a
        // resumed turn, and the only evidence a heavy stream can ever
        // produce: the quiet classifier needs a pause the stream never
        // offers. `InputNeeded` deliberately gets no such exit — a
        // parked prompt means the agent is NOT emitting new content,
        // and #374 makes `?` yield to affirmative evidence only.
        if current == Some(AgentState::Done)
            && reading.state == AgentState::Working
            && !reading.clear
        {
            if reading.progress {
                self.done_progress_streak += 1;
                if self.done_progress_streak >= DONE_EXIT_PROGRESS_STREAK {
                    self.done_progress_streak = 0;
                    return Outcome::Committed(AgentState::Working);
                }
            }
            return Outcome::Damped;
        }
        if Self::suppress_input_needed_exit(current, reading) {
            return Outcome::Damped;
        }
        match Self::transition(current, reading.state) {
            Some(to) => Outcome::Committed(to),
            None if current == Some(reading.state) => Outcome::Unchanged,
            None => Outcome::Rejected,
        }
    }

    /// Whether to hold an ambiguous reading that would leave `InputNeeded`.
    /// A parked prompt emits no output (#205), so an ambiguous byte-flow
    /// `Working` reaching it is an incidental repaint (a click, a focus, a
    /// resize) — never evidence the ask was answered. Leaving `InputNeeded`
    /// requires affirmative evidence: a clear reading (the agent visibly
    /// streaming or idle again), or a positive transition committed through
    /// `transition` directly (the user's answer flip, a hook). Unbounded in
    /// time, exactly like [`Self::suppress_done_exit`] — navigation can
    /// never clear a `?` (#374).
    fn suppress_input_needed_exit(current: Option<AgentState>, reading: Reading) -> bool {
        current == Some(AgentState::InputNeeded)
            && reading.state != AgentState::InputNeeded
            && !reading.clear
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentState::{Done, Idle, InputNeeded, Working};

    const EXITED: AgentState = AgentState::Exited { code: Some(0) };
    const ALL: [AgentState; 5] = [Working, InputNeeded, Idle, Done, EXITED];

    #[test]
    fn transition_table_enforces_idle_and_exit_invariants() {
        for from in ALL {
            for to in ALL {
                if from == to {
                    continue;
                }
                let allowed = transition_allowed(from, to);
                let is_forbidden = matches!((from, to), (Done, Idle) | (Working, Idle))
                    || matches!(from, AgentState::Exited { .. });
                if is_forbidden {
                    assert!(!allowed, "{from:?} → {to:?} must be forbidden");
                } else {
                    assert!(allowed, "{from:?} → {to:?} must be allowed");
                }
            }
        }
    }

    #[test]
    fn any_state_can_exit() {
        // The process can die at any moment — every live state reaches
        // `Exited`.
        for from in [Working, InputNeeded, Idle, Done] {
            assert_eq!(
                AgentStateMachine::transition(Some(from), EXITED),
                Some(EXITED),
                "{from:?} → Exited must commit",
            );
        }
    }

    #[test]
    fn exited_is_absorbing_even_when_a_late_signal_differs() {
        for late in [Working, InputNeeded, Idle, Done] {
            assert_eq!(
                AgentStateMachine::transition(Some(EXITED), late),
                None,
                "late {late:?} must not resurrect an exited terminal",
            );
            let mut m = machine();
            assert_eq!(
                m.on_reading(Some(EXITED), clear(late)),
                Outcome::Rejected,
                "late clear {late:?} reading must be rejected",
            );
        }
        // A second teardown cannot rewrite the process's terminal result.
        assert_eq!(
            AgentStateMachine::transition(Some(EXITED), AgentState::Exited { code: Some(9) }),
            None,
        );
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
        Reading {
            state,
            clear: true,
            progress: false,
        }
    }
    fn ambiguous(state: AgentState) -> Reading {
        Reading {
            state,
            clear: false,
            progress: false,
        }
    }
    /// An ambiguous byte-flow `Working` whose chunk changed the content
    /// fingerprint — the pump's "real output arrived" reading.
    fn progressing() -> Reading {
        Reading {
            state: AgentState::Working,
            clear: false,
            progress: true,
        }
    }

    /// A machine already past boot, so ambiguous `Working` readings commit
    /// straight away (most tests exercise the steady state, not the gate).
    fn machine() -> AgentStateMachine {
        let mut m = AgentStateMachine::new();
        m.mark_booted();
        m
    }

    #[test]
    fn on_reading_reports_dedupe_and_sticky_distinctly() {
        let mut m = machine();
        // A reading that matches the current state is a silent no-op. (An
        // ambiguous `Working` — the steady-state byte-flow reading — while
        // already `Working`; a *clear* `Working` here would be a wedged
        // settle promoted to `Done`, covered separately.)
        assert_eq!(
            m.on_reading(Some(Working), ambiguous(Working)),
            Outcome::Unchanged
        );
        // The forbidden Done→Idle edge is a structural rejection, NOT a damp
        // — so the caller can log the two differently.
        assert_eq!(m.on_reading(Some(Done), clear(Idle)), Outcome::Rejected);
    }

    // ── the boot gate ─────────────────────────────────────────────

    #[test]
    fn ambiguous_working_is_held_until_booted() {
        // A fresh machine hasn't booted: boot output flows as bytes and
        // reads as an ambiguous `Working`, which must NOT enter `Working`.
        let mut m = AgentStateMachine::new();
        assert_eq!(m.on_reading(None, ambiguous(Working)), Outcome::Damped);
        // A clear resting classification proves boot and commits Idle...
        assert_eq!(m.on_reading(None, clear(Idle)), Outcome::Committed(Idle));
        // ...after which the ambiguous byte-flow Working commits normally.
        assert_eq!(
            m.on_reading(Some(Idle), ambiguous(Working)),
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
        assert_eq!(m.on_reading(None, ambiguous(Working)), Outcome::Damped);
        m.mark_booted();
        assert_eq!(
            m.on_reading(None, ambiguous(Working)),
            Outcome::Committed(Working)
        );
    }

    #[test]
    fn clear_working_commits_and_proves_boot_even_unbooted() {
        // A clear `Working` (a positive status-line classification) is
        // affirmative, so it commits pre-boot AND latches the boot flag.
        let mut m = AgentStateMachine::new();
        assert_eq!(
            m.on_reading(None, clear(Working)),
            Outcome::Committed(Working)
        );
        // The gate is now open for subsequent ambiguous Working.
        assert_eq!(
            m.on_reading(Some(Done), ambiguous(Working)),
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
        assert_eq!(
            m.on_reading(Some(Working), clear(Idle)),
            Outcome::Committed(Done),
        );
    }

    #[test]
    fn a_wedged_working_status_line_settles_to_done() {
        // The stream has gone quiet but the last frame still paints a
        // working status line, so the quiet classifier reads a *clear*
        // Working. A genuinely working agent repaints within the quiet
        // window, so a quiet one has finished — promote to Done rather than
        // spin forever (the "keeps spinning" bug).
        let mut m = machine();
        assert_eq!(
            m.on_reading(Some(Working), clear(Working)),
            Outcome::Committed(Done),
        );
    }

    #[test]
    fn an_unclassifiable_quiet_screen_settles_working_to_done() {
        // The quiet timer surfaces a bare `Done` reading when the resting
        // screen classifies as nothing (weak Codex/Cursor detection, #225).
        // From Working it settles to Done...
        let mut m = machine();
        assert_eq!(
            m.on_reading(Some(Working), clear(Done)),
            Outcome::Committed(Done),
        );
    }

    #[test]
    fn a_bare_done_reading_never_fabricates_a_finished_turn() {
        // ...but from any state that never worked it carries no evidence and
        // is held: a never-worked Idle stays Idle, a live `?` stays asking,
        // a fresh session stays blank.
        let mut m = machine();
        assert_eq!(m.on_reading(Some(Idle), clear(Done)), Outcome::Rejected);
        assert_eq!(
            m.on_reading(Some(InputNeeded), clear(Done)),
            Outcome::Rejected,
        );
        assert_eq!(m.on_reading(None, clear(Done)), Outcome::Rejected);
        // A Done reading while already Done is a plain dedupe.
        assert_eq!(m.on_reading(Some(Done), clear(Done)), Outcome::Unchanged);
    }

    #[test]
    fn a_fresh_idle_settle_is_not_a_false_done() {
        // A resting composer reached from Idle/None (never worked) stays
        // Idle — the promotion only fires from Working.
        let mut m = machine();
        assert_eq!(m.on_reading(None, clear(Idle)), Outcome::Committed(Idle));
        assert_eq!(m.on_reading(Some(Idle), clear(Idle)), Outcome::Unchanged);
    }

    #[test]
    fn ambiguous_idle_never_leaves_working() {
        // The only Idle reading the machine ever sees is the clear
        // quiet-classification (byte flow only ever reads Working). But even
        // a hypothetical ambiguous Idle can't demote Working: it's the
        // forbidden edge, structurally rejected.
        let mut m = machine();
        assert_eq!(
            m.on_reading(Some(Working), ambiguous(Idle)),
            Outcome::Rejected,
        );
    }

    // ── InputNeeded stickiness (#374) ─────────────────────────────

    #[test]
    fn ambiguous_working_never_clears_input_needed() {
        // A parked prompt emits no output, so an ambiguous byte-flow
        // `Working` reaching it is an incidental repaint (a click, a focus,
        // a redraw) — it must NEVER clear the `?`, no matter how long the
        // prompt has been up.
        let mut m = machine();
        assert_eq!(
            m.on_reading(Some(Working), clear(InputNeeded)),
            Outcome::Committed(InputNeeded)
        );
        assert_eq!(
            m.on_reading(Some(InputNeeded), ambiguous(Working)),
            Outcome::Damped
        );
    }

    #[test]
    fn a_clear_reading_may_leave_input_needed() {
        // Affirmative evidence the agent moved on — a clear `Working` (a
        // recognized live status line) or a clear `Idle` (a resting
        // composer, the ask resolved) — is honored immediately.
        let mut m = machine();
        assert_eq!(
            m.on_reading(Some(Working), clear(InputNeeded)),
            Outcome::Committed(InputNeeded)
        );
        assert_eq!(
            m.on_reading(Some(InputNeeded), clear(Working)),
            Outcome::Committed(Working)
        );
        // And a clear Idle resolves it back to a resting composer.
        assert_eq!(
            m.on_reading(Some(Working), clear(InputNeeded)),
            Outcome::Committed(InputNeeded)
        );
        assert_eq!(
            m.on_reading(Some(InputNeeded), clear(Idle)),
            Outcome::Committed(Idle)
        );
    }

    #[test]
    fn a_live_dialog_mid_work_surfaces_immediately() {
        let mut m = machine();
        assert_eq!(
            m.on_reading(Some(Idle), clear(Working)),
            Outcome::Committed(Working)
        );
        assert_eq!(
            m.on_reading(Some(Working), ambiguous(InputNeeded)),
            Outcome::Committed(InputNeeded)
        );
    }

    // ── Done stickiness ───────────────────────────────────────────

    #[test]
    fn done_survives_an_idle_reading_but_not_progress() {
        let mut m = machine();
        // A bare idle reading can't clear Done (a structural rejection).
        assert_eq!(m.on_reading(Some(Done), clear(Idle)), Outcome::Rejected);
        assert_eq!(m.on_reading(Some(Done), ambiguous(Idle)), Outcome::Rejected);
        // Real progress and a fresh prompt do.
        assert_eq!(
            m.on_reading(Some(Done), clear(Working)),
            Outcome::Committed(Working)
        );
        assert_eq!(
            m.on_reading(Some(Done), clear(InputNeeded)),
            Outcome::Committed(InputNeeded)
        );
    }

    #[test]
    fn ambiguous_working_cannot_clear_done() {
        // A byte-flow Working (a stray repaint) must be held: only a CLEAR
        // Working — a live status line classified on a quiet screen — is
        // real progress. No time window: it stays held arbitrarily.
        let mut m = machine();
        assert_eq!(
            m.on_reading(Some(Done), ambiguous(Working)),
            Outcome::Damped
        );
        assert_eq!(
            m.on_reading(Some(Done), clear(Working)),
            Outcome::Committed(Working)
        );
    }

    // ── the progress-streak exit from Done (#398) ─────────────────

    #[test]
    fn sustained_progress_reopens_working_from_done() {
        // Several chunks that each changed the meaningful content are a
        // resumed turn — the one exit a heavy stream can take, since it
        // never pauses long enough for a quiet classification.
        let mut m = machine();
        assert_eq!(m.on_reading(Some(Done), progressing()), Outcome::Damped);
        assert_eq!(m.on_reading(Some(Done), progressing()), Outcome::Damped);
        assert_eq!(
            m.on_reading(Some(Done), progressing()),
            Outcome::Committed(Working),
            "the third progressing reading is the resume event",
        );
    }

    #[test]
    fn churn_never_reopens_working_from_done() {
        // Spinner frames and counter ticks (no fingerprint change) are
        // held forever, interleaved or not — they don't advance the
        // streak, but they don't reset it either (a busy stream mixes
        // content and churn chunks).
        let mut m = machine();
        for _ in 0..10 {
            assert_eq!(
                m.on_reading(Some(Done), ambiguous(Working)),
                Outcome::Damped
            );
        }
        assert_eq!(m.on_reading(Some(Done), progressing()), Outcome::Damped);
        assert_eq!(
            m.on_reading(Some(Done), ambiguous(Working)),
            Outcome::Damped
        );
        assert_eq!(m.on_reading(Some(Done), progressing()), Outcome::Damped);
        assert_eq!(
            m.on_reading(Some(Done), progressing()),
            Outcome::Committed(Working),
        );
    }

    #[test]
    fn a_quiet_classification_resets_the_progress_streak() {
        // A stray full-screen redraw can fragment into a couple of
        // distinct chunks; the settle that follows (any clear reading)
        // restarts the streak, so repaint fragments can never
        // accumulate across rests into a false resume.
        let mut m = machine();
        assert_eq!(m.on_reading(Some(Done), progressing()), Outcome::Damped);
        assert_eq!(m.on_reading(Some(Done), progressing()), Outcome::Damped);
        // The redraw ends; the quiet timer classifies the resting
        // composer — rejected against Done, but it settles the screen.
        assert_eq!(m.on_reading(Some(Done), clear(Idle)), Outcome::Rejected);
        assert_eq!(m.on_reading(Some(Done), progressing()), Outcome::Damped);
        assert_eq!(m.on_reading(Some(Done), progressing()), Outcome::Damped);
        assert_eq!(
            m.on_reading(Some(Done), progressing()),
            Outcome::Committed(Working),
        );
    }

    #[test]
    fn the_progress_streak_never_unasks_a_parked_prompt() {
        // #374: `?` yields to affirmative evidence only. A parked prompt
        // emits nothing, so a progressing reading reaching InputNeeded is
        // repaint fallout, not an answer — no streak exit exists there.
        let mut m = machine();
        for _ in 0..10 {
            assert_eq!(
                m.on_reading(Some(InputNeeded), progressing()),
                Outcome::Damped
            );
        }
    }

    #[test]
    fn the_progress_streak_only_counts_while_done() {
        // Progress observed in other states must not bank toward the
        // Done exit: two progressing chunks while Working, a settle to
        // Done, then one progressing chunk must NOT flip Done back.
        let mut m = machine();
        assert_eq!(
            m.on_reading(Some(Working), progressing()),
            Outcome::Unchanged
        );
        assert_eq!(
            m.on_reading(Some(Working), progressing()),
            Outcome::Unchanged
        );
        assert_eq!(
            m.on_reading(Some(Working), clear(Idle)),
            Outcome::Committed(Done)
        );
        assert_eq!(m.on_reading(Some(Done), progressing()), Outcome::Damped);
    }

    /// The end-to-end hookless lifecycle: boot (held) → ready → work →
    /// settle to Done → new turn → crash. No step ever lands on `Idle`
    /// after work, and no false `Done` appears at boot.
    #[test]
    fn hookless_lifecycle_never_regresses_to_idle() {
        let mut m = AgentStateMachine::new();
        // Boot bytes are held.
        assert_eq!(m.on_reading(None, ambiguous(Working)), Outcome::Damped);
        // Composer settles → Idle (boot complete, never worked).
        assert_eq!(m.on_reading(None, clear(Idle)), Outcome::Committed(Idle));
        // A real turn begins (byte flow, now booted).
        assert_eq!(
            m.on_reading(Some(Idle), ambiguous(Working)),
            Outcome::Committed(Working)
        );
        // The turn ends at a resting composer → Done (not Idle).
        assert_eq!(
            m.on_reading(Some(Working), clear(Idle)),
            Outcome::Committed(Done)
        );
        // Sitting at the composer keeps Done.
        assert_eq!(m.on_reading(Some(Done), clear(Idle)), Outcome::Rejected);
        // A new turn.
        assert_eq!(
            m.on_reading(Some(Done), ambiguous(Working)),
            Outcome::Damped, // a churn-only byte-flow Working is held against Done
        );
        assert_eq!(
            m.on_reading(Some(Done), clear(Working)),
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
