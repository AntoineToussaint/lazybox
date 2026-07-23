//! Tests for `Agent` impls and `Registry`. These lock in the argv each
//! built-in uses so a rename or flag change is caught immediately.
//! Generic CLI gets its own block — it's the extensibility surface
//! users will drive from YAML.

use lazybox_agents::agent::builtins::{Claude, Codex, Cursor, GenericCli};
use lazybox_agents::{
    Agent, AgentObservation, AgentState, PromptFraming, PromptIntent, PromptShape, PtyProtocol,
    ReadinessPolicy, Registry, SpawnCtx, trim_leading_blank_lines,
};
use std::collections::HashMap;
use std::path::PathBuf;

fn sample_ctx() -> SpawnCtx {
    SpawnCtx {
        session_key: "github:o/r#1".into(),
        worktree: PathBuf::from("/tmp/wt"),
        repo: Some("o/r".into()),
        pr_number: Some("1".into()),
        env: HashMap::new(),
        skip_permissions: false,
        hook_settings_path: None,
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
fn claude_skip_permissions_appends_unattended_flags() {
    let agent = Claude;
    let ctx = SpawnCtx {
        skip_permissions: true,
        ..sample_ctx()
    };
    // `--strict-mcp-config` makes Claude ignore ambient MCP configs so an
    // unauthenticated MCP server can't wedge the autonomous spawn on its
    // `run /mcp` auth gate (issue #256).
    assert_eq!(
        agent.spawn(&ctx),
        vec![
            "claude".to_string(),
            "--dangerously-skip-permissions".to_string(),
            "--strict-mcp-config".to_string(),
        ],
        "autonomous (skip_permissions) spawn must bypass tool-use prompts and drop ambient MCP"
    );
    assert_eq!(
        agent.resume(&ctx),
        vec![
            "claude".to_string(),
            "--continue".to_string(),
            "--dangerously-skip-permissions".to_string(),
            "--strict-mcp-config".to_string(),
        ],
        "resume must carry the unattended flags too so a resumed autonomous session stays clean"
    );
}

#[test]
fn codex_argv() {
    let agent = Codex;
    let ctx = sample_ctx();
    assert_eq!(agent.spawn(&ctx), vec!["codex".to_string()]);
    // Resume reattaches this worktree's most recent session — Codex
    // filters `resume --last` by cwd, the per-worktree analog of
    // Claude's `--continue`.
    assert_eq!(
        agent.resume(&ctx),
        vec![
            "codex".to_string(),
            "resume".to_string(),
            "--last".to_string(),
        ]
    );
}

#[test]
fn codex_skip_permissions_bypasses_every_startup_gate() {
    let agent = Codex;
    let ctx = SpawnCtx {
        worktree: PathBuf::from("/definitely-missing/worktree.with.dot"),
        skip_permissions: true,
        ..sample_ctx()
    };
    let expected = vec![
        "codex".to_string(),
        "--dangerously-bypass-approvals-and-sandbox".to_string(),
        "-c".to_string(),
        "projects={\"/definitely-missing/worktree.with.dot\"={trust_level=\"trusted\"}}"
            .to_string(),
        "-c".to_string(),
        "check_for_update_on_startup=false".to_string(),
    ];
    assert_eq!(agent.spawn(&ctx), expected);
    // Resume prepends `resume --last` and then carries the identical
    // unattended tail, so a resumed autonomous session stays as clean
    // as a fresh spawn.
    let mut expected_resume = vec![
        "codex".to_string(),
        "resume".to_string(),
        "--last".to_string(),
    ];
    expected_resume.extend(expected[1..].iter().cloned());
    assert_eq!(
        agent.resume(&ctx),
        expected_resume,
        "resumed autonomous Codex sessions must retain the bypass and trust config",
    );
}

#[test]
fn codex_hook_command_args_register_every_lifecycle_event() {
    let agent = Codex;
    let cmd = "lazybox hook-ingest --backend-key-file \"/run/lzb/key-7\"";
    let args = agent.hook_command_args(cmd);

    // Hook trust must be bypassed or Codex silently drops the injected
    // hooks (no persisted per-source trust, no prompt).
    assert_eq!(
        args.first().map(String::as_str),
        Some("--dangerously-bypass-hook-trust"),
    );

    // One `-c hooks.<Event>=…` override per tracked lifecycle event, each
    // registering our correlated hook command.
    let overrides: Vec<&String> = args
        .iter()
        .skip_while(|a| a.as_str() != "-c")
        .filter(|a| a.as_str() != "-c")
        .collect();
    assert_eq!(
        overrides.len(),
        lazybox_agents::hook_settings::HOOKED_EVENTS.len()
    );
    for event in lazybox_agents::hook_settings::HOOKED_EVENTS {
        assert!(
            overrides
                .iter()
                .any(|o| o.starts_with(&format!("hooks.{event}="))),
            "missing hook override for {event}",
        );
    }

    // The command is embedded as a TOML basic string with its inner
    // quotes escaped, so the `-c` value parses.
    assert!(
        overrides.iter().all(|o| o
            .contains("command=\"lazybox hook-ingest --backend-key-file \\\"/run/lzb/key-7\\\"\"")),
        "hook command not TOML-escaped into the override: {overrides:?}",
    );
}

#[test]
fn agents_without_argv_hooks_emit_none() {
    // Claude wires hooks through its settings file, not spawn argv; the
    // rest have no authoritative signal at all.
    assert!(Claude.hook_command_args("x").is_empty());
    assert!(Cursor.hook_command_args("x").is_empty());
}

#[test]
fn cursor_argv() {
    let agent = Cursor;
    let ctx = sample_ctx();
    assert_eq!(agent.spawn(&ctx), vec!["cursor-agent".to_string()]);
}

#[test]
fn codex_yn_only_fires_at_bottom_of_screen() {
    let agent = Codex;
    // A live prompt at the bottom → InputNeeded.
    assert_eq!(
        agent.detect_state(b"Running tests...\nOverwrite config.toml? [y/n] "),
        Some(AgentState::InputNeeded),
    );
    // A `[y/n]` echoed earlier (a diff, a doc, a printed command) with
    // several lines of fresh output below it must NOT fire — it's not a
    // live prompt, just scrollback.
    assert_eq!(
        agent.detect_state(
            b"README: answer [y/n] to confirm\nrunning step 1\nrunning step 2\nrunning step 3\nrunning step 4\nAll tests passed.\n"
        ),
        Some(AgentState::Idle),
    );
    // A live prompt with a few trailing helper/hint lines below the
    // `[y/n]` still fires — the bottom-of-screen zone is several lines
    // deep, not just the last line.
    assert_eq!(
        agent.detect_state(
            b"Apply this patch? [y/n]\n  y = yes, apply\n  n = no, skip\n(press a key)"
        ),
        Some(AgentState::InputNeeded),
    );
}

#[test]
fn guarded_composer_protocol_is_shared_by_claude_and_codex() {
    let agents: [&dyn Agent; 2] = [&Claude, &Codex];
    for agent in agents {
        assert_eq!(agent.pty_protocol(), PtyProtocol::GUARDED_COMPOSER);

        let (initial, submit) = agent
            .encode_prompt("multi\nline", PromptIntent::Submit)
            .into_writes();
        assert_eq!(initial, b"\x1b[200~multi\nline\x1b[201~");
        assert_eq!(submit, Some(vec![b'\r']));

        let (initial, submit) = agent
            .encode_prompt("draft", PromptIntent::Compose)
            .into_writes();
        assert_eq!(initial, b"\x1b[200~draft\x1b[201~");
        assert_eq!(submit, None, "compose-only recall must not press Enter");
    }
}

#[test]
fn prompt_protocol_trims_blank_lines_for_every_framing() {
    let (guarded, submit) = PtyProtocol::GUARDED_COMPOSER
        .encode_prompt("\r\n \tadd an issue:", PromptIntent::Submit)
        .into_writes();
    assert_eq!(guarded, b"\x1b[200~add an issue:\x1b[201~");
    assert_eq!(submit, Some(vec![b'\r']));

    let (line, submit) = PtyProtocol::LINE_ORIENTED
        .encode_prompt("\r\n \tadd an issue:", PromptIntent::Compose)
        .into_writes();
    assert_eq!(line, b"add an issue:");
    assert_eq!(submit, None);
}

#[test]
fn prompt_protocol_preserves_first_line_indentation_and_trailing_whitespace() {
    let prompt = "    indented content\nnext line\n";
    let (guarded, submit) = PtyProtocol::GUARDED_COMPOSER
        .encode_prompt(prompt, PromptIntent::Compose)
        .into_writes();
    assert_eq!(
        guarded,
        b"\x1b[200~    indented content\nnext line\n\x1b[201~"
    );
    assert_eq!(submit, None);
}

#[test]
fn prompt_protocol_trims_unicode_padding_after_a_blank_line() {
    let (line, submit) = PtyProtocol::LINE_ORIENTED
        .encode_prompt(" \x1b\n\u{2003}add an issue:", PromptIntent::Compose)
        .into_writes();
    assert_eq!(line, b"add an issue:");
    assert_eq!(submit, None);
}

#[test]
fn leading_blank_line_normalization_is_bounded_and_idempotent() {
    for (input, expected) in [
        ("", ""),
        ("   ", "   "),
        ("    indented", "    indented"),
        ("\n\n \ttext", "text"),
        (" \r\n\u{2003}text\n", "text\n"),
        ("text\nnext", "text\nnext"),
    ] {
        let normalized = trim_leading_blank_lines(input);
        assert_eq!(normalized, expected, "input {input:?}");
        assert_eq!(trim_leading_blank_lines(normalized), normalized);
    }
}

#[test]
fn bracketed_paste_prompts_carry_echo_probes_for_the_settle_gate() {
    // The paste-settle gate matches these probes against post-paste output
    // to recognize the composer echo, so a repainting TUI (Codex) releases
    // the submit immediately instead of waiting for a quiet window that
    // never comes (issue #425).
    let encoded = PtyProtocol::GUARDED_COMPOSER
        .encode_prompt("Fix the failing CI job on PR #7", PromptIntent::Submit);
    let probes = encoded.echo_probes().to_vec();
    assert!(
        probes.iter().any(|p| p.contains("failingcijobonpr#7")),
        "probes must include a compact fragment of the prompt tail: {probes:?}",
    );
    assert!(
        probes.iter().any(|p| p == "pastedtext"),
        "probes must include the collapsed-paste placeholder chrome: {probes:?}",
    );

    // The echo matcher sees the probe through ANSI styling, line wraps and
    // case changes — and ignores unrelated repaint churn.
    let echo = b"\x1b[2K\x1b[7m>\x1b[0m Fix the\r\n  failing CI job on PR #7";
    assert!(lazybox_agents::detect::paste_echo_observed(echo, &probes));
    assert!(!lazybox_agents::detect::paste_echo_observed(
        b"\x1b[2K\xe2\x80\xa2 spinner tick",
        &probes,
    ));

    // Line framing has no settle gate → no probes.
    let line = PtyProtocol::LINE_ORIENTED.encode_prompt("do it now please", PromptIntent::Submit);
    assert!(line.echo_probes().is_empty());

    // A too-short prompt is not distinctive enough for a text probe, but the
    // placeholder probes still ride along.
    let short = PtyProtocol::GUARDED_COMPOSER.encode_prompt("ok", PromptIntent::Submit);
    assert!(short.echo_probes().iter().all(|p| !p.contains("ok")));
    assert!(short.echo_probes().iter().any(|p| p == "pastedcontent"));

    // Probes describe the text as actually framed: the #426 leading
    // blank-line trim and the ESC sanitizer both apply before probe
    // derivation, so a probe can never reference bytes the composer will
    // never echo.
    let padded = PtyProtocol::GUARDED_COMPOSER.encode_prompt(
        "\n\n  Fix the failing CI job on PR #7",
        PromptIntent::Submit,
    );
    assert_eq!(padded.echo_probes(), probes.as_slice());
    let escaped = PtyProtocol::GUARDED_COMPOSER
        .encode_prompt("Fix the \x1bfailing CI job on PR #7", PromptIntent::Submit);
    assert!(
        escaped.echo_probes().iter().all(|p| !p.contains('\x1b')),
        "probes must never contain raw ESC — the delivered text has none",
    );
    assert!(
        escaped
            .echo_probes()
            .iter()
            .any(|p| p.contains("failingcijobonpr#7")),
    );
}

#[test]
fn chunked_readiness_defaults_to_the_whole_buffer_detector() {
    // Claude doesn't override the chunk-aware hook: any chunk hint must give
    // exactly the whole-buffer verdict.
    let agent = Claude;
    let idle = "❯ \n? for shortcuts";
    for hint in [0, 3, idle.len()] {
        assert_eq!(
            agent.detect_ready_for_prompt_chunked(idle.as_bytes(), hint),
            agent.detect_ready_for_prompt(idle.as_bytes()),
        );
    }
}

#[test]
fn codex_chunked_readiness_fires_on_a_composer_frame_over_stale_working_bytes() {
    // Synthetic mirror of the fixture-driven #425 test: stale working bytes
    // pin the positional read, a composer-rotation frame unpins the
    // chunk-aware one. (The real-byte version lives in codex_fixtures.rs.)
    let agent = Codex;
    let stale = "› Try something\ngpt-5.5 xhigh · /repo\n• Working (9s · esc to interrupt)";
    let mut buf = stale.as_bytes().to_vec();
    assert!(!agent.detect_ready_for_prompt(&buf));
    assert!(!agent.detect_ready_for_prompt_chunked(&buf, 0));

    let frame_start = buf.len();
    buf.extend_from_slice("\n› Summarize recent commits".as_bytes());
    assert!(
        !agent.detect_ready_for_prompt(&buf),
        "whole-buffer read must still be pinned by the stale working line",
    );
    assert!(agent.detect_ready_for_prompt_chunked(&buf, frame_start));

    // A chooser frame (`› 1.`) is a modal, not the composer — never ready.
    let mut modal = stale.as_bytes().to_vec();
    let modal_start = modal.len();
    modal.extend_from_slice("\n› 1. Yes, proceed (y)".as_bytes());
    assert!(!agent.detect_ready_for_prompt_chunked(&modal, modal_start));
}

#[test]
fn prompt_protocol_neutralizes_escape_bytes_once_for_every_agent() {
    // SECURITY: framing is centralized, so both line-oriented agents and
    // guarded composers get the same untrusted-text protection.
    let malicious = "ok\x1b[201~\x1b[200~run something";
    let (guarded, submit) = PtyProtocol::GUARDED_COMPOSER
        .encode_prompt(malicious, PromptIntent::Submit)
        .into_writes();
    let text = String::from_utf8(guarded).unwrap();
    assert_eq!(text, "\x1b[200~ok[201~[200~run something\x1b[201~");
    assert_eq!(text.matches("\x1b[200~").count(), 1);
    assert_eq!(text.matches("\x1b[201~").count(), 1);
    assert_eq!(submit, Some(vec![b'\r']));

    let (line, submit) = PtyProtocol::LINE_ORIENTED
        .encode_prompt(malicious, PromptIntent::Submit)
        .into_writes();
    assert_eq!(line, b"ok[201~[200~run something\n");
    assert_eq!(submit, None);
}

#[test]
fn line_protocol_distinguishes_submit_from_compose_only_recall() {
    let generic = GenericCli {
        id: "custom",
        display_name: "Custom",
        spawn_cmd: vec!["custom-bin".into()],
        resume_cmd: None,
        asking_patterns: vec![],
    };
    for agent in [&Cursor as &dyn Agent, &generic as &dyn Agent] {
        assert_eq!(agent.pty_protocol(), PtyProtocol::LINE_ORIENTED);

        let (submitted, second_write) = agent
            .encode_prompt("do it", PromptIntent::Submit)
            .into_writes();
        assert_eq!(submitted, b"do it\n");
        assert_eq!(second_write, None);

        let (composing, second_write) = agent
            .encode_prompt("edit me", PromptIntent::Compose)
            .into_writes();
        assert_eq!(composing, b"edit me");
        assert_eq!(second_write, None);
    }
}

#[test]
fn custom_protocol_and_observation_keep_agent_semantics_together() {
    let protocol = PtyProtocol::new(PromptFraming::Line, ReadinessPolicy::Required);
    assert_eq!(protocol.framing(), PromptFraming::Line);
    assert_eq!(protocol.readiness(), ReadinessPolicy::Required);
    assert!(protocol.requires_ready());

    let observation = AgentObservation::input_needed(PromptShape::FreeText);
    assert_eq!(observation.state(), AgentState::InputNeeded);
    assert_eq!(observation.prompt_shape(), Some(PromptShape::FreeText));

    let idle = AgentObservation::from_state(AgentState::Idle);
    assert_eq!(idle.state(), AgentState::Idle);
    assert_eq!(idle.prompt_shape(), None);
}

#[test]
fn codex_detects_yn_prompt() {
    // Codex prompts the user with `[y/n]` for bare tool approvals. The
    // detector flags those as InputNeeded; quiet text with no working
    // status line and no prompt is Idle.
    let agent = Codex;
    assert_eq!(
        agent.detect_state(b"run rm -rf? [y/n]"),
        Some(AgentState::InputNeeded)
    );
    assert_eq!(agent.detect_state(b"hello world"), Some(AgentState::Idle));
}

#[test]
fn codex_detects_working_status_line() {
    // Codex paints a live status line while busy:
    // `• Working (3s · esc to interrupt)`. The `esc to interrupt` hint is
    // the per-agent "working" pulser the sidebar spinner keys off — the
    // gap the issue (#225) was filed for, where a busy Codex looked idle.
    let agent = Codex;
    let buf = "• Working (3s · esc to interrupt)";
    assert_eq!(
        agent.detect_state(buf.as_bytes()),
        Some(AgentState::Working),
    );
}

#[test]
fn codex_stale_working_under_composer_footer_is_idle() {
    // A finished turn repaints the composer footer (`gpt-5.5 xhigh ·
    // /repo`) BELOW the now-stale `esc to interrupt`. The more-recent
    // footer wins → Idle, not a forever-busy false positive.
    let agent = Codex;
    let buf = "• Working (12s · esc to interrupt)\n\
               • Done.\n\
               › Try something\n\
               gpt-5.5 xhigh · /repo";
    assert_eq!(agent.detect_state(buf.as_bytes()), Some(AgentState::Idle));
    // And with the composer drawn and no modal up, Codex is ready for a
    // pasted prompt.
    assert!(agent.detect_ready_for_prompt(buf.as_bytes()));
}

#[test]
fn codex_detects_approval_modal_phrases() {
    // Codex's real approval / consent modals carry distinctive chrome
    // beyond `[y/n]`. Each must read InputNeeded.
    let agent = Codex;
    for buf in [
        // command approval
        "Would you like to run the following command?\n\
         › 1. Yes, proceed (y)\n  2. No (esc)\nPress enter to confirm or esc to cancel",
        // file-edit approval (shared "would you like to" stem)
        "Would you like to make the following edits?\n\
         › 1. Yes, proceed (y)\n  2. No (esc)\nPress enter to confirm or esc to cancel",
        // directory-trust gate
        "Do you trust the contents of this directory?\n\
         › 1. Yes, continue\n  2. No, quit\nPress enter to continue",
    ] {
        assert_eq!(
            agent.detect_state(buf.as_bytes()),
            Some(AgentState::InputNeeded),
            "approval modal must fire InputNeeded: {buf:?}",
        );
        assert!(
            !agent.detect_ready_for_prompt(buf.as_bytes()),
            "a live modal must veto the ready signal: {buf:?}",
        );
    }
}

#[test]
fn codex_immediate_prompt_detector_only_accepts_markers_touched_by_latest_chunk() {
    let agent = Codex;
    let modal = "Would you like to run the following command?\n\
                 › 1. Yes, proceed (y)\n  2. No (esc)\n\
                 Press enter to confirm or esc to cancel";
    assert_eq!(
        agent.detect_input_needed_in_current_chunk(modal.as_bytes(), 0),
        Some(PromptShape::Chooser),
    );

    let mut stale_then_output = modal.as_bytes().to_vec();
    let latest_start = stale_then_output.len();
    stale_then_output.extend_from_slice(b"\ntool output continues\n");
    assert_eq!(
        agent.detect_input_needed_in_current_chunk(&stale_then_output, latest_start),
        None,
        "a stale approval modal must not re-fire while fresh tool output streams",
    );

    let split = b"Press enter to con".to_vec();
    let mut fragmented = split.clone();
    fragmented.extend_from_slice(b"firm or esc to cancel");
    assert_eq!(
        agent.detect_input_needed_in_current_chunk(&fragmented, split.len()),
        Some(PromptShape::Chooser),
        "a prompt phrase split across adjacent PTY chunks must still fire",
    );
}

#[test]
fn codex_chooser_arrow_on_numbered_option_fires() {
    // The `› 1.` chooser shape (selection arrow directly on a numbered
    // option) is a live modal even without a recognised phrase — a
    // custom-label chooser whose question scrolled out of the window.
    let agent = Codex;
    let buf = "Pick an option\n› 1. Rewrite\n  2. Patch\n  3. Skip";
    assert_eq!(
        agent.detect_state(buf.as_bytes()),
        Some(AgentState::InputNeeded),
    );
    // The resting composer placeholder is `›` + prompt TEXT, not `› <digit>`,
    // so it must NOT read as a chooser.
    let idle = "› Improve documentation in @filename\ngpt-5.5 xhigh · /repo";
    assert_eq!(agent.detect_state(idle.as_bytes()), Some(AgentState::Idle));
}

#[test]
fn codex_answered_modal_then_working_is_not_input_needed() {
    // After the user answers a modal, Codex resumes work: the approval
    // chrome lingers in the append-only buffer but the live status line is
    // repainted below it. The more-recent work anchor wins → Working, not a
    // stuck InputNeeded.
    let agent = Codex;
    let buf = "Would you like to run the following command?\n\
               › 1. Yes, proceed (y)\n  2. No (esc)\nPress enter to confirm or esc to cancel\n\
               • Running (1s · esc to interrupt)";
    assert_eq!(
        agent.detect_state(buf.as_bytes()),
        Some(AgentState::Working)
    );
}

#[test]
fn codex_same_chunk_repaint_keeps_live_modal_over_status_preview() {
    // The flip side of the test above. A tmux full-screen repaint delivers
    // the SAME bytes — a live approval modal AND an earlier `esc to
    // interrupt` status line — in ONE chunk, status line last. Position
    // alone would read the modal as already-answered (Working); the
    // same-chunk rule must keep it live. Mirrors Claude's
    // `same_chunk_full_repaint_keeps_dialog_over_statusbar`.
    let agent = Codex;
    let buf = "Would you like to run the following command?\n\
               › 1. Yes, proceed (y)\n  2. No (esc)\nPress enter to confirm or esc to cancel\n\
               • Running (1s · esc to interrupt)";
    // Positional (no chunk hint): the work line is most recent → Working.
    assert_eq!(
        agent.detect_state(buf.as_bytes()),
        Some(AgentState::Working)
    );
    // Whole buffer arrived in one repaint (`last_chunk_start = 0`): the
    // status line can't suppress the modal → InputNeeded.
    assert_eq!(
        agent.detect_state_chunked(buf.as_bytes(), 0),
        Some(AgentState::InputNeeded),
    );
}

#[test]
fn codex_requires_ready_before_initial_prompt_injection() {
    // Never paste the work prompt into Codex's directory-trust or approval
    // chooser. The adapter's composer detector is the authoritative gate.
    assert!(Codex.pty_protocol().requires_ready());
}

#[test]
fn claude_detects_chooser_footer() {
    // The Claude Code permission chooser is recognisable by its
    // `Esc to cancel` footer plus a question phrasing and numbered
    // options. The dialog REPLACES the composer, so it carries no
    // `Tab to amend` (that's the idle composer's footer) — with no
    // composer footer below it the chooser is the most-recent marker
    // and reads InputNeeded.
    let agent = Claude;
    let buf = b"Do you want to proceed?\n> 1. Yes\n  2. No\n\n\
                Esc to cancel \xc2\xb7 ctrl+e to explain";
    assert_eq!(agent.detect_state(buf), Some(AgentState::InputNeeded));
}

#[test]
fn claude_idle_when_no_working_hint_and_no_prompt() {
    // Plain text with neither a prompt nor the `(esc to interrupt)`
    // working hint is `Idle` — we never infer "working" from raw
    // output, only from Claude's explicit streaming pulser. This is
    // the acceptance guard: a quiet agent must not look busy.
    let agent = Claude;
    assert_eq!(
        agent.detect_state(b"running tests..."),
        Some(AgentState::Idle)
    );
}

#[test]
fn claude_working_when_interrupt_hint_present() {
    // While streaming / running a tool, Claude paints a pulsing glyph
    // plus `(esc to interrupt)` in its bottom status line. That hint
    // is the per-agent "working" pulser the side panel keys off.
    let agent = Claude;
    let buf = "✻ Cogitating… (12s · ↑ 1.2k tokens · esc to interrupt)";
    assert_eq!(
        agent.detect_state(buf.as_bytes()),
        Some(AgentState::Working),
    );
}

#[test]
fn claude_idle_when_interrupt_hint_is_stale_under_input_box() {
    // Recency guard: a just-finished agent has a now-stale
    // `esc to interrupt` still in the rolling buffer, but the idle
    // input box (`Tab to amend`) was painted AFTER it. The more-
    // recent bottom-line marker wins → Idle, not a forever-busy
    // false positive.
    let agent = Claude;
    let buf = "✻ Working… (esc to interrupt)\n\
               <result streamed>\n\
               │ > \n\
               Esc to cancel · Tab to amend · ctrl+e to explain\n";
    assert_eq!(agent.detect_state(buf.as_bytes()), Some(AgentState::Idle));
}

#[test]
fn claude_idle_at_empty_input_box() {
    // A drawn-but-quiet input box with nothing pending is `Idle`,
    // not `InputNeeded` — "done / waiting for my next instruction"
    // is distinct from "blocking on a specific decision" (#26/#58).
    let agent = Claude;
    let idle = "│ > \n│ \n\n\
                Esc to cancel · Tab to amend · ctrl+e to explain\n";
    assert_eq!(agent.detect_state(idle.as_bytes()), Some(AgentState::Idle));
}

#[test]
fn claude_fresh_startup_banner_is_idle_not_input_needed() {
    // #209: a just-launched agent that hasn't reached a real prompt must
    // never read `InputNeeded`. The fresh-spawn screen carries the version
    // banner (`v2.1.159` — decimals that look like `1.`/`2.` option labels)
    // and the composer prompt glyph `❯`, sitting under the spaceless
    // `? for shortcuts` footer. None of that is a structural prompt, so the
    // detector must report `Idle`.
    let agent = Claude;
    let startup = concat!(
        "╭─── Claude Code v2.1.159 ──────────────────────────────╮\n",
        "│  Welcome back!                                        │\n",
        "│  Opus 4.8 (1M context)                                │\n",
        "╰───────────────────────────────────────────────────────╯\n",
        "❯ Try \"fix lint errors\"\n",
        "? for shortcuts",
    );
    assert_eq!(
        agent.detect_state(startup.as_bytes()),
        Some(AgentState::Idle),
        "the fresh startup banner must not raise a false `?`"
    );
}

#[test]
fn default_agent_detect_state_is_none_so_consumers_render_idle() {
    // The trait default has no detector → None. Consumers treat the
    // absence of a signal as Idle, so an unknown agent never lies
    // about working — even when its output literally contains the
    // Claude working hint.
    struct Unknown;
    impl Agent for Unknown {
        fn id(&self) -> &'static str {
            "unknown"
        }
        fn display_name(&self) -> &'static str {
            "Unknown"
        }
        fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
            vec!["unknown".into()]
        }
    }
    assert_eq!(
        Unknown.detect_state(b"esc to interrupt streaming hard"),
        None
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

/// Regression for the spawn-time injection delay (#142): the newer
/// Claude composer renders only `? for shortcuts` as its footer — NOT
/// the older `Esc to cancel · Tab to amend` pair the previous readiness
/// check required. With that strict pair the ready signal never fired
/// on a healthy idle screen and the injector rode its 10s hard
/// deadline. Readiness now keys off "composer drawn + state isn't
/// asking", so the `? for shortcuts`-only footer reports ready.
#[test]
fn claude_ready_for_prompt_with_short_composer_footer() {
    let agent = Claude;
    let idle = concat!(
        "● All set.\n",
        "╭─────────────────────────────────────────╮\n",
        "│ >                                         │\n",
        "╰─────────────────────────────────────────╯\n",
        "  ? for shortcuts",
    );
    assert!(
        agent.detect_ready_for_prompt(idle.as_bytes()),
        "an idle composer with only the `? for shortcuts` footer must report ready"
    );
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

/// Claude has an authoritative readiness detector, so the spawn-time
/// injector must gate on it — never blind-write past the folder-trust
/// prompt on a settle timer.
#[test]
fn claude_inject_requires_ready() {
    assert!(Claude.pty_protocol().requires_ready());
}

/// Agents that rely on the default always-false
/// `detect_ready_for_prompt` must NOT gate the injector, or every
/// inject would stall to the hard deadline.
#[test]
fn detectorless_agents_do_not_require_ready() {
    assert!(!Cursor.pty_protocol().requires_ready());
    let generic = GenericCli {
        id: "custom",
        display_name: "Custom",
        spawn_cmd: vec!["custom-bin".into()],
        resume_cmd: None,
        asking_patterns: vec![],
    };
    assert!(!generic.pty_protocol().requires_ready());
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
    assert_eq!(
        agent.detect_state(buf.as_bytes()),
        Some(AgentState::InputNeeded),
    );
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
        Some(AgentState::InputNeeded),
        "arrow + 1. Yes anywhere in buffer must fire Asking, even when not adjacent",
    );
}

#[test]
fn claude_detects_choice_arrow_ascii_fallback() {
    // Some terminals / tmux configs render the arrow as `> ` when
    // the UTF-8 glyph isn't available. Cover both shapes.
    let agent = Claude;
    let buf = b"Do you want to make this edit?\n> 1. Yes\n  2. No\n";
    assert_eq!(agent.detect_state(buf), Some(AgentState::InputNeeded));
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
        Some(AgentState::InputNeeded)
    );
    assert_eq!(
        agent.detect_state(b"Install all? [y/N]"),
        Some(AgentState::InputNeeded)
    );
    // No match → Idle, not None: returning None left the cached
    // InputNeeded stuck forever once the prompt was answered.
    assert_eq!(
        agent.detect_state(b"just normal output"),
        Some(AgentState::Idle)
    );
    // ANSI-colored prompts are stripped before matching, like the
    // built-ins — a SGR run inside the marker must not split it.
    assert_eq!(
        agent.detect_state(b"Install all? \x1b[1m[y/N]\x1b[0m"),
        Some(AgentState::InputNeeded)
    );
}

