//! Tests for `Agent` impls and `Registry`. These lock in the argv each
//! built-in uses so a rename or flag change is caught immediately.
//! Generic CLI gets its own block — it's the extensibility surface
//! users will drive from YAML.

use pilot_agents::agent::builtins::{Claude, Codex, Cursor, GenericCli};
use pilot_agents::{Agent, AgentState, Registry, SessionWrapper, SpawnCtx};
use std::collections::HashMap;
use std::path::PathBuf;

fn sample_ctx() -> SpawnCtx {
    SpawnCtx {
        session_key: "github:o/r#1".into(),
        worktree: PathBuf::from("/tmp/wt"),
        repo: Some("o/r".into()),
        pr_number: Some("1".into()),
        env: HashMap::new(),
        autonomous: false,
    }
}

#[test]
fn registry_has_expected_builtins() {
    let r = Registry::default_builtins();
    assert!(r.get("claude").is_some(), "claude agent registered");
    assert!(r.get("codex").is_some(), "codex agent registered");
    assert!(r.get("cursor-agent").is_some(), "cursor-agent registered");
    assert!(r.get("does-not-exist").is_none(), "unknown returns None");
}

#[test]
fn claude_spawn_and_resume_argv() {
    let agent = Claude;
    let ctx = sample_ctx();
    assert_eq!(agent.spawn(&ctx), vec!["claude".to_string()]);
    assert_eq!(
        agent.resume(&ctx),
        vec!["claude".to_string(), "--continue".to_string()],
        "resume must use --continue so the previous conversation is picked up"
    );
}

#[test]
fn claude_autonomous_spawn_skips_permissions() {
    // Unattended pilot spawns (auto-fix / auto-spawn-on-mention) run
    // with no human at the terminal: the agent must clear the first-
    // run workspace-trust dialog on a fresh worktree (otherwise the
    // injected prompt lands in the trust chooser and is lost) and
    // push edits without anyone to approve them. Both gates are
    // bypassed by `--dangerously-skip-permissions`.
    let agent = Claude;
    let ctx = SpawnCtx {
        autonomous: true,
        ..sample_ctx()
    };
    assert_eq!(
        agent.spawn(&ctx),
        vec![
            "claude".to_string(),
            "--dangerously-skip-permissions".to_string()
        ],
    );
    assert_eq!(
        agent.resume(&ctx),
        vec![
            "claude".to_string(),
            "--continue".to_string(),
            "--dangerously-skip-permissions".to_string()
        ],
        "resume must keep both --continue and the permission bypass"
    );
}

#[test]
fn codex_argv() {
    let agent = Codex;
    let ctx = sample_ctx();
    assert_eq!(agent.spawn(&ctx), vec!["codex".to_string()]);
    // Default trait impl: resume == spawn when the agent doesn't
    // override. Codex has no --continue flag today.
    assert_eq!(agent.resume(&ctx), agent.spawn(&ctx));
}

#[test]
fn cursor_argv() {
    let agent = Cursor;
    let ctx = sample_ctx();
    assert_eq!(agent.spawn(&ctx), vec!["cursor-agent".to_string()]);
}

#[test]
fn claude_inject_prompt_is_just_the_prompt_body() {
    // Claude Code batches rapid byte arrival as a paste. If we
    // included `\r` here it would land inside the paste blob and
    // Claude would interpret it as a soft line break in the input
    // buffer, not a submit — the prompt would sit in the input box
    // waiting on a keystroke (the bug this test guards against).
    // The trailing Enter is delivered separately by `inject_submit`,
    // after a brief delay so the paste batch settles first.
    let agent = Claude;
    assert_eq!(agent.inject_prompt("hi"), b"hi");
    assert_eq!(agent.inject_prompt(""), b"");
    // Internal `\n` is preserved verbatim — it's intentionally a
    // line break inside Claude's input.
    assert_eq!(agent.inject_prompt("multi\nline"), b"multi\nline");
}

#[test]
fn claude_inject_submit_is_carriage_return() {
    // Companion to `claude_inject_prompt_is_just_the_prompt_body`:
    // the actual submit keystroke. The spawn handler writes this
    // ~200ms after the paste so Claude's paste detection has
    // closed its batch — Enter then fires as an independent
    // keystroke and submits the buffered prompt.
    let agent = Claude;
    assert_eq!(agent.inject_submit(), Some(vec![b'\r']));
}

