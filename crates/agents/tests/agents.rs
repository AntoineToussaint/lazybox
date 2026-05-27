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
fn claude_qmark_heuristic_skips_long_prose_lines() {
    // A long-form question (claude narrating its plan and ending
    // with `?`) used to flicker the Asking state — the 8s Asking→
    // Active hysteresis amplified each blip into 8s of stuck
    // "needs input." Real prompts are short; >80 chars is prose.
    let agent = Claude;
    let buf = b"Reading through the file now to figure out which of the three \
                possible refactors would land us the cleanest API surface, so \
                we extract the inner type or keep it inline?";
    assert_eq!(agent.detect_state(buf), Some(AgentState::Active));
}

#[test]
fn claude_qmark_heuristic_skips_two_sentence_prose() {
    // Two-sentence line ending in `?` (with the second sentence
    // starting with a capital after `.`) is prose, not a prompt.
    let agent = Claude;
    let buf = b"I've finished the implementation. Want me to run the tests now?";
    assert_eq!(agent.detect_state(buf), Some(AgentState::Active));
}

#[test]
fn claude_qmark_heuristic_still_fires_on_short_prompts() {
    // Belt-and-braces: the tightened heuristic must still catch
    // the canonical short prompt that motivated it.
    let agent = Claude;
    let buf = b"Some context above\nProceed with the rewrite?";
    assert_eq!(agent.detect_state(buf), Some(AgentState::Asking));
}
