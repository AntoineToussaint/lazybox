//! Agent abstractions — Claude Code, Codex, Cursor, or any CLI.
//!
//! An `Agent` is a recipe for (1) launching an AI coding tool inside a
//! worktree, (2) recognizing when it's working / idle / asking for
//! input, and (3) injecting prompts. Adding a new agent is one file.

pub mod agent;
pub(crate) mod claude_env;
pub mod detect;
pub mod hook;
pub mod hook_settings;
pub mod state_machine;

pub use agent::{Agent, LlmProvider, PromptShape, Registry, SpawnCtx, StructuredAgentProtocol};
pub use lazybox_ipc::AgentState;
pub use state_machine::{AgentStateMachine, Outcome, Reading};

/// Look up a built-in agent by id, or fall back to a `GenericCli`
/// configured from YAML.
pub fn registry() -> agent::Registry {
    agent::Registry::default_builtins()
}