#[test]
fn default_agent_inject_submit_is_none() {
    // For agents where `inject_prompt` already includes the submit
    // keystroke (the default trait impl appends `\n`), the spawn
    // handler skips the second write. Codex/Cursor inherit this
    // default — only Claude needs the paste/submit split.
    let agent = Codex;
    assert_eq!(agent.inject_submit(), None);
    let agent = Cursor;
    assert_eq!(agent.inject_submit(), None);
}

#[test]
fn codex_detects_yn_prompt() {
    // Codex prompts the user with `[y/n]` for tool approvals. The
    // detector flags those as Asking; everything else is Active.
    let agent = Codex;
    assert_eq!(
        agent.detect_state(b"run rm -rf? [y/n]"),
        Some(AgentState::Asking)
    );
    assert_eq!(agent.detect_state(b"hello world"), Some(AgentState::Active));
}

#[test]
fn claude_detects_chooser_footer() {
    // The Claude Code chooser UI is recognisable by its `Esc to
    // cancel · Tab to amend` footer plus a question phrasing. Both
    // need to match for Asking; neither alone is sufficient
    // (chat output could include the phrase).
    let agent = Claude;
    let buf = b"Do you want to proceed?\n> 1. Yes\n  2. No\n\n\
                Esc to cancel \xc2\xb7 Tab to amend \xc2\xb7 ctrl+e to explain";
    assert_eq!(agent.detect_state(buf), Some(AgentState::Asking));
}

#[test]
fn claude_active_when_just_streaming() {
    let agent = Claude;
    assert_eq!(
        agent.detect_state(b"running tests..."),
        Some(AgentState::Active)
    );
}

/// `detect_ready_for_prompt` is true when Claude's input box is
/// drawn AND no permission / chooser gate is up. Distinct from
/// `detect_state` (which would flag both as `Asking`). This is
/// the gate the spawn-time injector uses so a fresh `w` press
/// lands the prompt the moment Claude is ready, no false-positive
/// 60s wait.
#[test]
fn claude_ready_for_prompt_when_input_box_visible_and_no_chooser() {
    let agent = Claude;
    let idle = "│ > \n│ \n\n\
                Esc to cancel · Tab to amend · ctrl+e to explain\n";
    assert!(agent.detect_ready_for_prompt(idle.as_bytes()));
}

#[test]
fn claude_not_ready_during_permission_chooser() {
    // Same footer marker AS WELL AS the chooser arrow + numbered
    // options. The chooser must veto the "ready" signal.
    let agent = Claude;
    let permission = "Trust this folder? \n\
                      ❯ 1. Yes, proceed\n\
                        2. No, exit\n\
                      Esc to cancel · Tab to amend\n";
    assert!(!agent.detect_ready_for_prompt(permission.as_bytes()));
}

#[test]
fn claude_not_ready_when_input_box_absent() {
    // Boot banner with no input markers — claude isn't ready yet.
    let agent = Claude;
    let booting = b"Welcome to Claude Code\nLoading...\n";
    assert!(!agent.detect_ready_for_prompt(booting));
}

/// Trust-folder prompt without the chooser arrow (older claude
/// builds, alt phrasings) still blocks the "ready" signal —
/// otherwise the original `y eats my prompt` race comes back.
#[test]
fn claude_not_ready_during_trust_folder_prompt() {
    let agent = Claude;
    let trust = "Do you trust the files in this folder?\n\
                 \n\
                 Esc to cancel · Tab to amend\n";
    assert!(!agent.detect_ready_for_prompt(trust.as_bytes()));
}

#[test]
fn claude_detects_choice_arrow() {
    // Claude's permission / tool-approval / multi-choice UI renders
    // the selected row as `❯ 1.` (or `❯ 1)`). That glyph+digit
    // sequence is unambiguous — no question pairing needed. This
    // is the path that kept failing for the user: the chat output
    // doesn't have an "Esc to cancel" footer in the buffer once
    // status ticks scroll it out, but the arrow line is always
    // re-rendered as long as the chooser is up.
    let agent = Claude;
    let buf = "Allow Bash this command?\n\
               ❯ 1. Yes\n\
                 2. Yes, and don't ask again\n\
                 3. No, and tell Claude what to do differently\n";
    assert_eq!(agent.detect_state(buf.as_bytes()), Some(AgentState::Asking),);
}

