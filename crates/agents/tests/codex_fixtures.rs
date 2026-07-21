//! Real-byte Codex detector fixtures.
//!
//! Each `include_bytes!` is a raw PTY transcript captured from a genuine
//! `codex` session (0.142 for the original corpus, 0.144.6 for the
//! `codex_real_approval_*` phases) — real CSI/SGR escapes, cursor-positioned
//! status bar, the U+203A chooser arrow, the U+2022 spinner bullet and the
//! U+00B7 footer separator. Codex renders INLINE (no alt-screen) and animates
//! its "Working" spinner with heavy cursor churn, so the wire shape looks
//! nothing like the hand-typed strings in `agents.rs`; this suite is what
//! keeps the `detect::codex_state*` family honest against it.
//!
//! Capture recipe (a PTY harness that drives `codex`, sends a prompt, and
//! dumps the raw bytes per phase) is described in the PR for issue #225; the
//! approval round-trip phases were sliced from one continuous
//! `tmux pipe-pane` capture of a live command approval (issue #399).
//! Acceptance: the state pill must match reality for idle, working, the
//! command/edit approval modals, the directory-trust gate, and the
//! finished-turn idle screen that must evict a now-stale `esc to interrupt`.

use lazybox_agents::detect::{
    codex_input_needed_in_current_chunk, codex_ready_for_prompt, codex_ready_for_prompt_chunked,
    codex_state, codex_state_chunked,
};
use lazybox_agents::{AgentState, PromptShape};

struct ByteFixture {
    name: &'static str,
    bytes: &'static [u8],
    expected: AgentState,
    /// Whether the spawn-time injector should consider Codex ready to receive
    /// a pasted prompt — `true` only for a resting composer with no modal up.
    ready: bool,
}

const FIXTURES: &[ByteFixture] = &[
    // Resting composer right after boot: the input box, a rotating
    // placeholder (`›Improve documentation in @filename`) and the
    // `<model> <effort> · <cwd>` footer. A stale `esc to interrupt` from the
    // MCP-server boot still sits earlier in the window; the footer painted
    // after it must win → Idle AND ready.
    ByteFixture {
        name: "codex_real_idle",
        bytes: include_bytes!("fixtures/codex_real_idle.bin"),
        expected: AgentState::Idle,
        ready: true,
    },
    // A genuinely busy session: the `• Working (Ns · esc to interrupt)`
    // status line plus the animated spinner. The `esc to interrupt` anchor is
    // more recent than the composer footer → Working, never ready.
    ByteFixture {
        name: "codex_real_working",
        bytes: include_bytes!("fixtures/codex_real_working.bin"),
        expected: AgentState::Working,
        ready: false,
    },
    // A finished turn: the answer, then the composer + footer repainted
    // BELOW the now-stale `esc to interrupt`. The more-recent footer evicts
    // the stale working anchor → Idle AND ready (the regression that kept a
    // finished Codex stuck looking busy — or here, the inverse guard).
    ByteFixture {
        name: "codex_real_done_idle",
        bytes: include_bytes!("fixtures/codex_real_done_idle.bin"),
        expected: AgentState::Idle,
        ready: true,
    },
    // A live command-approval modal: `Would you like to run the following
    // command?`, the `› 1. Yes, proceed (y)` chooser, and the
    // `Press enter to confirm or esc to cancel` footer — all more recent than
    // the `Running … (esc to interrupt)` line above them → InputNeeded.
    ByteFixture {
        name: "codex_real_command_approval",
        bytes: include_bytes!("fixtures/codex_real_command_approval.bin"),
        expected: AgentState::InputNeeded,
        ready: false,
    },
    // A live file-edit approval modal: `Would you like to make the following
    // edits?` + the same chooser / confirm chrome. Exercises the shared
    // "would you like to" phrase stem.
    ByteFixture {
        name: "codex_real_edit_approval",
        bytes: include_bytes!("fixtures/codex_real_edit_approval.bin"),
        expected: AgentState::InputNeeded,
        ready: false,
    },
    // The directory-trust gate Codex shows on first launch in an untrusted
    // worktree: `Do you trust the contents of this directory?` + a
    // `› 1. Yes, continue` chooser + `Press enter to continue`. Must veto the
    // ready signal so a spawn-time paste can't land in it.
    ByteFixture {
        name: "codex_real_trust",
        bytes: include_bytes!("fixtures/codex_real_trust.bin"),
        expected: AgentState::InputNeeded,
        ready: false,
    },
    // The 0.144.6 command-approval chrome (the older approval fixtures are
    // 0.142): the burst that paints `Would you like to run the following
    // command?` + the three-option chooser mid-turn, working spinner still
    // churning above it.
    ByteFixture {
        name: "codex_real_approval_paint_burst",
        bytes: include_bytes!("fixtures/codex_real_approval_paint_burst.bin"),
        expected: AgentState::InputNeeded,
        ready: false,
    },
    // The screen at rest after that approval round-trip completed: resting
    // composer + footer, the turn's transcript above.
    ByteFixture {
        name: "codex_real_approval_settled_idle",
        bytes: include_bytes!("fixtures/codex_real_approval_settled_idle.bin"),
        expected: AgentState::Idle,
        ready: true,
    },
];