#[test]
fn simple_pattern_agents_ignore_prompts_that_scrolled_past_the_tail() {
    // Codex / Cursor / GenericCli match only the last ~2 KiB of the
    // detect window. A prompt buried under more than that of fresh
    // output was answered long ago — it must read Idle, not pin
    // InputNeeded until the 16 KiB window finally evicts it.
    let mut buf = b"run rm -rf? [y/n]\n".to_vec();
    buf.extend(std::iter::repeat_n(b'x', 4 * 1024));
    assert_eq!(Codex.detect_state(&buf), Some(AgentState::Idle));
    assert_eq!(Cursor.detect_state(&buf), Some(AgentState::Idle));
    let generic = GenericCli {
        id: "custom",
        display_name: "Custom",
        spawn_cmd: vec!["custom".into()],
        resume_cmd: None,
        asking_patterns: vec!["[y/n]".into()],
    };
    assert_eq!(generic.detect_state(&buf), Some(AgentState::Idle));
    // The same prompt within the tail still fires.
    let mut recent = b"some output\n".to_vec();
    recent.extend_from_slice(b"run rm -rf? [y/n]");
    assert_eq!(Codex.detect_state(&recent), Some(AgentState::InputNeeded));
    assert_eq!(generic.detect_state(&recent), Some(AgentState::InputNeeded));
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

// ── Shared detect helpers ─────────────────────────────────────────────
//
// Cover the primitives every agent's `detect_state` now sits on
// top of. The per-agent tests above (`codex_detects_yn_prompt`,
// `claude_detects_chooser_footer`, etc.) cover composition; these
// pin the building blocks.

#[test]
fn contains_any_matches_first_pattern() {
    use lazybox_agents::agent::detect;
    assert!(detect::contains_any("approve? y/n", &["approve?", "(y/n)"]));
    assert!(detect::contains_any(
        "running tests... [y/n]",
        detect::YN_PROMPT_PATTERNS,
    ));
}

#[test]
fn contains_any_returns_false_when_no_match() {
    use lazybox_agents::agent::detect;
    assert!(!detect::contains_any(
        "regular output, no prompts here",
        detect::YN_PROMPT_PATTERNS,
    ));
}

#[test]
fn contains_any_empty_pattern_set_is_false() {
    use lazybox_agents::agent::detect;
    // Edge case: an empty pattern set never matches, even on
    // matching-looking text. The GenericCli `detect_state` guards
    // its empty path explicitly, but the primitive should also be
    // safe.
    assert!(!detect::contains_any("[y/n]", &[]));
}

#[test]
fn contains_paired_requires_both_a_choice_and_a_question() {
    use lazybox_agents::agent::detect;
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
    use lazybox_agents::agent::detect;
    let buf = "Listing options: 1. Yes 2. No";
    assert!(!detect::contains_paired(
        buf,
        &["1. Yes"],
        &["Do you want to", "Approve"],
    ));
}

#[test]
fn contains_paired_with_only_question_is_false() {
    use lazybox_agents::agent::detect;
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
    use lazybox_agents::agent::detect;
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
    assert_eq!(agent.detect_state(buf), Some(AgentState::InputNeeded));
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
            Some(AgentState::InputNeeded),
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
        Some(AgentState::Idle),
    );
    assert_eq!(
        agent.detect_state(b"Reading the manual to figure out how to continue."),
        Some(AgentState::Idle),
    );
}

