//! Claude Code lifecycle hooks → lazybox state.
//!
//! Claude Code fires structured hook events at lifecycle points (`Stop`,
//! `Notification`, `PreToolUse`, …). lazybox injects a hook command at
//! spawn (see [`crate::hook_settings`]) so the daemon receives these as
//! deterministic JSON payloads instead of reverse-engineering the
//! rendered TUI from the PTY byte stream (`crate::detect`).
//!
//! Two pure functions live here, both exercised against captured
//! payload fixtures in `tests/`:
//!   - [`parse_claude_hook`] — Claude's wire JSON → the IPC-stable
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
        "PreToolUse" => HookEventKind::PreToolUse,
        "PostToolUse" => HookEventKind::PostToolUse,
        "Notification" => HookEventKind::Notification,
        "PermissionRequest" => HookEventKind::PermissionRequest,
        "Stop" => HookEventKind::Stop,
        "SubagentStart" => HookEventKind::SubagentStart,
        "SubagentStop" => HookEventKind::SubagentStop,
        "PreCompact" => HookEventKind::PreCompact,
        "PostCompact" => HookEventKind::PostCompact,
        _ => HookEventKind::Other,
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// Map a hook event to the [`AgentState`] it implies, or `None` when
/// the event carries no state change (e.g. an unmapped hook name).
///
/// The mapping is deliberately total over the lifecycle:
///   - tool use, compaction, and subagent activity all mean the main
///     agent is **busy** → [`AgentState::Working`];
///   - a permission request, or a `Notification` whose text asks for
///     permission or elicitation, means Claude is **waiting on the
///     user** → [`AgentState::InputNeeded`];
///   - `Stop`, `SessionStart`/`SessionEnd`, and every other
///     `Notification` (the idle "waiting for your input" nudge
///     included) mean the composer is **quiet** → [`AgentState::Idle`].
///
/// `Stop` does NOT fire on a manual interrupt (Ctrl-C / Esc); the
/// daemon keeps the PTY detector as a fallback for that gap.
pub fn hook_to_state(event: &HookEvent) -> Option<AgentState> {
    let state = match event.kind {
        HookEventKind::PreToolUse
        | HookEventKind::PostToolUse
        | HookEventKind::PreCompact
        | HookEventKind::PostCompact
        | HookEventKind::SubagentStart
        | HookEventKind::SubagentStop => AgentState::Working,
        HookEventKind::PermissionRequest => AgentState::InputNeeded,
        HookEventKind::Notification => notification_state(event.notification.as_deref()),
        HookEventKind::Stop | HookEventKind::SessionStart | HookEventKind::SessionEnd => {
            AgentState::Idle
        }
        HookEventKind::Other => return None,
    };
    Some(state)
}

/// Classify a `Notification` payload. A notification only blocks the
/// user when it asks for permission or surfaces an elicitation dialog;
/// the idle nudge ("Claude is waiting for your input") and anything
/// else mean the composer is sitting ready, not blocked → `Idle`.
///
/// We match the blocking case affirmatively and default the rest to
/// `Idle`: Claude's permission/elicitation payloads carry a stable
/// keyword, while its idle wording does not, so an unrecognized
/// notification is far more likely to be the idle nudge than a real
/// prompt — and a real prompt still surfaces via the `PermissionRequest`
/// hook and the PTY detector fallback.
fn notification_state(notification: Option<&str>) -> AgentState {
    match notification {
        Some(n) if blocks_on_user(n) => AgentState::InputNeeded,
        _ => AgentState::Idle,
    }
}

fn blocks_on_user(notification: &str) -> bool {
    let n = notification.to_ascii_lowercase();
    n.contains("permission") || n.contains("elicit")
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
        assert_eq!(hook_to_state(&ev), None);
    }

    #[test]
    fn parse_rejects_non_json() {
        assert!(parse_claude_hook("not json at all").is_none());
        assert!(parse_claude_hook("").is_none());
    }

    #[test]
    fn tool_and_compaction_and_subagent_are_working() {
        for name in [
            "PreToolUse",
            "PostToolUse",
            "PreCompact",
            "PostCompact",
            "SubagentStart",
            "SubagentStop",
        ] {
            let ev = parse(&format!(r#"{{"hook_event_name":"{name}"}}"#));
            assert_eq!(
                hook_to_state(&ev),
                Some(AgentState::Working),
                "{name} should be Working",
            );
        }
    }

    #[test]
    fn stop_and_session_lifecycle_are_idle() {
        for name in ["Stop", "SessionStart", "SessionEnd"] {
            let ev = parse(&format!(r#"{{"hook_event_name":"{name}"}}"#));
            assert_eq!(
                hook_to_state(&ev),
                Some(AgentState::Idle),
                "{name} should be Idle",
            );
        }
    }

    #[test]
    fn permission_request_is_input_needed() {
        let ev = parse(r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash"}"#);
        assert_eq!(hook_to_state(&ev), Some(AgentState::InputNeeded));
    }

    #[test]
    fn notification_permission_prompt_is_input_needed() {
        let ev =
            parse(r#"{"hook_event_name":"Notification","notification_type":"permission_prompt"}"#);
        assert_eq!(hook_to_state(&ev), Some(AgentState::InputNeeded));
    }

    #[test]
    fn notification_permission_message_is_input_needed() {
        // The free-text wording Claude actually sends when blocking on a
        // tool approval.
        let ev = parse(
            r#"{"hook_event_name":"Notification","message":"Claude needs your permission to use Bash"}"#,
        );
        assert_eq!(hook_to_state(&ev), Some(AgentState::InputNeeded));
    }

    #[test]
    fn notification_elicitation_is_input_needed() {
        let ev =
            parse(r#"{"hook_event_name":"Notification","notification_type":"elicitation_dialog"}"#);
        assert_eq!(hook_to_state(&ev), Some(AgentState::InputNeeded));
    }

    #[test]
    fn notification_idle_waiting_for_input_is_idle() {
        // Claude's real idle nudge after ~60s of inactivity — the
        // composer is ready, not blocked. This carries no "idle"
        // substring, which is exactly the misclassification #190 fixed.
        let ev = parse(
            r#"{"hook_event_name":"Notification","message":"Claude is waiting for your input"}"#,
        );
        assert_eq!(hook_to_state(&ev), Some(AgentState::Idle));
    }

    #[test]
    fn notification_without_type_defaults_to_idle() {
        // An unrecognized notification is far more likely the idle nudge
        // than a real prompt; real prompts say "permission"/"elicit".
        let ev = parse(r#"{"hook_event_name":"Notification"}"#);
        assert_eq!(hook_to_state(&ev), Some(AgentState::Idle));
    }
}
