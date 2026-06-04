//! Real-byte detector fixtures.
//!
//! The synthetic tests in `agents.rs` feed the detector clean,
//! hand-typed strings — none carry ANSI escapes or the temporal
//! reordering that tmux produces, so the detector can pass every one of
//! them and still misfire live. This suite closes that gap: each
//! `include_bytes!` is a raw PTY transcript (genuine CSI/SGR escapes,
//! multi-byte glyphs, fragmented chooser lines) of the shape
//! `LAZYBOX_CAPTURE_PTY` dumps from a real session. See
//! `tests/fixtures/generate.py` for how they're built and how to add
//! captures from new sessions.
//!
//! Acceptance (#142): the state pill must match reality across a
//! real-transcript corpus for idle, working, permission prompt, trust
//! prompt, conversational question, and finished.

use lazybox_agents::AgentState;
use lazybox_agents::detect::{claude_ready_for_prompt, claude_state};

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
    // The genuine fresh-spawn idle screen (#153). Carries the version
    // banner `v2.1.159` (decimals that look like option labels) + the
    // composer's `❯` prompt glyph + the spaceless `?forshortcuts`
    // status-bar footer. Must read Idle AND ready — the combination that
    // used to misfire InputNeeded and never signal ready, so the
    // spawn-time injector rode its 10s hard deadline on every start.
    ByteFixture {
        name: "claude_real_idle_ready",
        bytes: include_bytes!("fixtures/claude_real_idle_ready.bin"),
        expected: AgentState::Idle,
        ready: true,
    },
    // A genuinely active session (#153): scrolling bash output, the live
    // `(10s · ↓ 137 tokens)` counter, the spaceless `esctointerrupt`
    // hint — plus the same version banner + composer `❯` that masked the
    // Working state behind a false InputNeeded. Must read Working, never
    // ready.
    ByteFixture {
        name: "claude_real_working_statusbar",
        bytes: include_bytes!("fixtures/claude_real_working_statusbar.bin"),
        expected: AgentState::Working,
        ready: false,
    },
    // #179: lazybox spawns agents with `--dangerously-skip-permissions`,
    // so the idle composer footer is the bypass-mode mode line —
    // `bypass permissions on (shift+tab to cycle) · ← for agents`, with
    // no `? for shortcuts`. Before the fix this footer matched no idle
    // anchor, so a finished agent's stale `esc to interrupt` was never
    // evicted and the working glyph stuck ON; readiness also never fired,
    // so the spawn-time injector rode its hard deadline. Must read Idle
    // AND ready.
    ByteFixture {
        name: "claude_real_bypass_idle",
        bytes: include_bytes!("fixtures/claude_real_bypass_idle.bin"),
        expected: AgentState::Idle,
        ready: true,
    },
    // #156 false positive: a parked `❯` prompt in the composer above a
    // prose numbered list fired InputNeeded on an idle screen. The
    // composer footer is the most recent bottom-of-screen marker, so no
    // live chooser can be up — must read Idle.
    ByteFixture {
        name: "false_positive_prose_list",
        bytes: include_bytes!("fixtures/false_positive_prose_list.bin"),
        expected: AgentState::Idle,
        ready: true,
    },
    // #156 false-negative guard: a live permission chooser rendered
    // below a now-stale composer footer still in the detect window. The
    // chooser markers are more recent than the footer, so it must still
    // fire InputNeeded — a naive "composer visible → not asking" veto
    // would miss it.
    ByteFixture {
        name: "permission_below_stale_composer",
        bytes: include_bytes!("fixtures/permission_below_stale_composer.bin"),
        expected: AgentState::InputNeeded,
        ready: false,
    },
    // #2 false negative: a real bash-approval dialog whose footer is the
    // full `Esc to cancel · Tab to amend · ctrl+e to explain` form. The
    // `Tab to amend` token made `idle_box_pos` treat the live chooser
    // above it as stale scrollback, so the `?` never showed. The strong
    // consent-phrase tier (gated on the reliable `? for shortcuts` /
    // bypass anchor, not `Tab to amend`) now catches it.
    ByteFixture {
        name: "permission_bash_amend_footer",
        bytes: include_bytes!("fixtures/permission_bash_amend_footer.bin"),
        expected: AgentState::InputNeeded,
        ready: false,
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