#[test]
fn claude_freeform_question_is_idle_not_input_needed() {
    // Issue #122: a turn that merely ends on a `?` is NOT a structural
    // prompt — no chooser, no permission footer — so it must read Idle.
    // The old last-line-ends-with-`?` heuristic flagged this as
    // InputNeeded and was the dominant false-positive source (it fired
    // on finished agents whose summary ended in a question).
    let agent = Claude;
    let buf = b"I checked the file.\nShall I delete the cache directory?";
    assert_eq!(agent.detect_state(buf), Some(AgentState::Idle));
}

#[test]
fn claude_done_with_numbered_summary_above_composer_is_idle() {
    // Issue #164: a turn that ends DONE — a plain summary that happens
    // to contain a numbered list — sits above the idle composer. The
    // composer draws its OWN `❯` prompt glyph, so arrow+chooser both
    // match even though nothing is being asked. The `?` pill must NOT
    // appear: a DONE/idle agent with no pending question reads Idle.
    let agent = Claude;
    let buf = "I've finished the task. Summary of changes:\n\
               1. Added the chooser recency guard\n\
               2. Wrote a regression test\n\
               \n\
               ❯ \n\
               ? for shortcuts";
    assert_eq!(
        agent.detect_state(buf.as_bytes()),
        Some(AgentState::Idle),
        "DONE summary with a numbered list above the composer must clear the asking-set",
    );
}

