//! # lazybox-core
//!
//! Generic domain types for lazybox. Source-agnostic: nothing here knows about
//! GitHub, Linear, or any specific provider.

pub mod agent;
pub mod autofix;
pub mod config;
pub mod issue_links;
pub mod paths;
pub mod policy;
pub mod priority;
pub mod project;
pub mod prompts;
pub mod provider;
pub mod scope;
mod session_key;
pub mod slug;
mod task;
pub mod time;
mod workspace;

pub use agent::{AgentConfig, AgentModels, ModelTier};
pub use autofix::{
    AutoFixKind, AutoFixSettings, auto_fix_candidate, evaluate_auto_fix, is_auto_fix_opted_out,
};
pub use config::{
    KV_KEY_ARCHIVED, KV_KEY_LAYOUT, KV_KEY_SETUP, KV_KEY_THEME, PaneLayout, PersistedSetup,
    ProviderConfig,
};
pub use issue_links::{IssueLink, extract as extract_issue_links};
pub use policy::{AutomationPolicies, PolicyArm, auto_fix_permitted, toggled_arm};
pub use priority::{PriorityTier, resolve_priority_tier};
pub use project::{Project, ProjectKey, github_owner_repo_from_url};
pub use provider::{ProviderError, TaskProvider};
pub use scope::{MockScopeSource, Scope, ScopeKind, ScopeSource};
pub use session_key::SessionKey;
pub use task::*;
pub use workspace::{
    MAX_ACTIVITY_ITEMS, SENT_SNIPPETS_MAX, Session as WorkspaceSession, SessionId, SessionKind,
    SessionLayout, SessionRunState, TileDirection, TileTree, Workspace, WorkspaceKey,
    project_key_for_task, workspace_key_for, workspace_project_key,
};
