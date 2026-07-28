//! Machine-checked proof of the agent state machine's load-bearing
//! invariants.
//!
//! The unit tests in `state_machine.rs` assert individual, hand-picked
//! edges ("a wedged status line settles to Done", "an ambiguous Working
//! never clears InputNeeded"). Those are examples. This suite asserts the
//! *invariants those examples are instances of* hold across the **entire**
//! reachable input space — every current state, folding every reading, in
//! every order — rather than the handful a human thought to write down.
//!
//! Getting agent status right (Working / InputNeeded / Idle / Done /
//! Exited) is the product's fundamental correctness property, and the fold
//! logic in [`AgentStateMachine`] is where it can silently rot: a future
//! edit that makes a working spinner blank to no pill, un-asks a parked
//! prompt on a stray repaint, or resurrects an exited terminal would pass
//! every example test that didn't happen to cover it. Here it fails a
//! build instead.
//!
//! The input alphabet is finite and small, so this is genuine exhaustive
//! enumeration — a proof over all histories up to a bound — not sampled
//! property testing. Four safety invariants, each independently re-derived
//! from the wire contract (never leaning on the implementation's own
//! `transition_allowed` predicate, so a bug there can't hide a bug here):
//!
//!   - **INV1 — the one-way door.** A committed move is never
//!     `Working → Idle` or `Done → Idle`. A settled worker has *finished a
//!     turn* (`Done`); it never reverts to the never-worked `Idle`.
//!   - **INV2 — an absorbing exit.** Nothing commits a change out of
//!     `Exited`. The process is gone; late readings and hooks are stale.
//!   - **INV3 — no fabricated Done.** The PTY path only ever commits `Done`
//!     from `Working` (a settled turn). It never manufactures a finished
//!     turn from a never-worked `Idle`, an unknown fresh session, or a
//!     live `?`.
//!   - **INV4 — `InputNeeded` yields only to evidence.** An *ambiguous*
//!     reading (a stray repaint from a click/focus/redraw) never clears a
//!     parked prompt, no matter how long it has been up (#374). The sole
//!     exception is the one carve-out the machine documents: a stale-hook
//!     `Working` reading backed by `supersedes_dialog` evidence.

use lazybox_agents::{
    AgentState, AgentStateMachine, HOOK_STALENESS, Liveness, Outcome, PtyReading,
};
use std::time::Duration;

use AgentState::{Done, Idle, InputNeeded, Working};

const EXITED: AgentState = AgentState::Exited { code: Some(0) };

/// Every state a terminal's cache can hold.
const STATES: [AgentState; 5] = [Working, InputNeeded, Idle, Done, EXITED];

/// The states a PTY *reading* can carry. A detector reports Working /
/// InputNeeded / Idle, and the quiet timer surfaces a bare `Done`; a
/// reading never carries `Exited` (that is teardown-only, committed
/// through `transition` directly).
const READING_STATES: [AgentState; 4] = [Working, InputNeeded, Idle, Done];

const FRESH: Duration = Duration::from_secs(1);
const STALE: Duration = HOOK_STALENESS; // exactly at the threshold counts as stale

/// The forbidden edges, re-stated straight from the wire contract so this
/// check never consults the implementation it is policing. A committed
/// `from → to` must satisfy all of these.
fn edge_is_legal(from: AgentState, to: AgentState) -> bool {
    // INV3-adjacent structural rules that hold for *every* commit path:
    from != to // a self-loop is not a transition
        && !matches!(from, AgentState::Exited { .. }) // INV2: exit absorbs
        && !matches!((from, to), (Working, Idle) | (Done, Idle)) // INV1: one-way door
}