#[test]
fn claude_detects_choice_arrow_with_tmux_repaint_fragmentation() {
    // Regression: tmux paints the screen by absolute cursor position
    // so `❯ 1. Yes` arrives in the buffer with arbitrary content
    // between the arrow and the numbered option. The old `❯ 1.`
    // literal-substring matcher missed this; the new paired matcher
    // (arrow + 1.Yes anywhere in buffer) catches it.
    let agent = Claude;
    // Simulate a real tmux-fragmented buffer: arrow appears, then a
    // status-bar ticker chunk, then the numbered options.
    let buf = "❯ \n\
               Tokens: 1.2k  elapsed: 4s\n\
               1. Yes\n\
               2. No\n";
    assert_eq!(
        agent.detect_state(buf.as_bytes()),
        Some(AgentState::Asking),
        "arrow + 1. Yes anywhere in buffer must fire Asking, even when not adjacent",
    );
}

#[test]
fn claude_detects_choice_arrow_ascii_fallback() {
    // Some terminals / tmux configs render the arrow as `> ` when
    // the UTF-8 glyph isn't available. Cover both shapes.
    let agent = Claude;
    let buf = b"Do you want to make this edit?\n> 1. Yes\n  2. No\n";
    assert_eq!(agent.detect_state(buf), Some(AgentState::Asking));
}

#[test]
fn generic_cli_spawn_and_resume() {
    let agent = GenericCli {
        id: "custom",
        display_name: "Custom",
        spawn_cmd: vec!["custom-bin".into(), "--start".into()],
        resume_cmd: Some(vec!["custom-bin".into(), "--resume".into()]),
        asking_patterns: vec![],
    };
    let ctx = sample_ctx();
    assert_eq!(agent.spawn(&ctx), vec!["custom-bin", "--start"]);
    assert_eq!(agent.resume(&ctx), vec!["custom-bin", "--resume"]);
}

#[test]
fn generic_cli_resume_defaults_to_spawn() {
    let agent = GenericCli {
        id: "custom",
        display_name: "Custom",
        spawn_cmd: vec!["custom".into()],
        resume_cmd: None,
        asking_patterns: vec![],
    };
    let ctx = sample_ctx();
    assert_eq!(agent.resume(&ctx), agent.spawn(&ctx));
}

#[test]
fn generic_cli_asking_pattern_matching() {
    let agent = GenericCli {
        id: "custom",
        display_name: "Custom",
        spawn_cmd: vec!["custom".into()],
        resume_cmd: None,
        asking_patterns: vec!["Press Enter to continue".into(), "[y/N]".into()],
    };
    assert_eq!(
        agent.detect_state(b"Some output... Press Enter to continue\n"),
        Some(AgentState::Asking)
    );
    assert_eq!(
        agent.detect_state(b"Install all? [y/N]"),
        Some(AgentState::Asking)
    );
    assert_eq!(agent.detect_state(b"just normal output"), None);
}

#[test]
fn generic_cli_empty_patterns_returns_none() {
    // Empty patterns = "no opinion"; must return None (not Asking!)
    let agent = GenericCli {
        id: "x",
        display_name: "x",
        spawn_cmd: vec!["x".into()],
        resume_cmd: None,
        asking_patterns: vec![],
    };
    assert_eq!(agent.detect_state(b"anything"), None);
}

// ── SessionWrapper tests ───────────────────────────────────────────────

#[test]
fn tmux_wrap_shape() {
    use pilot_agents::TmuxWrapper;
    let w = TmuxWrapper::new();
    let argv = w.wrap(
        "github:o/r#1",
        &["claude".to_string(), "--continue".to_string()],
        std::path::Path::new("/tmp/wt"),
    );
    assert_eq!(argv[0], "tmux");
    assert_eq!(argv[1], "new-session");
    assert_eq!(argv[2], "-A", "-A makes tmux attach if session exists");
    assert_eq!(argv[3], "-s");
    assert_eq!(
        argv[4], "github_o_r#1",
        "session id must be sanitized — colons and slashes become underscores"
    );
    assert_eq!(
        argv[5], "claude --continue",
        "inner command is joined into one string for tmux"
    );
}

#[test]
fn tmux_sanitize_id_replaces_reserved_chars() {
    use pilot_agents::TmuxWrapper;
    let w = TmuxWrapper::new();
    assert_eq!(w.sanitize_id("a:b/c"), "a_b_c");
    assert_eq!(w.sanitize_id("simple"), "simple");
    assert_eq!(w.sanitize_id("deep/nested:key#1"), "deep_nested_key#1");
}

