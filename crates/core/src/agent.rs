//! Agent abstraction. Any coding agent (Claude Code, Aider, Cursor, etc.)
//! implements this trait to integrate with lazybox.

use serde::{Deserialize, Serialize};

/// Configuration for a coding agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Display name (e.g., "Claude Code").
    pub name: String,
    /// Command to spawn (e.g., "claude").
    pub command: String,
    /// Additional args for first launch.
    #[serde(default)]
    pub args: Vec<String>,
    /// Args to resume a previous session (e.g., ["--continue"]).
    #[serde(default)]
    pub resume_args: Vec<String>,
    /// Patterns in terminal output that indicate the agent is asking a question.
    /// Used for notification detection.
    #[serde(default = "default_asking_patterns")]
    pub asking_patterns: Vec<String>,
}

fn default_asking_patterns() -> Vec<String> {
    vec![
        "(y/n)".into(),
        "(yes/no)".into(),
        "allow ".into(),
        "do you want".into(),
        "would you like".into(),
        "press enter".into(),
    ]
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: "Claude Code".into(),
            command: "claude".into(),
            args: vec![],
            resume_args: vec!["--continue".into()],
            asking_patterns: default_asking_patterns(),
        }
    }
}

impl AgentConfig {
    /// Build the command + args to spawn the agent.
    pub fn spawn_command(&self, resume: bool) -> Vec<String> {
        let mut cmd = vec![self.command.clone()];
        if resume && !self.resume_args.is_empty() {
            cmd.extend(self.resume_args.iter().cloned());
        } else {
            cmd.extend(self.args.iter().cloned());
        }
        cmd
    }
}
