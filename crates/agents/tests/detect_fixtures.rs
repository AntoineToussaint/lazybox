//! Real-byte detector fixtures.
//!
//! The synthetic tests in `agents.rs` feed the detector clean,
//! hand-typed strings — none carry ANSI escapes or the temporal
//! reordering that tmux produces, so the detector can pass every one of
//! them and still misfire live. This suite closes that gap: each
//! `include_bytes!` is a raw PTY transcript (genuine CSI/SGR escapes,
//! multi-byte glyphs, fragmented chooser lines) of the shape
//! `PILOT_CAPTURE_PTY` dumps from a real session. See
//! `tests/fixtures/generate.py` for how they're built and how to add
//! captures from new sessions.
//!
//! Acceptance (#142): the state pill must match reality across a
//! real-transcript corpus for idle, working, permission prompt, trust
//! prompt, conversational question, and finished.

use pilot_agents::AgentState;
use pilot_agents::detect::{claude_ready_for_prompt, claude_state};

struct ByteFixture {
    name: &'static str,
    bytes: &'static [u8],
    expected: AgentState,
    /// Whether the spawn-time injector should consider Claude ready to
    /// receive a pasted prompt in this transcript. `true` only for an
    /// idle composer with no gate up.
    ready: bool,
}

const FIXTURES: &[ByteFixture] = &[
    ByteFixture {
        name: "idle_composer",
        bytes: include_bytes!("fixtures/idle_composer.bin"),
        expected: AgentState::Idle,
        ready: true,
    },
    ByteFixture {
        name: "working_status_line",
        bytes: include_bytes!("fixtures/working_status_line.bin"),
        expected: AgentState::Working,
        ready: false,
    },
    ByteFixture {
        name: "permission_prompt_fragmented",
        bytes: include_bytes!("fixtures/permission_prompt_fragmented.bin"),
        expected: AgentState::InputNeeded,
        ready: false,
    },
    ByteFixture {
        name: "trust_folder_prompt",
        bytes: include_bytes!("fixtures/trust_folder_prompt.bin"),
        expected: AgentState::InputNeeded,
        ready: false,
    },
    ByteFixture {
        name: "conversational_question",
        bytes: include_bytes!("fixtures/conversational_question.bin"),
        expected: AgentState::Idle,
        ready: true,
    },
    ByteFixture {
        name: "finished_with_parked_prompt",
        bytes: include_bytes!("fixtures/finished_with_parked_prompt.bin"),
        expected: AgentState::Idle,
        ready: true,
    },
    ByteFixture {
        name: "idle_with_numbered_list",
        bytes: include_bytes!("fixtures/idle_with_numbered_list.bin"),
        expected: AgentState::Idle,
        ready: true,
    },
];

#[test]
fn detector_matches_real_byte_corpus() {
    let mut failures: Vec<String> = Vec::new();
    for f in FIXTURES {
        let actual = claude_state(f.bytes);
        if actual != Some(f.expected) {
            failures.push(format!(
                "fixture `{}` expected {:?} but got {:?}",
                f.name, f.expected, actual,
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} real-byte fixtures failed:\n{}",
        failures.len(),
        FIXTURES.len(),
        failures.join("\n"),
    );
}

#[test]
fn readiness_matches_real_byte_corpus() {
    let mut failures: Vec<String> = Vec::new();
    for f in FIXTURES {
        let actual = claude_ready_for_prompt(f.bytes);
        if actual != f.ready {
            failures.push(format!(
                "fixture `{}` expected ready={} but got {}",
                f.name, f.ready, actual,
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} real-byte readiness checks failed:\n{}",
        failures.len(),
        FIXTURES.len(),
        failures.join("\n"),
    );
}

/// The fixtures must actually carry ANSI escape bytes — otherwise this
/// suite would silently degrade into another synthetic-string test and
/// stop guarding the wire shape it exists for.
#[test]
fn fixtures_contain_real_ansi_escapes() {
    for f in FIXTURES {
        assert!(
            f.bytes.contains(&0x1b),
            "fixture `{}` has no ESC byte — it isn't a real PTY transcript",
            f.name,
        );
    }
}