#[test]
fn raw_wrapper_returns_inner_unchanged() {
    use pilot_agents::session_wrapper::RawWrapper;
    let w = RawWrapper;
    let inner = vec!["bash".to_string(), "-c".to_string(), "echo x".to_string()];
    assert_eq!(
        w.wrap("any-key", &inner, std::path::Path::new("/")),
        inner,
        "RawWrapper must not modify the argv"
    );
    assert!(w.list_sessions().is_empty(), "raw has no session registry");
    assert!(w.kill("anything").is_ok(), "raw kill is always Ok");
}

// ── Shared detect helpers ─────────────────────────────────────────────
//
// Cover the primitives every agent's `detect_state` now sits on
// top of. The per-agent tests above (`codex_detects_yn_prompt`,
// `claude_detects_chooser_footer`, etc.) cover composition; these
// pin the building blocks.

#[test]
fn contains_any_matches_first_pattern() {
    use pilot_agents::agent::detect;
    assert!(detect::contains_any("approve? y/n", &["approve?", "(y/n)"]));
    assert!(detect::contains_any(
        "running tests... [y/n]",
        detect::YN_PROMPT_PATTERNS,
    ));
}

#[test]
fn contains_any_returns_false_when_no_match() {
    use pilot_agents::agent::detect;
    assert!(!detect::contains_any(
        "regular output, no prompts here",
        detect::YN_PROMPT_PATTERNS,
    ));
}

#[test]
fn contains_any_empty_pattern_set_is_false() {
    use pilot_agents::agent::detect;
    // Edge case: an empty pattern set never matches, even on
    // matching-looking text. The GenericCli `detect_state` guards
    // its empty path explicitly, but the primitive should also be
    // safe.
    assert!(!detect::contains_any("[y/n]", &[]));
}

#[test]
fn contains_paired_requires_both_a_choice_and_a_question() {
    use pilot_agents::agent::detect;
    // Claude's pairing contract: a numbered choice ALONE doesn't
    // trigger (could be chat output listing options), nor does a
    // question phrase alone. Both must appear together.
    let buf = "1. Yes\n  2. No\nDo you want to proceed?";
    assert!(detect::contains_paired(
        buf,
        &["1. Yes"],
        &["Do you want to"],
    ));
}

#[test]
fn contains_paired_with_only_choice_is_false() {
    use pilot_agents::agent::detect;
    let buf = "Listing options: 1. Yes 2. No";
    assert!(!detect::contains_paired(
        buf,
        &["1. Yes"],
        &["Do you want to", "Approve"],
    ));
}

#[test]
fn contains_paired_with_only_question_is_false() {
    use pilot_agents::agent::detect;
    // Prevents a false-positive on chat output that mentions the
    // question phrase without an actual prompt UI.
    let buf = "The assistant said: 'Do you want to know more?'";
    assert!(!detect::contains_paired(
        buf,
        &["1. Yes", "(y/n)"],
        &["Do you want to"],
    ));
}

#[test]
fn yn_pattern_constant_matches_every_published_variant() {
    use pilot_agents::agent::detect;
    // The four canonical forms agents emit today. Catches an
    // accidental drop from the constant.
    for marker in ["[y/n]", "(y/n)", "[Y/n]", "[y/N]"] {
        assert!(
            detect::contains_any(&format!("Confirm? {marker}"), detect::YN_PROMPT_PATTERNS),
            "YN_PROMPT_PATTERNS must include {marker}",
        );
    }
}

#[test]
fn conversational_ask_phrase_constant_each_entry_fires() {
    use pilot_agents::agent::detect::conversational;
    // Every documented phrase, used in a minimal sentence ending in
    // a statement (no trailing `?`), must trigger the conversational
    // detector via the phrase list alone. Guards against an
    // accidental drop from CONVERSATIONAL_ASK_PHRASES and proves the
    // non-`?` recall path (e.g. "Let me know if …").
    for phrase in conversational::CONVERSATIONAL_ASK_PHRASES {
        let line = format!("Sure, {phrase} that for you now.");
        assert!(
            conversational::is_conversational_ask(&line),
            "CONVERSATIONAL_ASK_PHRASES entry {phrase:?} must fire is_conversational_ask",
        );
    }
}

#[test]
fn conversational_ask_skips_input_box_chrome_to_reach_question() {
    use pilot_agents::agent::detect::conversational;
    // The ask sits above the rendered input box; the detector must
    // skip the borders + footer to find it.
    let buf = concat!(
        "● Should I also update the docs?\n",
        "╭───────────────╮\n",
        "│ >             │\n",
        "╰───────────────╯\n",
        "  ? for shortcuts",
    );
    assert!(conversational::is_conversational_ask(buf));
}

