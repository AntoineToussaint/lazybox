//! Agent lifecycle hooks → lazybox state.
//!
//! Claude Code fires structured hook events at lifecycle points (`Stop`,
//! `Notification`, `PreToolUse`, …). lazybox injects a hook command at
//! spawn (see [`crate::hook_settings`]) so the daemon receives these as
//! deterministic JSON payloads instead of reverse-engineering the
//! rendered TUI from the PTY byte stream (`crate::detect`).
//!
//! Codex's hook system is wire-compatible: it delivers the same
//! PascalCase `hook_event_name` discriminants (`SessionStart`, `Stop`,
//! `UserPromptSubmit`, `PreToolUse`, …) as JSON on the hook command's
//! stdin, so both agents flow through [`parse_claude_hook`] unchanged.
//! Codex is wired through spawn-argv `-c hooks.*` overrides rather than a
//! settings file (see [`crate::agent::Agent::hook_command_args`]), but
//! once ingested the two are indistinguishable — the authoritative signal
//! the daemon consumes, not a per-agent PTY guess.
//!
//! Two pure functions live here, both exercised against captured
//! payload fixtures in `tests/`:
//!   - [`parse_claude_hook`] — the agent's wire JSON → the IPC-stable
//!     [`HookEvent`].
//!   - [`hook_to_state`] — [`HookEvent`] → the [`AgentState`] transition
//!     it implies (or `None` when the event doesn't change state).

use lazybox_ipc::{AgentState, HookEvent, HookEventKind};
use serde_json::Value;

/// Parse one Claude Code hook payload (the JSON it writes to a hook
/// command's stdin) into a normalized [`HookEvent`]. Returns `None`
/// only when the input isn't JSON at all — a payload with an unknown
/// `hook_event_name` still parses, as [`HookEventKind::Other`].
pub fn parse_claude_hook(json: &str) -> Option<HookEvent> {
    let value: Value = serde_json::from_str(json.trim()).ok()?;
    Some(hook_from_value(&value))
}

fn hook_from_value(v: &Value) -> HookEvent {
    HookEvent {
        kind: kind_from_name(str_field(v, "hook_event_name")),
        session_id: str_field(v, "session_id").map(str::to_string),
        cwd: str_field(v, "cwd").map(str::to_string),
        tool_name: str_field(v, "tool_name").map(str::to_string),
        // Claude has used both `notification_type` (the documented
        // discriminant) and the free-text `message`; take whichever is
        // present so the permission/idle distinction in `hook_to_state`
        // can fire on either wire shape.
        notification: str_field(v, "notification_type")
            .or_else(|| str_field(v, "message"))
            .map(str::to_string),
    }
}

