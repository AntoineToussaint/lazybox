//! Setup configuration types.
//!
//! These live in lazybox_core (rather than lazybox_tui) so the daemon
//! can read them out of the store and the providers can filter by
//! them, while the TUI's setup screen is the only thing that writes
//! them. Keys are opaque strings — provider crates know how to
//! interpret them; lazybox_core stays source-agnostic.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Per-provider config: a flat set of opaque keys describing what
/// lazybox should poll for from this provider.
///
/// ## GitHub key schema
///
/// **Per-type roles** — issues have no concept of reviewer, so the
/// keys are scoped per item type. Setting any `pr.*` key implies
/// "fetch pull requests"; setting any `issue.*` key implies "fetch
/// issues":
///
/// - `pr.author`, `pr.reviewer`, `pr.assignee`, `pr.mentioned`
/// - `issue.author`, `issue.assignee`, `issue.mentioned`
///
/// **Legacy keys** (still readable for backward compat, migrated on
/// load by `migrate_legacy_keys()`):
///
/// - `role.author`, `role.reviewer`, `role.assignee`, `role.mentioned`
/// - `type.prs`, `type.issues`
///
/// Old saved configs deserialize fine; the migration projects them
/// onto the new `pr.*` / `issue.*` keys (skipping `reviewer` for
/// issues, which makes no sense there).
///
/// ## Linear key schema
///
/// Linear has only roles, no PR/Issue split:
/// `role.author`, `role.assignee`, `role.subscriber`, `role.mentioned`.
///
/// Unknown keys are ignored at apply time so the option schema can
/// grow without invalidating saved configs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    pub enabled_keys: BTreeSet<String>,
}

impl ProviderConfig {
    pub fn has(&self, key: &str) -> bool {
        self.enabled_keys.contains(key)
    }

    pub fn toggle(&mut self, key: &str) {
        if !self.enabled_keys.remove(key) {
            self.enabled_keys.insert(key.to_string());
        }
    }

    // ── GitHub: per-type role checks ─────────────────────────────────

    /// Does the user want this role for **PRs**?
    pub fn allows_pr_role(&self, role: crate::task::TaskRole) -> bool {
        self.has(pr_key(role))
    }

    /// Does the user want this role for **issues**? Always returns
    /// `false` for `Reviewer` (issues have no reviewers).
    pub fn allows_issue_role(&self, role: crate::task::TaskRole) -> bool {
        match issue_key(role) {
            Some(key) => self.has(key),
            None => false,
        }
    }

    /// Whether to fetch PRs at all — true if any `pr.*` key is set.
    pub fn pr_enabled(&self) -> bool {
        self.enabled_keys.iter().any(|k| k.starts_with("pr."))
    }

    /// Whether to fetch issues at all — true if any `issue.*` key is set.
    pub fn issue_enabled(&self) -> bool {
        self.enabled_keys.iter().any(|k| k.starts_with("issue."))
    }

    // ── Linear: flat role check ──────────────────────────────────────

    /// Linear-style role check (no per-type split).
    pub fn allows_linear_role(&self, role: crate::task::TaskRole) -> bool {
        let key = match role {
            crate::task::TaskRole::Author => "role.author",
            crate::task::TaskRole::Reviewer => return false, // Linear has no reviewer
            crate::task::TaskRole::Assignee => "role.assignee",
            crate::task::TaskRole::Mentioned => "role.mentioned",
        };
        self.has(key)
    }

    // ── Migration ────────────────────────────────────────────────────

    /// Project legacy `role.*` + `type.*` keys onto the new per-type
    /// schema. Idempotent — running it twice is a no-op. Called once
    /// at load time by `setup_flow::load_persisted` so stored configs
    /// from older versions of lazybox keep working.
    pub fn migrate_legacy_keys(&mut self) {
        let prs_enabled = self.enabled_keys.contains("type.prs");
        let issues_enabled = self.enabled_keys.contains("type.issues");
        // If neither type was explicitly checked, the old default was
        // "both" — preserve that on migration.
        let default_both = !prs_enabled && !issues_enabled;
        let want_pr = prs_enabled || default_both;
        let want_issue = issues_enabled || default_both;

        let roles = ["author", "reviewer", "assignee", "mentioned"];
        let mut to_add: Vec<String> = Vec::new();
        for r in &roles {
            if self.enabled_keys.contains(&format!("role.{r}")) {
                if want_pr {
                    to_add.push(format!("pr.{r}"));
                }
                if want_issue && *r != "reviewer" {
                    to_add.push(format!("issue.{r}"));
                }
            }
        }
        if to_add.is_empty()
            && !self
                .enabled_keys
                .iter()
                .any(|k| k.starts_with("role.") || k == "type.prs" || k == "type.issues")
        {
            return; // No legacy keys to migrate.
        }
        for k in to_add {
            self.enabled_keys.insert(k);
        }
        // Drop legacy keys.
        self.enabled_keys
            .retain(|k| !k.starts_with("role.") && k != "type.prs" && k != "type.issues");
    }

