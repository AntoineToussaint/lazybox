//! Pure IPC dispatch planning.

use lazybox_core::SessionKey;
use lazybox_ipc::{Command, SpawnFallback, TerminalId, TerminalKind};

/// Read-only view of the running agent terminals needed to plan command
/// dispatch.
pub trait AgentTerminalView {
    fn find_agent_terminal(&self, session_key: &SessionKey, agent_id: &str) -> Option<TerminalId>;
}

/// Rewrite contextual agent spawns into prompt injection when the matching
/// conversation is already running.
pub fn plan_dispatch(cmds: Vec<Command>, sidebar_view: &impl AgentTerminalView) -> Vec<Command> {
    cmds.into_iter()
        .map(|cmd| {
            let terminal_id = match &cmd {
                Command::Spawn {
                    session_key,
                    kind: TerminalKind::Agent(agent_id),
                    initial_prompt: Some(_),
                    ..
                } => sidebar_view.find_agent_terminal(session_key, agent_id),
                _ => None,
            };
            match terminal_id {
                Some(terminal_id) => plan_spawn_for_terminal(cmd, terminal_id),
                None => cmd,
            }
        })
        .collect()
}

/// Rewrite one contextual spawn to a specific running terminal.
pub fn plan_spawn_for_terminal(cmd: Command, terminal_id: TerminalId) -> Command {
    match cmd {
        Command::Spawn {
            model_alias,
            session_key,
            session_id,
            client_request_id,
            kind: TerminalKind::Agent(agent_id),
            cwd,
            initial_prompt: Some(prompt),
            // Contextual work prompts are never snippet-seeded, so the
            // spawn→inject rewrite has no snippet identity to preserve
            // (#1215's broadcast seed path never routes through here —
            // a live terminal gets DeliverSnippet instead).
            initial_snippet: _,
            on_main: _,
            access,
        } if access == lazybox_ipc::AgentRunAccess::Default => Command::InjectPrompt {
            terminal_id,
            prompt,
            fallback_spawn: Some(SpawnFallback {
                model_alias,
                session_key,
                session_id,
                client_request_id,
                kind: TerminalKind::Agent(agent_id),
                cwd,
                access,
            }),
            submit: true,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_core::SessionId;
    use std::collections::HashMap;

    struct View(HashMap<(SessionKey, String), TerminalId>);

    impl AgentTerminalView for View {
        fn find_agent_terminal(
            &self,
            session_key: &SessionKey,
            agent_id: &str,
        ) -> Option<TerminalId> {
            self.0
                .get(&(session_key.clone(), agent_id.to_string()))
                .copied()
        }
    }

    fn spawn(prompt: Option<&str>) -> Command {
        Command::Spawn {
            session_key: SessionKey::new("owner/repo/1"),
            session_id: Some(SessionId::new()),
            client_request_id: None,
            kind: TerminalKind::Agent("codex".to_string()),
            cwd: Some("/tmp/worktree".to_string()),
            initial_prompt: prompt.map(str::to_string),
            initial_snippet: None,
            on_main: false,
            model_alias: Some("L".to_string()),
            access: lazybox_ipc::AgentRunAccess::Default,
        }
    }

    #[test]
    fn plan_dispatch_rewrites_only_prompted_matching_agent_spawns() {
        let session_key = SessionKey::new("owner/repo/1");
        let terminal_id = TerminalId(7);
        let view = View(HashMap::from([(
            (session_key.clone(), "codex".to_string()),
            terminal_id,
        )]));

        let planned = plan_dispatch(vec![spawn(Some("fix CI")), spawn(None)], &view);

        assert!(matches!(
            &planned[0],
            Command::InjectPrompt {
                terminal_id: id,
                prompt,
                fallback_spawn: Some(SpawnFallback {
                    session_key: fallback_key,
                    model_alias: Some(alias),
                    ..
                }),
                submit: true,
            } if *id == terminal_id
                && prompt == "fix CI"
                && fallback_key == &session_key
                && alias == "L"
        ));
        assert!(matches!(
            &planned[1],
            Command::Spawn {
                initial_prompt: None,
                ..
            }
        ));
    }

    #[test]
    fn plan_spawn_for_terminal_preserves_the_spawn_fallback() {
        let planned = plan_spawn_for_terminal(spawn(Some("continue")), TerminalId(9));

        assert!(matches!(
            planned,
            Command::InjectPrompt {
                terminal_id: TerminalId(9),
                prompt,
                fallback_spawn: Some(SpawnFallback {
                    session_id: Some(_),
                    kind: TerminalKind::Agent(agent),
                    cwd: Some(cwd),
                    ..
                }),
                ..
            } if prompt == "continue" && agent == "codex" && cwd == "/tmp/worktree"
        ));
    }

    #[test]
    fn plan_spawn_for_terminal_does_not_rewrite_read_only_agents() {
        let mut command = spawn(Some("review"));
        let Command::Spawn { access, .. } = &mut command else {
            unreachable!("spawn helper returns Spawn");
        };
        *access = lazybox_ipc::AgentRunAccess::ReadOnly;

        assert!(matches!(
            plan_spawn_for_terminal(command, TerminalId(9)),
            Command::Spawn {
                access: lazybox_ipc::AgentRunAccess::ReadOnly,
                ..
            }
        ));
    }
}