/// Assert the four invariants on a single committed move `from → to`,
/// given the reading that produced it. `from` is `None` for the first
/// reading on a never-reported terminal — a legal establishing commit with
/// no prior edge, but INV3 still binds it (a fresh session must not
/// fabricate a `Done`). `via_supersede` records whether the machine was
/// allowed to treat an ambiguous reading as authoritative on this step (the
/// documented stale-hook dialog-supersession carve-out).
fn assert_commit_is_legal(
    from: Option<AgentState>,
    to: AgentState,
    reading: PtyReading,
    via_supersede: bool,
) {
    if let Some(from) = from {
        assert!(
            edge_is_legal(from, to),
            "illegal committed edge {from:?} → {to:?} (INV1/INV2/INV3-self-loop)",
        );
        // INV4: an ambiguous reading never leaves a parked prompt, save the
        // one documented carve-out.
        if from == InputNeeded && to != InputNeeded && !reading.clear {
            assert!(
                via_supersede,
                "an ambiguous reading cleared InputNeeded → {to:?} without supersession evidence (#374)",
            );
        }
    }
    // INV3: the PTY path only ever commits `Done` from a `Working` turn —
    // never fabricates a finished turn from Idle, an unknown fresh session
    // (`None`), or a live `?`.
    if to == Done {
        assert_eq!(
            from,
            Some(Working),
            "PTY committed Done from {from:?}, not Working — a fabricated finished turn",
        );
    }
}

/// Build a `PtyReading` from its five independent facts.
fn reading(
    state: AgentState,
    clear: bool,
    progress: bool,
    liveness: Liveness,
    ready: bool,
) -> PtyReading {
    PtyReading {
        state,
        clear,
        progress,
        liveness,
        ready_for_prompt: ready,
    }
}

/// The current-state axis for single-step enumeration: every cache value,
/// plus the never-reported `None`.
const CURRENTS: [Option<AgentState>; 6] = [
    None,
    Some(Working),
    Some(InputNeeded),
    Some(Idle),
    Some(Done),
    Some(EXITED),
];

const LIVENESSES: [Liveness; 4] = [
    Liveness::Streaming,
    Liveness::Silent,
    Liveness::Stalled,
    Liveness::Watchdog,
];
const HOOK_AGES: [Option<Duration>; 3] = [None, Some(FRESH), Some(STALE)];

