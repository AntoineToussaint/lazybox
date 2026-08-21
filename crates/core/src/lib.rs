//! # lazybox-core
//!
//! Generic domain types for lazybox. Source-agnostic: nothing here knows about
//! GitHub, Linear, or any specific provider.

pub mod agent;
pub mod autofix;
pub mod branch_template;
pub mod config;
pub mod conventions;
pub mod error_class;
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
pub mod stack;
mod task;
pub mod time;
mod workspace;

pub use agent::{AgentConfig, AgentModels, ModelTier};
pub use autofix::{
    AutoFixKind, AutoFixSettings, auto_fix_candidate, auto_fix_enabled_and_permitted,
    evaluate_auto_fix, is_auto_fix_opted_out, resolve_auto_fix,
};
pub use config::{
    KV_KEY_ARCHIVED, KV_KEY_LAYOUT, KV_KEY_SETUP, KV_KEY_THEME, LinearScope, PaneLayout,
    PersistedSetup, ProviderConfig,
};
pub use conventions::{CommitStyle, Conventions};
pub use error_class::{ErrorClass, HttpErrorSignals, classify, classify_message, classify_status};
pub use issue_links::{IssueLink, extract as extract_issue_links};
pub use policy::{
    ApprovalPolicy, AutomationPolicies, MergeOnGreenPolicy, NON_AUTHOR_BLOCK, PolicyArm,
    approval_policy_blocks, author_gate_blocks, auto_fix_permitted, auto_merge_block_reason,
    merge_block_reason, should_auto_merge, toggled_arm,
};
pub use priority::{PriorityTier, resolve_priority_tier};
pub use project::{Project, ProjectKey, github_owner_repo_from_url};
pub use provider::{
    DEFAULT_MAX_PAGES, FetchCoverage, FetchOutcome, FetchPage, FetchPageInfo, GITHUB_SOURCE,
    LINEAR_SOURCE, PaginationOutcome, PaginationStop, ProviderError, TaskProvider, paginate,
};
pub use scope::{MockScopeSource, Scope, ScopeKind, ScopeSource};
pub use session_key::SessionKey;
pub use stack::{StackPosition, detect_stacks};
pub use task::*;
pub use workspace::{
    CleanupPrompt, HopperMeta, MAX_ACTIVITY_ITEMS, SENT_SNIPPETS_MAX, Session as WorkspaceSession,
    SessionId, SessionKind, SessionLayout, SessionRunState, TileDirection, TileTree,
    WORKING_CLAIM_HEARTBEAT_SECS, WORKING_CLAIM_LABEL_PREFIX, WORKING_CLAIM_TTL_SECS,
    WORKING_LABEL_NAME, WORKSPACE_SCHEMA_VERSION, Workspace, WorkspaceDecodeError, WorkspaceKey,
    project_key_for_task, workspace_key_for, workspace_project_key,
};
