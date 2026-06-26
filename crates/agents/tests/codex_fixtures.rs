//! Real-byte Codex detector fixtures.
//!
//! Each `include_bytes!` is a raw PTY transcript captured from a genuine
//! `codex` 0.142 session — real CSI/SGR escapes, cursor-positioned status
//! bar, the U+203A chooser arrow, the U+2022 spinner bullet and the U+00B7
//! footer separator. Codex renders INLINE (no alt-screen) and animates its
//! "Working" spinner with heavy cursor churn, so the wire shape looks nothing
//! like the hand-typed strings in `agents.rs`; this suite is what keeps the
//! `detect::codex_state*` family honest against it.
//!
//! Capture recipe (a PTY harness that drives `codex`, sends a prompt, and
//! dumps the raw bytes per phase) is described in the PR for issue #225.
//! Acceptance: the state pill must match reality for idle, working, the
//! command/edit approval modals, the directory-trust gate, and the
//! finished-turn idle screen that must evict a now-stale `esc to interrupt`.

use lazybox_agents::AgentState;
use lazybox_agents::detect::{codex_ready_for_prompt, codex_state, codex_state_chunked};

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