    /// Provider-specific defaults — what most users want without
    /// thinking. Lives here so daemon code can fall back to a
    /// reasonable filter when there's no saved config.
    ///
    /// GitHub default is **PR-only** because issues are typically
    /// tracked in a separate issue tracker (Linear, Jira, …) rather
    /// than on GitHub for users sophisticated enough to use a tool
    /// like lazybox. Users who do want issues opt in via the filter
    /// step.
    pub fn default_for(provider_id: &str) -> Self {
        let mut keys = BTreeSet::new();
        match provider_id {
            "github" => {
                // PR roles only by default. `mentioned` is off — it's
                // noisy on busy repos. Issues are off entirely;
                // users tick them in the filter step if they care.
                keys.insert("pr.author".into());
                keys.insert("pr.reviewer".into());
                keys.insert("pr.assignee".into());
            }
            "linear" => {
                // Assigned-to-me + created-by-me, matching the default
                // query scope ([`LinearScope::default_scopes`]). Subscriber
                // / mentioned are off — the noisy signals a user opts into.
                keys.insert("role.assignee".into());
                keys.insert("role.author".into());
            }
            _ => {}
        }
        Self { enabled_keys: keys }
    }
}

/// Map a `TaskRole` to its `pr.*` key.
fn pr_key(role: crate::task::TaskRole) -> &'static str {
    match role {
        crate::task::TaskRole::Author => "pr.author",
        crate::task::TaskRole::Reviewer => "pr.reviewer",
        crate::task::TaskRole::Assignee => "pr.assignee",
        crate::task::TaskRole::Mentioned => "pr.mentioned",
    }
}

/// Map a `TaskRole` to its `issue.*` key. `Reviewer` returns `None`
/// because issues have no reviewers.
fn issue_key(role: crate::task::TaskRole) -> Option<&'static str> {
    match role {
        crate::task::TaskRole::Author => Some("issue.author"),
        crate::task::TaskRole::Reviewer => None,
        crate::task::TaskRole::Assignee => Some("issue.assignee"),
        crate::task::TaskRole::Mentioned => Some("issue.mentioned"),
    }
}

/// Which Linear issues the poller asks for. Each variant maps to one
/// `or` clause in the issues GraphQL filter (assignee / creator /
/// subscriber `isMe`). `Subscribed` is the noisy one — Linear
/// auto-subscribes you to a whole team's issues, ones you merely
/// opened, and more — so it's deliberately *not* in [`Self::default_scopes`].
/// Surfaced to users as `providers.linear.scope: [assigned, created]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinearScope {
    /// Issues assigned to me.
    Assigned,
    /// Issues I created.
    Created,
    /// Issues I subscribe to. Off by default — Linear auto-subscribes
    /// aggressively (team defaults, opening an issue), so this floods
    /// the inbox with issues you have no real relationship to.
    Subscribed,
}

impl LinearScope {
    /// The default query scope: assigned-to-me OR created-by-me, with
    /// no subscriber clause. This is the flood-free baseline; a user
    /// opts into `Subscribed` explicitly.
    pub fn default_scopes() -> Vec<Self> {
        vec![Self::Assigned, Self::Created]
    }
}

/// Persisted setup state — read at daemon startup, written by the
/// TUI's setup flow on confirm. Stored in the store's kv table under
/// `KV_KEY_SETUP`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PersistedSetup {
    pub enabled_providers: BTreeSet<String>,
    pub enabled_agents: BTreeSet<String>,
    pub provider_filters: BTreeMap<String, ProviderConfig>,
    /// Per-provider scope selection (org/repo for GH, project for
    /// Linear, …). Empty for a provider means "subscribe to
    /// everything the token can see" — same default as before this
    /// field existed, so upgrading users don't need to re-pick.
    /// Non-empty narrows polling to exactly the listed scope ids.
    #[serde(default)]
    pub selected_scopes: BTreeMap<String, BTreeSet<String>>,
}

impl PersistedSetup {
    pub fn provider_config(&self, id: &str) -> ProviderConfig {
        self.provider_filters
            .get(id)
            .cloned()
            .unwrap_or_else(|| ProviderConfig::default_for(id))
    }