#[test]
fn conversational_ask_ignores_question_above_a_closing_statement() {
    use pilot_agents::agent::detect::conversational;
    // Only the LAST conversational line is load-bearing — a question
    // earlier in the turn must not fire once the turn closes on a
    // statement.
    let buf = "Why is it slow? The cache is cold.\nFixed it — all green.";
    assert!(!conversational::is_conversational_ask(buf));
}

#[test]
fn claude_detects_standalone_proceed_prompt_lowercase() {
    // Regression: the user reported a real `Do you want to proceed?`
    // bash-permission prompt going undetected. The paired matcher
    // had `Proceed?` (capital P) but claude renders the actual
    // phrase lowercase. Standalone path matches on lowercase.
    let agent = Claude;
    let buf = b"some long bash output\nDo you want to proceed?\n";
    assert_eq!(agent.detect_state(buf), Some(AgentState::Asking));
}

#[test]
fn claude_detects_other_lowercase_standalone_prompts() {
    let agent = Claude;
    for prompt in [
        "Do you want to continue?",
        "do you want to allow this?",
        "do you want to apply these changes?",
        "do you want to retry?",
    ] {
        assert_eq!(
            agent.detect_state(prompt.as_bytes()),
            Some(AgentState::Asking),
            "standalone prompt should fire: {prompt:?}",
        );
    }
}

#[test]
fn claude_standalone_does_not_fire_on_chat_context() {
    // The standalone phrases are tight enough to not fire on
    // arbitrary chat output. Belt-and-braces sanity: prose that
    // mentions "proceed" but isn't the exact prompt stays Active.
    let agent = Claude;
    assert_eq!(
        agent.detect_state(b"I'll proceed with the change once you say so."),
        Some(AgentState::Active),
    );
    assert_eq!(
        agent.detect_state(b"Reading the manual to figure out how to continue."),
        Some(AgentState::Active),
    );
}

#[test]
fn claude_detects_question_via_last_line_ends_with_qmark() {
    // Last-resort heuristic: if the most recent non-footer line ends
    // with `?`, claude is most likely asking. Catches prompts that
    // don't match any of the specific patterns (custom approval UIs,
    // future claude prompt shapes, etc.).
    let agent = Claude;
    let buf = b"I checked the file.\nShall I delete the cache directory?";
    assert_eq!(agent.detect_state(buf), Some(AgentState::Asking));
}

#[test]
fn claude_question_heuristic_skips_quoted_continuation_lines() {
    // Lines prefixed with `>` are claude's quote-block UI for echoing
    // a prior prompt or a code-block continuation. They commonly end
    // in `?` (because the prior prompt was a question) but the
    // ACTUAL current state isn't Asking. The heuristic must skip
    // them and look at the next non-quote line.
    let agent = Claude;
    let buf = b"> Why does this happen?\nReading the file to find out.";
    assert_eq!(agent.detect_state(buf), Some(AgentState::Active));
}

#[test]
fn claude_question_heuristic_stays_active_on_plain_streaming() {
    // No question mark anywhere → Active. Belt-and-braces.
    let agent = Claude;
    assert_eq!(
        agent.detect_state(b"Running tests...\nCompiling pilot-tui v0.1.0\nFinished in 4.2s"),
        Some(AgentState::Active),
    );
}

#[test]
fn claude_detect_state_handles_multibyte_at_tail_boundary() {
    // Regression: the `?`-heuristic's `s[tail_start..]` was raw-byte
    // slicing, which panicked when `tail_start` landed inside a
    // multi-byte UTF-8 codepoint (`─` is 3 bytes; claude renders
    // it heavily for box-drawing borders). The panic killed the
    // per-terminal pump task and the user's host terminal got
    // stuck in raw mode with the alt screen still up.
    //
    // Construct a buffer where the natural 1024-byte tail boundary
    // hits the middle of a multi-byte character.
    let agent = Claude;
    let mut buf = Vec::new();
    // 1000 bytes of padding + 30 box-drawing dashes (90 bytes of
    // UTF-8) → ~1090 bytes total; the tail at len-1024 must land
    // inside one of the `─` sequences.
    buf.extend(std::iter::repeat_n(b'.', 1000));
    for _ in 0..30 {
        buf.extend_from_slice("─".as_bytes());
    }
    buf.extend_from_slice(b"\nclean line.");
    // Should not panic. Result is whatever — we only care about
    // not crashing inside detect_state.
    let _ = agent.detect_state(&buf);
}