/// **Exhaustive single-step check of `on_pty_reading`.**
///
/// Every `(current, reading, hook-age, supersedes, booted)` tuple the pump
/// can ever hand the machine — the full gate + hysteresis surface — folded
/// once, asserting the commit (if any) is a legal move. This is the whole
/// input space of one fold; nothing is sampled.
#[test]
fn every_single_pty_fold_is_a_legal_move() {
    let mut folds = 0u64;
    for &current in &CURRENTS {
        for &state in &READING_STATES {
            for clear in [false, true] {
                for progress in [false, true] {
                    for &liveness in &LIVENESSES {
                        for ready in [false, true] {
                            for &since in &HOOK_AGES {
                                for supersedes in [false, true] {
                                    for booted in [false, true] {
                                        let pty = reading(state, clear, progress, liveness, ready);
                                        let mut m = AgentStateMachine::new();
                                        if booted {
                                            m.mark_booted();
                                        }
                                        let outcome =
                                            m.on_pty_reading(current, pty, since, || supersedes);
                                        folds += 1;
                                        if let Outcome::Committed(to) = outcome {
                                            // The carve-out is only reachable at/above
                                            // staleness, demoting a `?` to Working.
                                            let via_supersede = since
                                                .is_some_and(|s| s >= HOOK_STALENESS)
                                                && current == Some(InputNeeded)
                                                && state == Working
                                                && supersedes;
                                            assert_commit_is_legal(current, to, pty, via_supersede);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // Guard against the loop silently collapsing to nothing.
    assert_eq!(folds, 6 * 4 * 2 * 2 * 4 * 2 * 3 * 2 * 2);
}

/// **Exhaustive check of the direct `transition` table.**
///
/// Hooks, the optimistic answer-flip, and PTY-exit teardown all commit
/// through `AgentStateMachine::transition` rather than the reading fold.
/// Enumerate every `(from, to)` — including the never-reported `None`
/// origin — and assert its structural verdict matches the wire contract.
#[test]
fn every_direct_transition_matches_the_contract() {
    for from in STATES.into_iter().map(Some).chain([None]) {
        for to in STATES {
            let committed = AgentStateMachine::transition(from, to);
            match from {
                // A never-reported terminal accepts any first state.
                None => assert_eq!(committed, Some(to)),
                Some(f) if edge_is_legal(f, to) => {
                    assert_eq!(committed, Some(to), "legal {f:?} → {to:?} must commit",)
                }
                Some(f) => assert_eq!(committed, None, "illegal {f:?} → {to:?} must be held"),
            }
        }
    }
}

/// The reading alphabet for sequence enumeration: the pure-PTY fold
/// (`since_last_hook = None`, so the gate is a no-op and every symbol
/// routes through the full hysteresis). Sixteen symbols — state × clear ×
/// progress — is enough to drive the boot latch, the settle promotion, the
/// sticky-exit damping, and the Done progress streak.
fn sequence_alphabet() -> Vec<PtyReading> {
    let mut v = Vec::new();
    for &state in &READING_STATES {
        for clear in [false, true] {
            for progress in [false, true] {
                v.push(reading(state, clear, progress, Liveness::Streaming, false));
            }
        }
    }
    v
}

/// Replay one reading sequence on a fresh machine, tracking the cache
/// (`current`) exactly as the pump does — passing it into each fold and
/// advancing it on every commit — and assert every committed step is a
/// legal move. Returns nothing; it panics on the first violation.
fn replay_and_check(alphabet: &[PtyReading], indices: &[usize], booted: bool) {
    let mut m = AgentStateMachine::new();
    if booted {
        m.mark_booted();
    }
    let mut current: Option<AgentState> = None;
    for &i in indices {
        let pty = alphabet[i];
        if let Outcome::Committed(to) = m.on_pty_reading(current, pty, None, || false) {
            // No hooks in this path, so the supersession carve-out is
            // structurally unreachable: INV4 holds in its strict form.
            assert_commit_is_legal(current, to, pty, false);
            current = Some(to);
        }
    }
}

/// **Exhaustive check that no *history* violates an invariant.**
///
/// Every reading sequence up to [`MAX_LEN`] over the 16-symbol alphabet,
/// replayed from both a booted and an unbooted machine. Where the
/// single-step test proves one fold is safe, this proves the machine's
/// *stateful* accumulation — the boot latch and the Done progress streak,
/// which only misbehave across several folds — never reaches an illegal
/// commit either.
#[test]
fn no_reading_history_reaches_an_illegal_state() {
    const MAX_LEN: usize = 5;
    let alphabet = sequence_alphabet();
    let n = alphabet.len();
    let mut sequences = 0u64;
    for len in 1..=MAX_LEN {
        // An odometer over base-`n` digits enumerates every length-`len`
        // sequence without allocating the full product.
        let mut odometer = vec![0usize; len];
        loop {
            replay_and_check(&alphabet, &odometer, false);
            replay_and_check(&alphabet, &odometer, true);
            sequences += 1;
            // Increment the odometer; break when it wraps.
            let mut pos = 0;
            loop {
                if pos == len {
                    // fully wrapped → done with this length
                    odometer[0] = usize::MAX; // sentinel to break outer
                    break;
                }
                odometer[pos] += 1;
                if odometer[pos] < n {
                    break;
                }
                odometer[pos] = 0;
                pos += 1;
            }
            if odometer.first() == Some(&usize::MAX) {
                break;
            }
        }
    }
    // n + n^2 + ... + n^MAX_LEN — a cheap check the enumeration ran.
    let expected: u64 = (1..=MAX_LEN as u32).map(|k| (n as u64).pow(k)).sum();
    assert_eq!(sequences, expected);
}

/// **A full-lifecycle golden timeline.** One realistic Claude session that
/// exercises hooks, PTY readings, the hooks-primary gate, the byte-silent
/// settle carve-out, the Done→Working progress streak, and the absorbing
/// exit — all in one run, in order — asserting the emitted state at each
/// step. Where the exhaustive tests prove *no* history misbehaves, this
/// pins the shape of the *intended* one: the sequence the pipeline is
/// built to produce end to end.
///
/// The model mirrors the daemon: hooks and the answer-flip commit through
/// the static `transition`; PTY readings fold through the instance. Both
/// write the same `current` cache.
#[test]
fn a_full_hook_and_pty_session_walks_the_intended_timeline() {
    let mut m = AgentStateMachine::new();
    let mut current: Option<AgentState> = None;

    // A hook arrives: fold it through the static transition, mirroring the
    // daemon's `transition_and_broadcast_agent_state`.
    let hook = |current: &mut Option<AgentState>, to: AgentState| {
        if let Some(next) = AgentStateMachine::transition(*current, to) {
            *current = Some(next);
        }
    };

    // 1. SessionStart → a never-worked composer.
    hook(&mut current, Idle);
    assert_eq!(current, Some(Idle), "SessionStart → Idle");
    m.mark_booted(); // the "ready for prompt" signal opens the boot gate.

    // 2. The user submits a prompt (UserPromptSubmit hook) → Working.
    hook(&mut current, Working);
    assert_eq!(current, Some(Working), "a submitted prompt → Working");

    // 3. Bytes stream while the hook is fresh: the gate hands the
    //    Working↔Idle call to hooks, so the byte-flow reading is suppressed.
    let streaming = reading(Working, false, true, Liveness::Streaming, false);
    assert_eq!(
        m.on_pty_reading(current, streaming, Some(FRESH), || false),
        Outcome::Gated,
        "a fresh hook owns Working; the byte-flow reading is gated",
    );
    assert_eq!(current, Some(Working));

    // 4. A mid-turn approval dialog paints. No hook fires for an inline
    //    approval, so the rendered `?` is the only source of truth — the
    //    gate lets an InputNeeded correction through even while fresh.
    let dialog = reading(InputNeeded, false, false, Liveness::Streaming, false);
    if let Outcome::Committed(to) = m.on_pty_reading(current, dialog, Some(FRESH), || false) {
        current = Some(to);
    }
    assert_eq!(
        current,
        Some(InputNeeded),
        "an inline dialog surfaces the `?`"
    );

    // 5. The user answers: the optimistic flip commits Working directly.
    hook(&mut current, Working);
    assert_eq!(current, Some(Working), "answering resumes Working");

    // 6. The turn ends WITHOUT a Stop hook (a lost hook / manual settle),
    //    but the composer keeps painting. True byte-silence is
    //    authoritative that output stopped, so the Silent settle closes the
    //    turn to Done even under a still-fresh hook — the gap-1 fail-safe.
    let silent_idle = reading(Idle, true, false, Liveness::Silent, true);
    if let Outcome::Committed(to) = m.on_pty_reading(current, silent_idle, Some(FRESH), || false) {
        current = Some(to);
    }
    assert_eq!(
        current,
        Some(Done),
        "byte-silence settles a hookless turn-end to Done"
    );

    // 7. A new turn resumes and streams heavily (hooks now stale). The
    //    quiet classifier can't catch a stream that never pauses, so a
    //    streak of progressing chunks is the resume signal: the third
    //    re-opens Working.
    let progressing = reading(Working, false, true, Liveness::Streaming, false);
    let mut timeline_after_resume = Vec::new();
    for _ in 0..3 {
        if let Outcome::Committed(to) =
            m.on_pty_reading(current, progressing, Some(STALE), || false)
        {
            current = Some(to);
        }
        timeline_after_resume.push(current);
    }
    assert_eq!(
        timeline_after_resume,
        vec![Some(Done), Some(Done), Some(Working)],
        "the third progressing chunk re-opens Working from Done (#398)",
    );

    // 8. The process exits mid-turn. Exit commits from any live state.
    if let Some(next) = AgentStateMachine::transition(current, EXITED) {
        current = Some(next);
    }
    assert_eq!(current, Some(EXITED), "a live turn can always exit");

    // 9. A late Stop hook races in after teardown: the exit absorbs it.
    hook(&mut current, Done);
    assert_eq!(
        current,
        Some(EXITED),
        "Exited is terminal; a late hook can't resurrect it"
    );
}