#[test]
fn claude_live_chooser_after_stale_composer_footer_is_input_needed() {
    // The recency guard must NOT suppress a genuine chooser whose
    // composer footer still lingers in the append-only buffer from
    // BEFORE the turn began. The live arrow is re-rendered AFTER that
    // stale footer, so it still reads InputNeeded.
    let agent = Claude;
    let buf = "❯ previous prompt\n\
               ? for shortcuts\n\
               Allow Bash this command?\n\
               ❯ 1. Yes\n\
                 2. No\n";
    assert_eq!(
        agent.detect_state(buf.as_bytes()),
        Some(AgentState::InputNeeded),
        "a live chooser re-rendered after a stale composer footer must still fire",
    );
}

#[test]
fn claude_question_heuristic_stays_idle_on_plain_streaming() {
    // No question mark, no prompt, no working hint → Idle.
    // Belt-and-braces.
    let agent = Claude;
    assert_eq!(
        agent.detect_state(b"Running tests...\nCompiling lazybox-tui v0.1.0\nFinished in 4.2s"),
        Some(AgentState::Idle),
    );
}

#[test]
fn claude_detect_state_handles_multibyte_buffer() {
    // Belt-and-braces: detect_state must never panic on a buffer dense
    // with multi-byte glyphs (`─` is 3 bytes; claude renders it heavily
    // for box-drawing borders). A panic here would kill the
    // per-terminal pump task and leave the host terminal stuck in raw
    // mode with the alt screen still up.
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
fn claude_freeform_conversational_asks_are_idle() {
    // Issue #122 reverses #58's recall-over-precision tradeoff: a
    // freeform conversational ask with NO structural prompt marker
    // (no chooser, no permission footer) now reads Idle. These were
    // the dominant false-positive engine — agents routinely end a turn
    // on prose that ends in `?` or a confirmation phrase while nothing
    // is actually blocking — and they fired spurious desktop
    // notifications (#110).
    let agent = Claude;
    for buf in [
        "I've finished the implementation. Want me to run the tests now?".as_bytes(),
        "Some context above\nProceed with the rewrite?".as_bytes(),
        "Should I also update the tests for this change?".as_bytes(),
        "Let me know if you'd like me to continue.".as_bytes(),
        b"Reading through the file now to figure out which of the three \
          possible refactors would land us the cleanest API surface, so \
          we extract the inner type or keep it inline?",
    ] {
        assert_eq!(
            agent.detect_state(buf),
            Some(AgentState::Idle),
            "freeform ask must read Idle: {:?}",
            String::from_utf8_lossy(buf),
        );
    }
}

// ── Prompt-shape fixture suite ─────────────────────────────────────────
//
// A canonical version of every Claude Code prompt shape lazybox's
// detector is expected to recognise. Each fixture pairs a stable
// name with a representative buffer and the expected `AgentState`.
// The single test below iterates so adding a new prompt shape is a
// one-fixture entry — no per-shape `#[test]` boilerplate.
//
// Maintainer flow:
//   1. Capture the new shape (see `/tmp/lazybox.log` when claude
//      renders the prompt in lazybox, or transcribe the visible
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
    /// Expected `AgentState`. `InputNeeded` for real prompts,
    /// `Working` for the streaming pulser, `Idle` for false-positive
    /// controls (chat output that LOOKS like a prompt but isn't, or a
    /// quiet input box).
    expected: AgentState,
}