#[test]
fn claude_conversational_ask_fires_on_long_clarifying_question() {
    // Issue #58 reverses the earlier precision-first tradeoff: a
    // long-form clarifying question ("…so we extract the inner type
    // or keep it inline?") is a genuine ask the user must answer,
    // even though it's >80 chars. Recall over precision — a stuck
    // "needs input" pill is cheaper than a session sitting idle
    // forever while it actually waits on the user.
    let agent = Claude;
    let buf = b"Reading through the file now to figure out which of the three \
                possible refactors would land us the cleanest API surface, so \
                we extract the inner type or keep it inline?";
    assert_eq!(agent.detect_state(buf), Some(AgentState::Asking));
}

#[test]
fn claude_conversational_ask_fires_on_want_me_to_after_a_statement() {
    // The poster-child #58 case: Claude finishes a turn with a
    // statement followed by a confirmation ask. The old sentence-
    // break heuristic suppressed this as "prose"; #58 flags it —
    // the model is plainly waiting on the user.
    let agent = Claude;
    let buf = b"I've finished the implementation. Want me to run the tests now?";
    assert_eq!(agent.detect_state(buf), Some(AgentState::Asking));
}

#[test]
fn claude_conversational_ask_fires_on_short_prompts() {
    // Belt-and-braces: the canonical short prompt still fires.
    let agent = Claude;
    let buf = b"Some context above\nProceed with the rewrite?";
    assert_eq!(agent.detect_state(buf), Some(AgentState::Asking));
}

// ── Prompt-shape fixture suite ─────────────────────────────────────────
//
// A canonical version of every Claude Code prompt shape pilot's
// detector is expected to recognise. Each fixture pairs a stable
// name with a representative buffer and the expected `AgentState`.
// The single test below iterates so adding a new prompt shape is a
// one-fixture entry — no per-shape `#[test]` boilerplate.
//
// Maintainer flow:
//   1. Capture the new shape (see `/tmp/pilot.log` when claude
//      renders the prompt in pilot, or transcribe the visible
//      terminal).
//   2. Add a `PromptFixture` row.
//   3. If the detector misses it, extend `Claude::detect_state`
//      until this test passes. Keep the STANDALONE / paired /
//      arrow branches in sync with the shapes named here so the
//      coverage table stays auditable from one place.

struct PromptFixture {
    /// Stable name surfaced in failure messages. Should describe
    /// the prompt shape, not its content (e.g.
    /// `write_tool_permission_arrow_on_option_2`).
    name: &'static str,
    /// Raw buffer (post-ANSI, pre-detector) as it would appear in
    /// the per-terminal ring buffer. Newlines and indentation
    /// preserved; multi-line content uses `\n` literals so the
    /// fixture is one source line per visual row.
    buffer: &'static str,
    /// Expected `AgentState`. `Asking` for real prompts, `Active`
    /// for false-positive controls (chat output that LOOKS like a
    /// prompt but isn't).
    expected: AgentState,
}

