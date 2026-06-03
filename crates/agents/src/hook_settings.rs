//! Generate the per-session Claude Code settings file that wires
//! lazybox's hook command into every lifecycle event we track.
//!
//! Claude's `--settings <file|json>` flag takes precedence over the
//! user's `~/.claude/settings.json` rather than merging with it — so if
//! we passed a bare `{ "hooks": … }` we'd silently disable any hooks the
//! user configured. [`build_settings`] therefore starts from the user's
//! settings (when provided) and *appends* lazybox's hook entry to each
//! event's list, preserving the user's own hooks.
//!
//! The settings JSON is built here as a pure function so it's testable
//! without touching the filesystem; the daemon (`lazybox_server`) reads
//! the user's settings, calls this, and writes the result.

use serde_json::{Map, Value, json};

/// Lifecycle events lazybox injects its hook command on. `PreToolUse` /
/// `PostToolUse` give the "working, running `<tool>`" signal;
/// `Notification` / `PermissionRequest` give "needs input"; `Stop` /
/// `SessionEnd` give "done / idle"; the rest fill in subagent and
/// compaction activity. Each maps cleanly through
/// [`crate::hook::hook_to_state`].
pub const HOOKED_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "PermissionRequest",
    "Stop",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
];

/// Build the merged Claude settings JSON. `user_settings` is the
/// parsed `~/.claude/settings.json` (or `None` if absent / unreadable);
/// `hook_command` is the shell command Claude runs on each event
/// (`lazybox hook-ingest --terminal <id>`).
pub fn build_settings(user_settings: Option<&Value>, hook_command: &str) -> Value {
    // Clone the user's settings so their non-hook keys (model, theme,
    // env, permissions, …) survive into the file we hand Claude.
    let mut root = match user_settings {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };

    let mut hooks = match root.get("hooks") {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };

    let entry = json!({ "hooks": [ { "type": "command", "command": hook_command } ] });

    for event in HOOKED_EVENTS {
        let list = hooks
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        // A user could have set this key to a non-array; replace it with
        // a fresh array rather than panic — Claude expects an array here.
        if !list.is_array() {
            *list = Value::Array(Vec::new());
        }
        if let Value::Array(arr) = list {
            arr.push(entry.clone());
        }
    }

    root.insert("hooks".to_string(), Value::Object(hooks));
    Value::Object(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: &str = "lazybox hook-ingest --terminal 7";

    fn hook_commands_for(settings: &Value, event: &str) -> Vec<String> {
        settings["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|group| group["hooks"].as_array().cloned().unwrap_or_default())
            .filter_map(|h| h["command"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn injects_command_on_every_hooked_event() {
        let settings = build_settings(None, CMD);
        for event in HOOKED_EVENTS {
            assert_eq!(
                hook_commands_for(&settings, event),
                vec![CMD.to_string()],
                "missing lazybox hook on {event}",
            );
        }
    }

    #[test]
    fn entry_uses_command_hook_shape() {
        let settings = build_settings(None, CMD);
        let group = &settings["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(group["type"], "command");
        assert_eq!(group["command"], CMD);
    }

    #[test]
    fn preserves_user_hooks_on_same_event() {
        let user = json!({
            "hooks": {
                "Stop": [ { "hooks": [ { "type": "command", "command": "user-stop.sh" } ] } ]
            }
        });
        let settings = build_settings(Some(&user), CMD);
        let cmds = hook_commands_for(&settings, "Stop");
        assert!(
            cmds.contains(&"user-stop.sh".to_string()),
            "user hook dropped"
        );
        assert!(cmds.contains(&CMD.to_string()), "lazybox hook missing");
    }

    #[test]
    fn preserves_unrelated_user_settings() {
        let user = json!({ "model": "opus", "env": { "FOO": "bar" } });
        let settings = build_settings(Some(&user), CMD);
        assert_eq!(settings["model"], "opus");
        assert_eq!(settings["env"]["FOO"], "bar");
        // And still wires the hooks in.
        assert_eq!(
            hook_commands_for(&settings, "PreToolUse"),
            vec![CMD.to_string()]
        );
    }

    #[test]
    fn tolerates_malformed_user_hooks_key() {
        // A user whose `hooks.Stop` is not an array shouldn't crash us;
        // we replace it with a valid array carrying lazybox's hook.
        let user = json!({ "hooks": { "Stop": "oops-a-string" } });
        let settings = build_settings(Some(&user), CMD);
        assert_eq!(hook_commands_for(&settings, "Stop"), vec![CMD.to_string()]);
    }
}