#[test]
fn codex_detector_matches_real_byte_corpus() {
    let mut failures: Vec<String> = Vec::new();
    for f in FIXTURES {
        let actual = codex_state(f.bytes);
        if actual != Some(f.expected) {
            failures.push(format!(
                "fixture `{}` expected {:?} but got {:?}",
                f.name, f.expected, actual,
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} real-byte Codex fixtures failed:\n{}",
        failures.len(),
        FIXTURES.len(),
        failures.join("\n"),
    );
}

#[test]
fn codex_readiness_matches_real_byte_corpus() {
    let mut failures: Vec<String> = Vec::new();
    for f in FIXTURES {
        let actual = codex_ready_for_prompt(f.bytes);
        if actual != f.ready {
            failures.push(format!(
                "fixture `{}` expected ready={} but got {}",
                f.name, f.ready, actual,
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} real-byte Codex readiness checks failed:\n{}",
        failures.len(),
        FIXTURES.len(),
        failures.join("\n"),
    );
}

/// The same corpus fed to the chunk-aware detector as if the WHOLE transcript
/// arrived in one PTY chunk (`last_chunk_start = 0`) — the tmux full-screen
/// repaint the live daemon path sees right after spawn. Every fixture must
/// classify identically: the same-chunk relaxation only loosens the work
/// anchor's suppression of a prompt, and Codex's prompt chrome is already the
/// bottom-most marker, so the result must not change.
#[test]
fn codex_detector_matches_real_byte_corpus_as_full_repaint() {
    let mut failures: Vec<String> = Vec::new();
    for f in FIXTURES {
        let actual = codex_state_chunked(f.bytes, 0);
        if actual != Some(f.expected) {
            failures.push(format!(
                "fixture `{}` expected {:?} but got {:?}",
                f.name, f.expected, actual,
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} real-byte Codex fixtures failed as a single repaint chunk:\n{}",
        failures.len(),
        FIXTURES.len(),
        failures.join("\n"),
    );
}

/// A full command-approval round-trip captured phase-by-phase from one live
/// codex 0.144.6 session (issue #399), replayed against the per-chunk
/// detector the daemon runs on every PTY read. A parked approval modal keeps
/// repainting its spinner ticks, so the PTY never stays quiet long enough
/// for the resting-screen classifier — the current-chunk detector is the
/// only thing that can surface the `?`, and it must fire off the paint burst
/// alone, then stay silent for every later phase (the answered modal lingers
/// in the window's scrollback for all of them).
#[test]
fn codex_real_approval_round_trip_drives_the_chunk_detector() {
    let working = include_bytes!("fixtures/codex_real_working.bin");
    let paint = include_bytes!("fixtures/codex_real_approval_paint_burst.bin");
    let ticks = include_bytes!("fixtures/codex_real_approval_parked_ticks.bin");
    let answered = include_bytes!("fixtures/codex_real_approval_answered.bin");
    let settled = include_bytes!("fixtures/codex_real_approval_settled_idle.bin");

    // Phase 1 — the modal paint burst arrives while the working screen is
    // already in the window: the chunk detector fires on that burst itself,
    // with no quiet wait.
    let mut buf = working.to_vec();
    let paint_start = buf.len();
    buf.extend_from_slice(paint);
    assert_eq!(
        codex_input_needed_in_current_chunk(&buf, paint_start),
        Some(PromptShape::Chooser),
        "the approval paint burst must surface InputNeeded immediately",
    );
    assert_eq!(
        codex_state_chunked(&buf, paint_start),
        Some(AgentState::InputNeeded)
    );

    // Phase 2 — the parked modal repaints only its spinner ticks; they never
    // retouch the modal markers, so the chunk detector must not re-fire, and
    // the resting classification still reads the parked modal as asking.
    let ticks_start = buf.len();
    buf.extend_from_slice(ticks);
    assert_eq!(codex_input_needed_in_current_chunk(&buf, ticks_start), None);
    assert_eq!(codex_state(&buf), Some(AgentState::InputNeeded));

    // Phase 3 — the user answered: the aftermath (`✔ You approved codex to
    // run …`, the tool run, a fresh working line, the repainted footer)
    // streams in while the answered modal still sits in scrollback. Neither
    // detector may re-surface it.
    let answered_start = buf.len();
    buf.extend_from_slice(answered);
    assert_eq!(
        codex_input_needed_in_current_chunk(&buf, answered_start),
        None,
        "an answered modal lingering in scrollback must not re-fire off fresh output",
    );
    assert_ne!(
        codex_state(&buf),
        Some(AgentState::InputNeeded),
        "the post-answer screen must not classify as still asking",
    );

    // Phase 4 — the turn settles: composer + footer at rest → Idle and ready,
    // answered modal still in the window the whole time.
    buf.extend_from_slice(settled);
    assert_eq!(codex_state(&buf), Some(AgentState::Idle));
    assert!(codex_ready_for_prompt(&buf));
}

/// The chunk-aware readiness detector fed each capture as one full repaint
/// (`last_chunk_start = 0`) must agree with the whole corpus's per-fixture
/// `ready` expectation: the repaint-frame fast path may only accelerate the
/// verdict, never flip a live modal, a working screen, or the trust gate
/// into "ready".
#[test]
fn codex_chunked_readiness_matches_real_byte_corpus_as_full_repaint() {
    let mut failures: Vec<String> = Vec::new();
    for f in FIXTURES {
        let actual = codex_ready_for_prompt_chunked(f.bytes, 0);
        if actual != f.ready {
            failures.push(format!(
                "fixture `{}` expected chunked ready={} but got {}",
                f.name, f.ready, actual,
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} real-byte chunked readiness checks failed:\n{}",
        failures.len(),
        FIXTURES.len(),
        failures.join("\n"),
    );
}

/// The #425 stall shape, replayed deterministically: Codex's diff renderer
/// keeps landing status-line fragments after the last full footer paint, so
/// the whole-buffer positional read stays pinned on Working and the ready
/// signal never fires — the spawn-time inject then rides its 10s hard
/// deadline and the unbounded pending-ready park. A placeholder-rotation
/// frame (`› Summarize recent commits` — composer chrome Codex repaints
/// every few seconds while the composer is usable) must fire the chunk-aware
/// readiness on that frame, even though the whole-buffer read is still
/// pinned.
#[test]
fn codex_placeholder_rotation_frame_unpins_readiness_from_stale_working_bytes() {
    let working = include_bytes!("fixtures/codex_real_working.bin");
    let mut buf = working.to_vec();
    // Genuinely busy screen: neither detector may report ready.
    assert!(!codex_ready_for_prompt(&buf));
    assert!(!codex_ready_for_prompt_chunked(&buf, 0));

    // Diff-render spinner churn: tiny frames with no composer chrome. The
    // chunk-aware detector must keep falling back to the (still-pinned)
    // positional read.
    for _ in 0..3 {
        let start = buf.len();
        buf.extend_from_slice(b"\x1b[2K\xe2\x80\xa2 spin");
        assert!(!codex_ready_for_prompt_chunked(&buf, start));
    }

    // The placeholder-rotation frame: composer arrow + prompt text, no
    // footer repaint (the diff renderer repaints only the changed line).
    let start = buf.len();
    buf.extend_from_slice("\x1b[2K\x1b[7m\u{203a}\x1b[0m Summarize recent commits".as_bytes());
    assert!(
        !codex_ready_for_prompt(&buf),
        "the whole-buffer positional read must still be pinned by the stale working bytes \
         (otherwise this test no longer demonstrates the #425 stall)",
    );
    assert!(
        codex_ready_for_prompt_chunked(&buf, start),
        "the composer-rotation frame must fire chunk-aware readiness",
    );
}

/// The chunk-aware fast path must never become a hole for modals: the trust
/// gate and a parked approval (paint burst + spinner-only ticks) arriving as
/// the current frame all stay not-ready.
#[test]
fn codex_chunked_readiness_never_fires_off_modal_frames() {
    let working = include_bytes!("fixtures/codex_real_working.bin");
    let trust = include_bytes!("fixtures/codex_real_trust.bin");
    let paint = include_bytes!("fixtures/codex_real_approval_paint_burst.bin");
    let ticks = include_bytes!("fixtures/codex_real_approval_parked_ticks.bin");

    let mut buf = working.to_vec();
    let trust_start = buf.len();
    buf.extend_from_slice(trust);
    assert!(!codex_ready_for_prompt_chunked(&buf, trust_start));

    let mut buf = working.to_vec();
    let paint_start = buf.len();
    buf.extend_from_slice(paint);
    assert!(!codex_ready_for_prompt_chunked(&buf, paint_start));
    let ticks_start = buf.len();
    buf.extend_from_slice(ticks);
    assert!(!codex_ready_for_prompt_chunked(&buf, ticks_start));
}

/// The fixtures must actually carry ANSI escape bytes — otherwise this suite
/// would silently degrade into another synthetic-string test and stop
/// guarding the wire shape it exists for.
#[test]
fn codex_fixtures_contain_real_ansi_escapes() {
    for f in FIXTURES {
        assert!(
            f.bytes.contains(&0x1b),
            "fixture `{}` has no ESC byte — it isn't a real PTY transcript",
            f.name,
        );
    }
}