    /// Run legacy-key migration on every stored provider config.
    /// Idempotent; called by `setup_flow::load_persisted` so old
    /// saved configs project onto the per-type schema before the
    /// daemon starts polling.
    pub fn migrate_legacy_keys(&mut self) {
        for cfg in self.provider_filters.values_mut() {
            cfg.migrate_legacy_keys();
        }
    }

    /// Whether `scope_id` was selected for the given provider. An
    /// empty selection (no scopes ever picked) is treated as "all
    /// scopes allowed" so existing setups keep working — the user
    /// only narrows after explicitly visiting the picker.
    pub fn allows_scope(&self, provider_id: &str, scope_id: &str) -> bool {
        match self.selected_scopes.get(provider_id) {
            Some(set) if !set.is_empty() => set.contains(scope_id),
            _ => true,
        }
    }
}

/// Stable kv key for the persisted setup state. Externalized so the
/// daemon and TUI agree without spelling the same string twice.
pub const KV_KEY_SETUP: &str = "setup_v1";
/// Active theme name (matches `Theme.name`). Cycled with the `T`
/// global keybind; persisted so the user's choice survives restart.
pub const KV_KEY_THEME: &str = "theme_v1";

/// Stable kv key for the persisted pane layout (sidebar width +
/// horizontal split percentage).
/// Bumped to `_v2` when the sidebar split changed from cells to a
/// percentage. v1 values stored a 70-cell sidebar that, re-read as a
/// percent, would render as 70% of the screen — basically swallowing
/// the right pane. The version bump means the migration is implicit:
/// old saves are ignored, new writes go to the new key.
pub const KV_KEY_LAYOUT: &str = "layout_v2";

/// Set of workspace keys the user has explicitly archived
/// (`x x`). Stored as a JSON-encoded `Vec<String>` so re-poll
/// upserts know to skip them — without this the next poll's
/// `WorkspaceUpserted` would just re-create the row the user
/// just dismissed. Persistence is per-machine, not synced.
pub const KV_KEY_ARCHIVED: &str = "archived_workspaces_v1";

/// Set of workspace keys whose backing session must be killed —
/// never reattached — on the next recovery pass. Written on *every*
/// workspace removal (all `WorkspaceRemovalReason`s), unlike
/// [`KV_KEY_ARCHIVED`], which additionally suppresses re-polling and so
/// is deliberately skipped by non-archiving removals (`ClosedAuto`,
/// `Rescope`). Splitting the two lets those removals keep their
/// resurrection backstop — a survivor tmux session that the delete-time
/// sweep failed to reach is still killed on restart — without also
/// suppressing a reopened issue from resurfacing. Stored as a
/// JSON-encoded `Vec<String>`; per-machine, not synced.
pub const KV_KEY_SESSION_TOMBSTONES: &str = "session_tombstones_v1";

/// Pane layout knobs. Two splitters today: the sidebar's right edge
/// (left/right split, **as a percentage of the total width**) and the
/// right column's horizontal split (top/bottom, also a percentage).
/// Percent — not cells — so the layout adapts to terminal size and
/// stays consistent on a 4K monitor vs a laptop screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLayout {
    /// Sidebar width as a percentage of total width. 1..=99.
    pub sidebar_width: u16,
    /// Top half (right pane / activity feed) as a percentage of the
    /// right column's height; the bottom half (terminal stack) takes
    /// the remainder.
    pub right_top_pct: u16,
}

impl PaneLayout {
    /// Reasonable defaults: 40% sidebar so PR titles + status + time
    /// fit on a typical laptop screen without aggressive truncation;
    /// 25% top pane in the right column.
    pub const DEFAULT: PaneLayout = PaneLayout {
        sidebar_width: 40,
        right_top_pct: 25,
    };

    pub fn clamp(self) -> Self {
        Self {
            // Sidebar percent: 15-75% range covers everything from
            // "I want lots of terminal" to "I want lots of inbox."
            sidebar_width: self.sidebar_width.clamp(15, 75),
            right_top_pct: self.right_top_pct.clamp(0, 100),
        }
    }