const PROMPT_FIXTURES: &[PromptFixture] = &[
    // ── InputNeeded shapes — the real prompts ────────────────────
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
        expected: AgentState::InputNeeded,
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
        expected: AgentState::InputNeeded,
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
        expected: AgentState::InputNeeded,
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
        expected: AgentState::InputNeeded,
    },
    PromptFixture {
        name: "plan_mode_exit",
        buffer: concat!(
            "Do you want to proceed?\n",
            "❯ 1. Yes\n",
            "  2. No\n",
            "Esc to cancel",
        ),
        expected: AgentState::InputNeeded,
    },
    PromptFixture {
        name: "trust_folder_chooser",
        buffer: concat!(
            "Do you trust the files in this folder?\n",
            "❯ 1. Yes, proceed\n",
            "  2. No, exit\n",
            "Esc to cancel",
        ),
        expected: AgentState::InputNeeded,
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
        expected: AgentState::InputNeeded,
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
        expected: AgentState::InputNeeded,
    },
    PromptFixture {
        // Standalone "do you want to <verb>" phrase variants —
        // each one is a permission/consent prompt class.
        name: "write_permission_standalone_no_chooser",
        buffer: "Do you want to create README.md?",
        expected: AgentState::InputNeeded,
    },
    PromptFixture {
        name: "overwrite_permission_standalone",
        buffer: "Do you want to overwrite the existing file?",
        expected: AgentState::InputNeeded,
    },
    PromptFixture {
        name: "delete_permission_standalone",
        buffer: "Do you want to delete src/old_module.rs?",
        expected: AgentState::InputNeeded,
    },
    PromptFixture {
        name: "settings_edit_consent",
        buffer: concat!(
            "Claude wants to edit its own settings. Allow?\n",
            "❯ 1. Yes\n",
            "  2. No\n",
            "Esc to cancel",
        ),
        expected: AgentState::InputNeeded,
    },
    // ── Working shapes — the live status line ────────────────────
    PromptFixture {
        // Claude's bottom status line while the model streams /
        // a tool runs. The `(esc to interrupt)` hint is one stable
        // signal; the leading glyph cycles and isn't matched.
        name: "working_streaming_status_line",
        buffer: "✻ Cogitating… (8s · ↑ 412 tokens · esc to interrupt)",
        expected: AgentState::Working,
    },
    PromptFixture {
        // The dogfooding format (issue #122): the newer status line
        // replaces the interrupt hint with a phase suffix. The
        // `(<elapsed> · … tokens …)` live counter is the signal here —
        // proves Working detection no longer depends on the literal
        // `esc to interrupt`.
        name: "working_status_line_phase_suffix_no_interrupt_hint",
        buffer: "✦ Gusting… (2m 2s · ↓ 7.2k tokens · thinking some more)",
        expected: AgentState::Working,
    },
    PromptFixture {
        // A consent phrase the user already answered, with the LIVE
        // working status line painted after it. tmux repaints the
        // token-counter / `esc to interrupt` line continuously while
        // the agent is busy, but the static prompt footer is never
        // re-sent — so the stale phrase above the working anchor must
        // read Working, not pin InputNeeded until it scrolls out of
        // the detect window.
        name: "working_status_line_after_stale_consent_phrase",
        buffer: "Do you want to proceed?\n\u{273b} (8s · ↑ 412 tokens · esc to interrupt)",
        expected: AgentState::Working,
    },
    PromptFixture {
        // Tool-run variant — same interrupt hint, different verb.
        name: "working_running_tool",
        buffer: concat!(
            "● Bash(cargo test)\n",
            "  ⎿ Running…\n",
            "✽ Running… (3s · esc to interrupt)",
        ),
        expected: AgentState::Working,
    },
    // ── Idle controls — must NOT fire as InputNeeded or Working ──
    PromptFixture {
        // Stale `esc to interrupt` left in the buffer, but the idle
        // input box footer was painted AFTER it. Recency guard must
        // pick Idle, not a forever-busy Working.
        name: "idle_stale_interrupt_under_input_box",
        buffer: concat!(
            "✻ Working… (esc to interrupt)\n",
            "Done.\n",
            "│ > \n",
            "Esc to cancel · Tab to amend · ctrl+e to explain",
        ),
        expected: AgentState::Idle,
    },
    PromptFixture {
        // Quiet input box, nothing pending — done / waiting for the
        // next instruction, which is Idle (not InputNeeded).
        name: "idle_empty_input_box",
        buffer: concat!("│ > \n", "Esc to cancel · Tab to amend · ctrl+e to explain",),
        expected: AgentState::Idle,
    },
    PromptFixture {
        // Regression for issue #101 false positives: an IDLE input box
        // (footer carries both `Esc to cancel` AND `Tab to amend`) with
        // an ordinary numbered list in the scrollback above it. The
        // `Esc to cancel + numbered-options` permission-footer branch
        // must NOT fire here — a real permission dialog replaces the
        // input box, so its footer lacks `Tab to amend`. Pre-fix, every
        // idle session that had typed a `1.`/`2.` list anywhere in the
        // recent buffer wore a spurious `?` pill.
        name: "idle_input_box_with_numbered_list_above_is_not_a_prompt",
        buffer: concat!(
            "Here's the plan:\n",
            "  1. Refactor the parser\n",
            "  2. Add tests\n",
            "│ > \n",
            "Esc to cancel · Tab to amend · ctrl+e to explain",
        ),
        expected: AgentState::Idle,
    },
    PromptFixture {
        // Plain build output. No prompt markers, no `?`. Belt-
        // and-braces baseline.
        name: "idle_streaming_build_output",
        buffer: concat!(
            "Compiling lazybox-tui v0.1.0\n",
            "Finished release [optimized] target(s) in 4.32s",
        ),
        expected: AgentState::Idle,
    },
    PromptFixture {
        // Chat output that mentions "do you want to" but isn't
        // an actual prompt. The STANDALONE entries are tight
        // enough not to fire on this prose.
        name: "idle_chat_mentions_proceed",
        buffer: "I'll proceed with the change once you say so.",
        expected: AgentState::Idle,
    },
    PromptFixture {
        // A `?` appears mid-paragraph but the turn ends on a
        // statement. No structural prompt marker → Idle.
        name: "idle_question_mid_paragraph_not_last_line",
        buffer: concat!(
            "Why does this matter? Because the cache is cold on first run.\n",
            "Done — all tests pass.",
        ),
        expected: AgentState::Idle,
    },
    PromptFixture {
        // The concrete repro from issue #122: a finished agent —
        // completion summary ending in a CI URL, the `Brewed for`
        // done-marker, and the injected prompt still parked in the
        // composer. No chooser, no working status line → Idle. Pre-fix
        // the conversational-ask path mis-read the composer / summary
        // as a pending question and flagged InputNeeded.
        name: "idle_finished_with_parked_prompt_in_composer",
        buffer: concat!(
            "● Pushed the fix. CI: https://github.com/o/r/actions/runs/123\n",
            "✻ Brewed for 4m 21s\n",
            "╭──────────────────────────────────╮\n",
            "│ ❯ watch CI until it passes        │\n",
            "╰──────────────────────────────────╯\n",
            "  ? for shortcuts",
        ),
        expected: AgentState::Idle,
    },
    // ── Freeform conversational asks — deliberately Idle (#122) ───
    // Claude's OWN freeform asks ending a turn — no menu, no footer,
    // just prose parked on a question. Issue #58 once flagged these
    // for recall; #122 reverses that — they were the dominant
    // false-positive engine (and fired spurious notifications), and a
    // freeform `?` is not reliably distinguishable from a finished
    // turn. They now read Idle. Kept as controls so a future change
    // can't silently re-introduce the false positive.
    PromptFixture {
        // The canonical #58 case, rendered with the input box + footer
        // below it. No structural marker anywhere → Idle.
        name: "freeform_want_me_to_with_input_box_is_idle",
        buffer: concat!(
            "● Want me to proceed?\n",
            "\n",
            "╭─────────────────────────────────────────╮\n",
            "│ >                                         │\n",
            "╰─────────────────────────────────────────╯\n",
            "  ? for shortcuts",
        ),
        expected: AgentState::Idle,
    },
    PromptFixture {
        // Ends with `.`, not `?`, and carries a confirmation phrase —
        // the old phrase-list path. Now Idle.
        name: "freeform_let_me_know_is_idle",
        buffer: "Let me know if you'd like me to continue.",
        expected: AgentState::Idle,
    },
    PromptFixture {
        // Bare one-word confirmation ask. Now Idle.
        name: "freeform_proceed_bare_is_idle",
        buffer: "Proceed?",
        expected: AgentState::Idle,
    },
    PromptFixture {
        // Long clarifying prose ending in a choice question. Now Idle.
        name: "freeform_long_prose_ends_with_question_is_idle",
        buffer: "I have a few approaches in mind: refactor the loop, extract a helper, or inline the whole thing. Which would you prefer?",
        expected: AgentState::Idle,
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