fn kind_from_name(name: Option<&str>) -> HookEventKind {
    match name.unwrap_or_default() {
        "SessionStart" => HookEventKind::SessionStart,
        "SessionEnd" => HookEventKind::SessionEnd,
        "UserPromptSubmit" => HookEventKind::UserPromptSubmit,
        "PreToolUse" => HookEventKind::PreToolUse,
        "PostToolUse" => HookEventKind::PostToolUse,
        "Notification" => HookEventKind::Notification,
        "Stop" => HookEventKind::Stop,
        "SubagentStop" => HookEventKind::SubagentStop,
        "PreCompact" => HookEventKind::PreCompact,
        _ => HookEventKind::Other,
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// Map a hook event to the [`AgentState`] it implies, or `None` when
/// the event carries no state change. `current` is the terminal's
/// cached state at the moment the hook arrived — only the unrecognized-
/// `Notification` case consults it (see `notification_state`).
///
/// The mapping over the events Claude Code actually fires:
///   - a submitted prompt, tool use, and compaction mean the main
///     agent is **busy** → [`AgentState::Working`] (`UserPromptSubmit`
///     matters: a turn that streams text with no tool calls fires no
///     other working-shaped hook at all);
///   - `SubagentStop` (a Task-tool subagent finished) only KEEPS an
///     already-working agent working — it never resurrects a settled
///     `Done`/`Idle`/`InputNeeded` agent, because it can arrive after
///     the main turn's `Stop` and would otherwise strand it on
///     "running" forever;
///   - a `Notification` means Claude is **waiting on the user** →
///     [`AgentState::InputNeeded`]: either a permission / elicitation
///     dialog, or — the only variant that fires under lazybox's
///     `--dangerously-skip-permissions` spawns — the idle nudge it emits
///     after sitting ~60s at a ready composer ("Claude is waiting for
///     your input"), which is exactly the "this workspace needs me"
///     signal the `?` indicator and `!` jump surface (#62) — except when
///     that nudge fires before any turn has run (see `notification_state`
///     for the #209 startup carve-out);
///   - `Stop` means the agent **finished its turn** →
///     [`AgentState::Done`]: it ran work and has come to rest, the
///     "completed, take a look" signal the user wants alerting on (#80);
///   - `SessionStart`/`SessionEnd` mean the composer is **quiet with no
///     work behind it** → [`AgentState::Idle`].
///
/// `PermissionRequest` / `SubagentStart` / `PostCompact` are wire
/// variants Claude Code never fires — they map to no transition, like
/// `Other`. `Stop` does NOT fire on a manual interrupt (Ctrl-C / Esc);
/// the daemon keeps the PTY detector as a fallback for that gap.
pub fn hook_to_state(event: &HookEvent, current: Option<AgentState>) -> Option<AgentState> {
    let state = match event.kind {
        HookEventKind::UserPromptSubmit
        | HookEventKind::PreToolUse
        | HookEventKind::PostToolUse
        | HookEventKind::PreCompact => AgentState::Working,
        // A subagent (Task tool) finishing does NOT mean the top-level
        // turn resumed. Claude fires `SubagentStop` when a spawned
        // subagent completes, and it can land AFTER the main agent's
        // `Stop` (observed 2–5s later in the wild). Mapping it to
        // `Working` unconditionally then resurrects a just-settled
        // `Done` agent and strands it on "running" forever — the main
        // `Stop` already fired and won't fire again. So only keep an
        // ALREADY-working agent working; never pull a settled
        // (`Done`/`Idle`/`InputNeeded`) or `Exited` agent back into
        // `Working`. (PreToolUse/PostToolUse carry the real mid-turn
        // Working signal, so this loses nothing during a live turn.)
        HookEventKind::SubagentStop => match current {
            Some(AgentState::Working) => AgentState::Working,
            _ => return None,
        },
        HookEventKind::Notification => {
            return notification_state(event.notification.as_deref(), current);
        }
        HookEventKind::Stop => AgentState::Done,
        HookEventKind::SessionStart | HookEventKind::SessionEnd => AgentState::Idle,
        HookEventKind::PermissionRequest
        | HookEventKind::SubagentStart
        | HookEventKind::PostCompact
        | HookEventKind::Other => return None,
    };
    Some(state)
}

/// Classify a `Notification` payload. Claude fires this hook when it is
/// waiting on the user — a permission / elicitation dialog, or the idle
/// nudge ("Claude is waiting for your input") it emits after ~60s at a
/// ready composer. lazybox runs every agent with
/// `--dangerously-skip-permissions`, so the permission/elicitation
/// variants never fire in practice and the idle nudge is the only
/// `Notification` an agent emits; after a real turn it means the
/// workspace is blocked on me → `InputNeeded` (#62).
///
/// The one exception is the idle nudge fired BEFORE the agent has run
/// anything (#209). The nudge is an inactivity timeout, not a structural
/// gate, so at a freshly-spawned composer — still at its initial
/// `Idle`/unknown state, waiting on the work prompt or the user's first
/// keystroke — it's premature startup chrome, not "paused waiting on me."
/// Badging `?` then raises a false input alert at launch, so that case
/// stays `Idle`. A real permission / elicitation dialog only ever fires
/// mid-turn (its tool needs a submitted prompt first), so it's unaffected.
///
/// We match the waiting case affirmatively against the known wording. An
/// unrecognized notification is treated as the composer sitting ready →
/// `Idle`, with one exception: when the terminal is already
/// `InputNeeded`, an unrecognized payload is a NO-CHANGE (`None`) rather
/// than a force-clear, so it can't kill a live `?`. The pill still
/// clears via `Stop`, a resumed turn, or the PTY detector going stale.
fn notification_state(
    notification: Option<&str>,
    current: Option<AgentState>,
) -> Option<AgentState> {
    match notification {
        Some(n) if is_idle_nudge(n) && !has_run_a_turn(current) => {
            // Premature startup nudge: keep a never-used composer `Idle` so
            // it doesn't badge `?` at launch — but never force-clear a gate
            // already up (the no-change rule below).
            (current != Some(AgentState::InputNeeded)).then_some(AgentState::Idle)
        }
        Some(n) if waits_on_user(n) => Some(AgentState::InputNeeded),
        _ if current == Some(AgentState::InputNeeded) => None,
        _ => Some(AgentState::Idle),
    }
}

/// Whether the terminal has run at least one turn — `Working` (a turn in
/// flight) or `Done` (a turn finished). A fresh session sits at
/// `None`/`Idle` (the latter from `SessionStart`) until its first working
/// hook, so this distinguishes a genuine post-turn idle nudge (raise `?`,
/// #62) from the premature one Claude emits at a never-used composer
/// (#209).
fn has_run_a_turn(current: Option<AgentState>) -> bool {
    matches!(current, Some(AgentState::Working | AgentState::Done))
}

/// The idle nudge Claude fires after the composer sits ready ~60s with no
/// input ("Claude is waiting for your input"). Distinct from the
/// permission / elicitation payloads [`waits_on_user`] also matches: those
/// are live gates, this is an inactivity timeout.
fn is_idle_nudge(notification: &str) -> bool {
    notification
        .to_ascii_lowercase()
        .contains("waiting for your input")
}

/// Wording in a `Notification` payload that means Claude is blocked
/// waiting on the user: a permission / elicitation dialog, or the idle
/// nudge it fires after sitting ~60s at a ready composer with no input.
fn waits_on_user(notification: &str) -> bool {
    let n = notification.to_ascii_lowercase();
    n.contains("permission") || n.contains("elicit") || n.contains("waiting for your input")
}

/// Shape of the prompt a waiting `Notification` implies. Claude's
/// permission payloads render chooser dialogs (a bare digit / y / n /
/// Esc answers them); elicitation payloads and the idle nudge collect
/// free text, where a bare keystroke is just typing. Only meaningful for
/// notifications [`hook_to_state`] mapped to `InputNeeded`.
pub fn notification_prompt_shape(notification: Option<&str>) -> crate::agent::PromptShape {
    match notification {
        Some(n) if expects_free_text(n) => crate::agent::PromptShape::FreeText,
        _ => crate::agent::PromptShape::Chooser,
    }
}

fn expects_free_text(notification: &str) -> bool {
    let n = notification.to_ascii_lowercase();
    n.contains("elicit") || n.contains("waiting for your input")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> HookEvent {
        parse_claude_hook(json).expect("valid hook JSON")
    }

    #[test]
    fn parse_extracts_common_fields() {
        let ev = parse(
            r#"{"hook_event_name":"PreToolUse","session_id":"abc",
                "cwd":"/work","tool_name":"Bash","permission_mode":"default"}"#,
        );
        assert_eq!(ev.kind, HookEventKind::PreToolUse);
        assert_eq!(ev.session_id.as_deref(), Some("abc"));
        assert_eq!(ev.cwd.as_deref(), Some("/work"));
        assert_eq!(ev.tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn parse_unknown_hook_name_is_other_not_none() {
        let ev = parse(r#"{"hook_event_name":"SomethingNew","session_id":"x"}"#);
        assert_eq!(ev.kind, HookEventKind::Other);
        assert_eq!(hook_to_state(&ev, None), None);
    }

    #[test]
    fn names_claude_never_fires_parse_as_other() {
        // These were once in the hooked set but Claude Code doesn't
        // fire them; they must not map to a state transition.
        for name in ["PermissionRequest", "SubagentStart", "PostCompact"] {
            let ev = parse(&format!(r#"{{"hook_event_name":"{name}"}}"#));
            assert_eq!(ev.kind, HookEventKind::Other, "{name} should be Other");
            assert_eq!(hook_to_state(&ev, None), None, "{name} should be a no-op");
        }
    }

    #[test]
    fn parse_rejects_non_json() {
        assert!(parse_claude_hook("not json at all").is_none());
        assert!(parse_claude_hook("").is_none());
    }

    #[test]
    fn tool_and_compaction_are_working() {
        for name in ["PreToolUse", "PostToolUse", "PreCompact"] {
            let ev = parse(&format!(r#"{{"hook_event_name":"{name}"}}"#));
            assert_eq!(
                hook_to_state(&ev, None),
                Some(AgentState::Working),
                "{name} should be Working",
            );
        }
    }

    #[test]
    fn subagent_stop_never_resurrects_a_settled_agent() {
        // Regression: a `SubagentStop` can land AFTER the main agent's
        // `Stop` (observed 2–5s later in real logs). It must NOT pull a
        // settled agent back into Working — doing so stranded agents on
        // "running" forever because the main `Stop` won't fire again.
        let ev = parse(r#"{"hook_event_name":"SubagentStop"}"#);
        // Already Working → stays Working (mid-turn subagent completion).
        assert_eq!(
            hook_to_state(&ev, Some(AgentState::Working)),
            Some(AgentState::Working),
        );
        // Settled / finished states are untouched (no transition).
        for settled in [
            AgentState::Done,
            AgentState::Idle,
            AgentState::InputNeeded,
            AgentState::Exited { code: None },
        ] {
            assert_eq!(
                hook_to_state(&ev, Some(settled)),
                None,
                "SubagentStop must be a no-op from {settled:?}, not a resurrection to Working",
            );
        }
        // Unknown/no cached state → no-op (a later real signal sets it).
        assert_eq!(hook_to_state(&ev, None), None);
    }

    #[test]
    fn user_prompt_submit_is_working() {
        // Without this, a turn that streams text with no tool calls
        // produces NO working signal at all in hook mode.
        let ev = parse(r#"{"hook_event_name":"UserPromptSubmit","session_id":"abc"}"#);
        assert_eq!(ev.kind, HookEventKind::UserPromptSubmit);
        assert_eq!(hook_to_state(&ev, None), Some(AgentState::Working));
        // ...regardless of what the terminal was doing before.
        assert_eq!(
            hook_to_state(&ev, Some(AgentState::Idle)),
            Some(AgentState::Working)
        );
    }

    #[test]
    fn stop_is_done() {
        // `Stop` fires when the agent finishes its turn — the
        // "completed, take a look" signal (#80), distinct from a fresh
        // composer that never ran work.
        let ev = parse(r#"{"hook_event_name":"Stop","session_id":"abc"}"#);
        assert_eq!(hook_to_state(&ev, None), Some(AgentState::Done));
        assert_eq!(
            hook_to_state(&ev, Some(AgentState::Working)),
            Some(AgentState::Done),
        );
    }

    #[test]
    fn session_lifecycle_is_idle() {
        for name in ["SessionStart", "SessionEnd"] {
            let ev = parse(&format!(r#"{{"hook_event_name":"{name}"}}"#));
            assert_eq!(
                hook_to_state(&ev, None),
                Some(AgentState::Idle),
                "{name} should be Idle",
            );
        }
    }

    #[test]
    fn notification_permission_prompt_is_input_needed() {
        let ev =
            parse(r#"{"hook_event_name":"Notification","notification_type":"permission_prompt"}"#);
        assert_eq!(hook_to_state(&ev, None), Some(AgentState::InputNeeded));
    }

    #[test]
    fn notification_permission_message_is_input_needed() {
        // The free-text wording Claude actually sends when blocking on a
        // tool approval.
        let ev = parse(
            r#"{"hook_event_name":"Notification","message":"Claude needs your permission to use Bash"}"#,
        );
        assert_eq!(hook_to_state(&ev, None), Some(AgentState::InputNeeded));
    }

    #[test]
    fn notification_prompt_shape_splits_chooser_from_free_text() {
        use crate::agent::PromptShape;
        // Permission dialogs are choosers — one keystroke answers.
        assert_eq!(
            notification_prompt_shape(Some("permission_prompt")),
            PromptShape::Chooser
        );
        assert_eq!(
            notification_prompt_shape(Some("Claude needs your permission to use Bash")),
            PromptShape::Chooser
        );
        // Elicitation dialogs collect free text — a bare digit is typing.
        assert_eq!(
            notification_prompt_shape(Some("elicitation_dialog")),
            PromptShape::FreeText
        );
        // The idle nudge also collects free text: the user types their
        // next prompt, not a one-keystroke chooser answer.
        assert_eq!(
            notification_prompt_shape(Some("Claude is waiting for your input")),
            PromptShape::FreeText
        );
    }

    #[test]
    fn notification_elicitation_is_input_needed() {
        let ev =
            parse(r#"{"hook_event_name":"Notification","notification_type":"elicitation_dialog"}"#);
        assert_eq!(hook_to_state(&ev, None), Some(AgentState::InputNeeded));
    }

    #[test]
    fn notification_idle_waiting_for_input_is_input_needed_after_a_turn() {
        // Claude's idle nudge after ~60s of inactivity. lazybox spawns
        // every agent with `--dangerously-skip-permissions`, so the
        // permission/elicitation notifications never fire — this nudge is
        // the only "agent is waiting on you" signal available, and #62
        // wants it to drive the `?` indicator on the row. It only counts
        // once the agent has actually run a turn (`Working`/`Done`).
        let ev = parse(
            r#"{"hook_event_name":"Notification","message":"Claude is waiting for your input"}"#,
        );
        assert_eq!(
            hook_to_state(&ev, Some(AgentState::Working)),
            Some(AgentState::InputNeeded)
        );
        assert_eq!(
            hook_to_state(&ev, Some(AgentState::Done)),
            Some(AgentState::InputNeeded)
        );
    }

    #[test]
    fn idle_nudge_at_startup_before_any_turn_stays_idle() {
        // #209: the idle nudge can reach lazybox while a freshly-spawned
        // agent is still sitting at its initial composer — before the work
        // prompt is injected (autonomous) or the user's first keystroke
        // (interactive). With no turn behind it the nudge is startup
        // chrome, not a gate, so it must NOT badge `?`. A fresh terminal is
        // `None` (no hook yet) or `Idle` (after `SessionStart`).
        let ev = parse(
            r#"{"hook_event_name":"Notification","message":"Claude is waiting for your input"}"#,
        );
        assert_eq!(hook_to_state(&ev, None), Some(AgentState::Idle));
        assert_eq!(
            hook_to_state(&ev, Some(AgentState::Idle)),
            Some(AgentState::Idle)
        );
    }

    #[test]
    fn premature_idle_nudge_never_clears_a_live_gate() {
        // The startup suppression must not knock down a `?` already up: an
        // idle nudge arriving while the terminal is `InputNeeded` is a
        // no-change, never a force-clear (same protection as an
        // unrecognized notification).
        let ev = parse(
            r#"{"hook_event_name":"Notification","message":"Claude is waiting for your input"}"#,
        );
        assert_eq!(hook_to_state(&ev, Some(AgentState::InputNeeded)), None);
    }

    #[test]
    fn permission_dialog_is_input_needed_regardless_of_prior_turn() {
        // A genuine permission gate only ever fires mid-turn, but assert
        // it's unaffected by the startup-nudge suppression even from a
        // fresh state — it's a structural gate, not an inactivity timeout.
        let ev = parse(
            r#"{"hook_event_name":"Notification","message":"Claude needs your permission to use Bash"}"#,
        );
        assert_eq!(hook_to_state(&ev, None), Some(AgentState::InputNeeded));
    }

    #[test]
    fn notification_without_type_defaults_to_idle() {
        // An unrecognized notification is far more likely the idle nudge
        // than a real prompt; real prompts say "permission"/"elicit".
        let ev = parse(r#"{"hook_event_name":"Notification"}"#);
        assert_eq!(hook_to_state(&ev, None), Some(AgentState::Idle));
        assert_eq!(
            hook_to_state(&ev, Some(AgentState::Working)),
            Some(AgentState::Idle)
        );
    }

    // Codex hook payloads (captured from `codex 0.145.0` with our
    // injected `-c hooks.*` overrides) flow through the identical parser
    // and state map as Claude's — the authoritative signal that replaces
    // screen-scraping (#430). Screen-scraping topped out at `Idle`; the
    // real `Stop` hook reaches `Done` (#357).
    #[test]
    fn codex_session_start_payload_parses_to_idle() {
        let ev = parse(
            r#"{"session_id":"019f8e32-33b4-7910-852c-74859c95d519",
                "transcript_path":"/x/rollout.jsonl","cwd":"/w",
                "hook_event_name":"SessionStart","model":"gpt-5.6-sol",
                "permission_mode":"bypassPermissions","source":"startup"}"#,
        );
        assert_eq!(ev.kind, HookEventKind::SessionStart);
        assert_eq!(
            ev.session_id.as_deref(),
            Some("019f8e32-33b4-7910-852c-74859c95d519")
        );
        assert_eq!(hook_to_state(&ev, None), Some(AgentState::Idle));
    }

    #[test]
    fn codex_stop_payload_reaches_done() {
        // The signal Codex's screen-scraper could never produce.
        let ev = parse(
            r#"{"session_id":"019f8e32-33b4-7910-852c-74859c95d519",
                "turn_id":"019f8e32-345c-7650-81f4-c51f605ff397",
                "transcript_path":"/x/rollout.jsonl","cwd":"/w",
                "hook_event_name":"Stop","model":"gpt-5.6-sol",
                "permission_mode":"bypassPermissions","stop_hook_active":false,
                "last_assistant_message":"ok"}"#,
        );
        assert_eq!(ev.kind, HookEventKind::Stop);
        assert_eq!(
            hook_to_state(&ev, Some(AgentState::Working)),
            Some(AgentState::Done),
        );
    }

    #[test]
    fn codex_tool_use_payload_is_working() {
        let ev = parse(
            r#"{"session_id":"019f8e32-c41b-7ef0-b4f4-5cf767f2cf91",
                "turn_id":"019f8e32-c47d-7f31-9c2e-731896c3714e",
                "transcript_path":"/x/rollout.jsonl","cwd":"/w",
                "hook_event_name":"PreToolUse","model":"gpt-5.6-sol",
                "permission_mode":"bypassPermissions","tool_name":"Bash",
                "tool_input":{"command":"echo hello-from-tool"},
                "tool_use_id":"call_mhdzyyGJCZ2kZzqpAUahquMA"}"#,
        );
        assert_eq!(ev.kind, HookEventKind::PreToolUse);
        assert_eq!(ev.tool_name.as_deref(), Some("Bash"));
        assert_eq!(
            hook_to_state(&ev, Some(AgentState::Idle)),
            Some(AgentState::Working)
        );
    }

    #[test]
    fn codex_user_prompt_submit_payload_is_working() {
        let ev = parse(
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"019f8e32",
                "cwd":"/w","prompt":"do the thing"}"#,
        );
        assert_eq!(
            hook_to_state(&ev, Some(AgentState::Idle)),
            Some(AgentState::Working)
        );
    }

    #[test]
    fn unrecognized_notification_does_not_clear_input_needed() {
        // An unrecognized notification while the terminal is blocked is
        // a NO-CHANGE, not a force-clear: an unrecognized *blocking*
        // notification must not kill the `?` pill.
        let ev = parse(
            r#"{"hook_event_name":"Notification","message":"Some wording lazybox has never seen"}"#,
        );
        assert_eq!(hook_to_state(&ev, Some(AgentState::InputNeeded)), None);
        // A recognized blocking notification still (re)asserts the pill.
        let blocking = parse(
            r#"{"hook_event_name":"Notification","message":"Claude needs your permission to use Bash"}"#,
        );
        assert_eq!(
            hook_to_state(&blocking, Some(AgentState::InputNeeded)),
            Some(AgentState::InputNeeded)
        );
    }
}