const PROMPT_FIXTURES: &[PromptFixture] = &[
    // ── Asking shapes — the real prompts ─────────────────────────
    PromptFixture {
        // Regression for issue #26: cursor on option 2 (not 1),
        // and the chooser footer is just "Esc to cancel" (no
        // "Tab to amend"). Earlier detector missed all three
        // independent matchers — `> 2.` ≠ `> 1.` for the arrow,
        // missing "Tab to amend" for the footer pair, and no
        // STANDALONE entry for "do you want to create". Pinned
        // here so a future tightening can't silently re-introduce
        // the gap.
        name: "write_tool_permission_arrow_on_option_2",
        buffer: concat!(
            "Do you want to create MEMORY.md?\n",
            "  1. Yes\n",
            "> 2. Yes, and allow Claude to edit its own settings for this session\n",
            "  3. No\n",
            "Esc to cancel",
        ),
        expected: AgentState::Asking,
    },
    PromptFixture {
        name: "edit_tool_permission_utf8_arrow",
        buffer: concat!(
            "Do you want to make this edit to agent.rs?\n",
            "❯ 1. Yes\n",
            "  2. Yes, allow all edits during this session\n",
            "  3. No, and tell Claude what to do differently\n",
            "Esc to cancel",
        ),
        expected: AgentState::Asking,
    },
    PromptFixture {
        // Arrow on option 3 — exercises the generalized
        // `has_ascii_chooser_arrow` helper (any `> N.` shape, not
        // just `> 1.`).
        name: "bash_permission_cursor_on_option_3",
        buffer: concat!(
            "Allow Bash this command?\n",
            "  1. Yes\n",
            "  2. Yes, and don't ask again\n",
            "> 3. No, and tell Claude what to do differently\n",
            "Esc to cancel",
        ),
        expected: AgentState::Asking,
    },
    PromptFixture {
        name: "bash_permission_utf8_arrow",
        buffer: concat!(
            "Allow Bash this command?\n",
            "❯ 1. Yes\n",
            "  2. Yes, and don't ask again\n",
            "  3. No, and tell Claude what to do differently\n",
            "Esc to cancel",
        ),
        expected: AgentState::Asking,
    },
    PromptFixture {
        name: "plan_mode_exit",
        buffer: concat!(
            "Do you want to proceed?\n",
            "❯ 1. Yes\n",
            "  2. No\n",
            "Esc to cancel",
        ),
        expected: AgentState::Asking,
    },
    PromptFixture {
        name: "trust_folder_chooser",
        buffer: concat!(
            "Do you trust the files in this folder?\n",
            "❯ 1. Yes, proceed\n",
            "  2. No, exit\n",
            "Esc to cancel",
        ),
        expected: AgentState::Asking,
    },
    PromptFixture {
        // AskUserQuestion-style multi-option chooser. Same shape
        // as a permission prompt but more options, free-form
        // question text. Coverage hangs on the arrow+options
        // branch — no question-phrase pairing required.
        name: "ask_user_question_three_options",
        buffer: concat!(
            "Which library should we use?\n",
            "❯ 1. tokio\n",
            "  2. async-std\n",
            "  3. smol\n",
            "Esc to cancel",
        ),
        expected: AgentState::Asking,
    },
    PromptFixture {
        // Permission dialog where the footer (`Esc to cancel`)
        // and the chooser shape are the only signals — no `❯`
        // glyph, no `> N.` arrow, no recognised question phrase.
        // The new `Esc to cancel + numbered options` branch is
        // the only matcher that fires here.
        name: "permission_dialog_no_arrow_no_question_phrase",
        buffer: concat!(
            "  1. Approve\n",
            "  2. Skip\n",
            "  3. Cancel\n",
            "Esc to cancel",
        ),
        expected: AgentState::Asking,
    },
    PromptFixture {
        // Standalone "do you want to <verb>" phrase variants —
        // each one is a permission/consent prompt class.
        name: "write_permission_standalone_no_chooser",
        buffer: "Do you want to create README.md?",
        expected: AgentState::Asking,
    },
    PromptFixture {
        name: "overwrite_permission_standalone",
        buffer: "Do you want to overwrite the existing file?",
        expected: AgentState::Asking,
    },
    PromptFixture {
        name: "delete_permission_standalone",
        buffer: "Do you want to delete src/old_module.rs?",
        expected: AgentState::Asking,
    },
    PromptFixture {
        name: "settings_edit_consent",
        buffer: concat!(
            "Claude wants to edit its own settings. Allow?\n",
            "❯ 1. Yes\n",
            "  2. No\n",
            "Esc to cancel",
        ),
        expected: AgentState::Asking,
    },
    // ── Active controls — must NOT fire as Asking ────────────────
    PromptFixture {
        // Plain build output. No prompt markers, no `?`. Belt-
        // and-braces baseline.
        name: "active_streaming_build_output",
        buffer: concat!(
            "Compiling pilot-tui v0.1.0\n",
            "Finished release [optimized] target(s) in 4.32s",
        ),
        expected: AgentState::Active,
    },
    PromptFixture {
        // Chat output that mentions "do you want to" but isn't
        // an actual prompt. The STANDALONE entries are tight
        // enough not to fire on this prose.
        name: "active_chat_mentions_proceed",
        buffer: "I'll proceed with the change once you say so.",
        expected: AgentState::Active,
    },
    PromptFixture {
        // A `?` appears mid-paragraph but the turn ends on a
        // statement — only the LAST conversational line is load-
        // bearing, so the earlier question must NOT fire Asking.
        name: "active_question_mid_paragraph_not_last_line",
        buffer: concat!(
            "Why does this matter? Because the cache is cold on first run.\n",
            "Done — all tests pass.",
        ),
        expected: AgentState::Active,
    },
    // ── Conversational asks (issue #58) ──────────────────────────
    // Claude's OWN freeform asks ending a turn — no menu, no
    // footer, just the model parked on a question. Separate code
    // path from the structural matchers above; one fixture per
    // documented phrase shape in `CONVERSATIONAL_ASK_PHRASES`
    // plus the ends-with-`?` rule. Tuned for recall.
    PromptFixture {
        // The canonical case from the issue, rendered with the
        // input box + footer below it. The conversational detector
        // must skip the box chrome to reach the ask line above.
        name: "conversational_want_me_to_with_input_box",
        buffer: concat!(
            "● Want me to proceed?\n",
            "\n",
            "╭─────────────────────────────────────────╮\n",
            "│ >                                         │\n",
            "╰─────────────────────────────────────────╯\n",
            "  ? for shortcuts",
        ),
        expected: AgentState::Asking,
    },
    PromptFixture {
        // Hardening: the footer below the input box is a standalone
        // mode indicator (`⏵⏵ accept edits on`) with no "for
        // shortcuts" text. The bottom-up scan must still recognise it
        // as chrome and reach the ask above — otherwise the footer
        // masquerades as the last content line and the ask is missed.
        name: "conversational_ask_above_mode_indicator_footer",
        buffer: concat!(
            "● Should I run the test suite now?\n",
            "╭─────────────────────────────────────────╮\n",
            "│ >                                         │\n",
            "╰─────────────────────────────────────────╯\n",
            "  ⏵⏵ accept edits on",
        ),
        expected: AgentState::Asking,
    },
    PromptFixture {
        name: "conversational_should_i",
        buffer: "Should I also update the tests for this change?",
        expected: AgentState::Asking,
    },
    PromptFixture {
        name: "conversational_do_you_want",
        buffer: "Do you want me to update the changelog too?",
        expected: AgentState::Asking,
    },
    PromptFixture {
        name: "conversational_shall_i",
        buffer: "Shall I continue with the next file?",
        expected: AgentState::Asking,
    },
    PromptFixture {
        // Ends with `.`, not `?` — caught by the phrase list
        // ("let me know if"), not the ends-with-`?` rule.
        name: "conversational_let_me_know",
        buffer: "Let me know if you'd like me to continue.",
        expected: AgentState::Asking,
    },
    PromptFixture {
        name: "conversational_would_you_like_me_to",
        buffer: "Would you like me to refactor the helper as well?",
        expected: AgentState::Asking,
    },
    PromptFixture {
        // Bare one-word confirmation asks.
        name: "conversational_proceed_bare",
        buffer: "Proceed?",
        expected: AgentState::Asking,
    },
    PromptFixture {
        name: "conversational_continue_bare",
        buffer: "Continue?",
        expected: AgentState::Asking,
    },
    PromptFixture {
        name: "conversational_ok_to",
        buffer: "Ok to push these changes to the branch?",
        expected: AgentState::Asking,
    },
    PromptFixture {
        name: "conversational_which_one",
        buffer: "Which one should I start with?",
        expected: AgentState::Asking,
    },
    PromptFixture {
        // Long clarifying prose ending in a choice question. Under
        // the old precision-first heuristic this was suppressed for
        // being >80 chars; issue #58 flips the tradeoff toward
        // recall, and this is a genuine ask the user must answer.
        name: "conversational_long_prose_ends_with_question",
        buffer: "I have a few approaches in mind: refactor the loop, extract a helper, or inline the whole thing. Which would you prefer?",
        expected: AgentState::Asking,
    },
];

#[test]
fn claude_detector_covers_every_documented_prompt_shape() {
    let agent = Claude;
    let mut failures: Vec<String> = Vec::new();
    for fixture in PROMPT_FIXTURES {
        let actual = agent.detect_state(fixture.buffer.as_bytes());
        if actual != Some(fixture.expected) {
            failures.push(format!(
                "fixture `{}` expected {:?} but got {:?}\n--- buffer ---\n{}\n--- end ---",
                fixture.name, fixture.expected, actual, fixture.buffer,
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} prompt-shape fixtures failed:\n\n{}",
        failures.len(),
        PROMPT_FIXTURES.len(),
        failures.join("\n\n"),
    );
}

#[test]
fn claude_prompt_fixture_names_are_unique() {
    // Catches a copy-paste mistake — two fixtures with the same
    // name make failure messages ambiguous.
    let mut names: Vec<&str> = PROMPT_FIXTURES.iter().map(|f| f.name).collect();
    names.sort_unstable();
    let original_len = names.len();
    names.dedup();
    assert_eq!(
        names.len(),
        original_len,
        "fixture names must be unique — duplicate(s) found",
    );
}
