//! # pilot-core
//!
//! Generic domain types for pilot. Source-agnostic: nothing here knows about
//! GitHub, Linear, or any specific provider.

pub mod agent;
pub mod config;
pub mod issue_links;
pub mod paths;
pub mod project;
pub mod provider;
pub mod scope;
mod session_key;
pub mod slug;
mod task;
pub mod time;
mod workspace;

pub use agent::AgentConfig;
pub use config::{
    KV_KEY_LAYOUT, KV_KEY_SETUP, KV_KEY_THEME, PaneLayout, PersistedSetup, ProviderConfig,
};
pub use issue_links::{IssueLink, extract as extract_issue_links};
pub use project::{Project, ProjectKey};
pub use provider::{ProviderError, TaskProvider};
pub use scope::{MockScopeSource, Scope, ScopeKind, ScopeSource};
pub use session_key::SessionKey;
pub use task::*;
pub use workspace::{
    Session as WorkspaceSession, SessionId, SessionKind, SessionLayout, SessionRunState,
    TileDirection, TileTree, Workspace, WorkspaceKey, project_key_for_task, workspace_key_for,
    workspace_project_key,
};