    pub fn nudge(self, sidebar_delta: i16, right_top_delta: i16) -> Self {
        let sidebar = (self.sidebar_width as i16 + sidebar_delta).max(0) as u16;
        let right = (self.right_top_pct as i16 + right_top_delta).max(0) as u16;
        Self {
            sidebar_width: sidebar,
            right_top_pct: right,
        }
        .clamp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskRole;

    fn cfg_with(keys: &[&str]) -> ProviderConfig {
        let mut c = ProviderConfig::default();
        for k in keys {
            c.enabled_keys.insert((*k).into());
        }
        c
    }

    #[test]
    fn pr_role_check_uses_pr_key() {
        let c = cfg_with(&["pr.author"]);
        assert!(c.allows_pr_role(TaskRole::Author));
        assert!(!c.allows_pr_role(TaskRole::Reviewer));
    }

    #[test]
    fn issue_role_check_skips_reviewer() {
        let c = cfg_with(&["issue.author", "issue.assignee", "pr.reviewer"]);
        assert!(c.allows_issue_role(TaskRole::Author));
        assert!(c.allows_issue_role(TaskRole::Assignee));
        assert!(
            !c.allows_issue_role(TaskRole::Reviewer),
            "reviewer never applies to issues even if pr.reviewer is on"
        );
    }

    #[test]
    fn pr_enabled_iff_any_pr_key() {
        assert!(!ProviderConfig::default().pr_enabled());
        assert!(cfg_with(&["pr.author"]).pr_enabled());
        assert!(!cfg_with(&["issue.author"]).pr_enabled());
    }

    #[test]
    fn migration_projects_legacy_role_and_type_onto_new_schema() {
        // role.author + role.reviewer + type.prs → pr.author + pr.reviewer
        let mut c = cfg_with(&["role.author", "role.reviewer", "type.prs"]);
        c.migrate_legacy_keys();
        assert!(c.has("pr.author"));
        assert!(c.has("pr.reviewer"));
        assert!(!c.has("issue.author"), "type.issues was not set");
        assert!(!c.has("role.author"), "legacy keys are dropped");
        assert!(!c.has("type.prs"));
    }

    #[test]
    fn migration_legacy_no_type_means_both() {
        // role.author with NO type.* → defaulted to both PR + issue
        // (matches old behavior where empty type set meant "all").
        let mut c = cfg_with(&["role.author"]);
        c.migrate_legacy_keys();
        assert!(c.has("pr.author"));
        assert!(c.has("issue.author"));
    }

    #[test]
    fn migration_drops_reviewer_for_issues() {
        let mut c = cfg_with(&["role.reviewer", "type.prs", "type.issues"]);
        c.migrate_legacy_keys();
        assert!(c.has("pr.reviewer"));
        assert!(!c.has("issue.reviewer"), "issues have no reviewer");
    }

    #[test]
    fn migration_idempotent() {
        let mut c = cfg_with(&["role.author", "type.prs"]);
        c.migrate_legacy_keys();
        let after_first = c.clone();
        c.migrate_legacy_keys();
        assert_eq!(c, after_first);
    }

    #[test]
    fn migration_noop_on_already_new_schema() {
        let mut c = cfg_with(&["pr.author", "issue.author"]);
        let before = c.clone();
        c.migrate_legacy_keys();
        assert_eq!(c, before);
    }

    #[test]
    fn default_github_is_pr_only() {
        let c = ProviderConfig::default_for("github");
        assert!(c.has("pr.author"));
        assert!(c.has("pr.reviewer"));
        assert!(c.has("pr.assignee"));
        assert!(!c.has("pr.mentioned"));
        // Issues default OFF — users opt in via the filter step.
        assert!(!c.issue_enabled());
        assert!(c.pr_enabled());
    }

    #[test]
    fn default_linear_uses_flat_role_keys() {
        let c = ProviderConfig::default_for("linear");
        assert!(c.allows_linear_role(TaskRole::Assignee));
        assert!(
            c.allows_linear_role(TaskRole::Author),
            "created-by-me on by default"
        );
        // Subscribed issues map to `Mentioned` — off by default so the
        // subscriber flood (#862) stays out even if it were fetched.
        assert!(!c.allows_linear_role(TaskRole::Mentioned));
        assert!(!c.allows_linear_role(TaskRole::Reviewer));
    }

    #[test]
    fn linear_default_scope_is_assigned_and_created_only() {
        // The subscriber clause is the #862 flood — it must not be in
        // the default query scope.
        let scopes = LinearScope::default_scopes();
        assert!(scopes.contains(&LinearScope::Assigned));
        assert!(scopes.contains(&LinearScope::Created));
        assert!(!scopes.contains(&LinearScope::Subscribed));
    }

    #[test]
    fn linear_scope_serde_is_lowercase() {
        let json = r#"["assigned", "created", "subscribed"]"#;
        let parsed: Vec<LinearScope> = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            vec![
                LinearScope::Assigned,
                LinearScope::Created,
                LinearScope::Subscribed
            ]
        );
        assert_eq!(
            serde_json::to_string(&LinearScope::Subscribed).unwrap(),
            r#""subscribed""#
        );
    }
}
