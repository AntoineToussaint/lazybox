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
pub mod pty;
pub mod state_machine;
pub mod update;

pub use agent::{
    Agent, AgentAuthCommands, AgentObservation, AuthFailure, CredentialIsolation, LlmProvider,
    Registry, SpawnCtx, StructuredAgentProtocol, UnattendedPromptKind, UnattendedPromptNudge,
};
pub use lazybox_ipc::AgentState;
pub use pty::{
    EncodedPrompt, PromptFraming, PromptIntent, PromptShape, PtyProtocol, ReadinessPolicy,
    trim_leading_blank_lines,
};
pub use state_machine::{
    AgentStateMachine, HOOK_STALENESS, HookAuthority, Liveness, Outcome, PtyReading, Reading,
};
pub use update::UpdateChannel;

/// Look up a built-in agent by id, or fall back to a `GenericCli`
/// configured from YAML.
pub fn registry() -> agent::Registry {
    agent::Registry::default_builtins()
}
