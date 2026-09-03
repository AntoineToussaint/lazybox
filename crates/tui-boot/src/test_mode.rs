//! `lazybox --test` — boots the TUI against a throwaway git repo with a
//! single seeded session, no setup screen, no provider polling. The
//! goal is a one-command path for trying the side panel + terminal
//! pane end-to-end without needing GitHub credentials, a real
//! workspace, or the polling machinery.
//!
//! ## What gets created
//!
//! 1. A fresh temp directory under the OS temp path (held by a
//!    `TempDir` for the process lifetime so it auto-deletes on drop).
//! 2. `git init` inside it, so `b` (shell spawn) lands in something
//!    that looks like a real repo.
//! 3. An in-memory `MemoryStore` is given to the daemon. The
//!    SetupConfig kv row is pre-populated so the setup screen is
//!    skipped on first `Subscribe`. One synthetic `Session` row is
//!    saved so the sidebar boots with a workspace selected.

use chrono::Utc;
use lazybox_core::{
    CiStatus, KV_KEY_SETUP, PersistedSetup, ProviderConfig, ReviewStatus, SessionKind, Task,
    TaskId, TaskRole, TaskState, Workspace, WorkspaceSession,
};
use lazybox_store::{MemoryStore, Store, WorkspaceRecord};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

/// Built artifacts of the test fixture.
pub struct TestFixture {
    /// Owns the temp directory — drop = delete.
    pub repo: TempDir,
    /// In-memory store the daemon should use.
    pub store: Arc<dyn Store>,
    /// Deterministic client-side catalog available in the throwaway TUI.
    pub snippets: lazybox_config::Snippets,
}

impl TestFixture {
    /// Default: temp dir + `git init`, in-memory store with the
    /// setup-skip kv row populated, **no seeded sessions**. The
    /// sidebar boots empty and the user creates a workspace by hand.
    /// Only the tests below use the unseeded variant; the binary's
    /// `--test` path takes `new_with_seeded_session`.
    #[allow(dead_code)]
    pub fn new() -> anyhow::Result<Self> {
        Self::new_with_options(false)
    }

    /// Variant: also seed a synthetic Session pointing at the
    /// tempdir. Useful for screenshots / golden snapshots; the
    /// default path leaves the sidebar empty per UX direction.
    pub fn new_with_seeded_session() -> anyhow::Result<Self> {
        Self::new_with_options(true)
    }

    fn new_with_options(seed_session: bool) -> anyhow::Result<Self> {
        let repo = tempfile::Builder::new()
            .prefix("lazybox-test-")
            .tempdir()
            .map_err(|e| anyhow::anyhow!("create tempdir: {e}"))?;
        run_git_init(repo.path())?;

        let store = Arc::new(MemoryStore::new()) as Arc<dyn Store>;
        seed_skip_setup(&*store)?;
        if seed_session {
            seed_one_session(&*store, repo.path())?;
        }
        Ok(Self {
            repo,
            store,
            snippets: lazybox_config::Snippets::builtin(),
        })
    }
}

/// Drop a PersistedSetup with everything enabled into the kv table so
/// `setup_flow::run_with_persistence` finds it and skips the screen.
pub(crate) fn seed_skip_setup(store: &dyn Store) -> anyhow::Result<()> {
    let mut p = PersistedSetup::default();
    p.enabled_providers.insert("github".into());
    p.enabled_providers.insert("linear".into());
    p.enabled_agents.insert("claude".into());
    p.enabled_agents.insert("codex".into());
    p.enabled_agents.insert("cursor-agent".into());
    p.provider_filters
        .insert("github".into(), ProviderConfig::default_for("github"));
    p.provider_filters
        .insert("linear".into(), ProviderConfig::default_for("linear"));
    let json = serde_json::to_string(&p)?;
    store
        .set_kv(KV_KEY_SETUP, &json)
        .map_err(|e| anyhow::anyhow!("set_kv: {e}"))?;
    Ok(())
}

/// Save one synthetic Workspace whose worktree_path is the tempdir,
/// so the sidebar boots with something selectable and `s` spawns a
/// shell rooted at the test repo.
fn seed_one_session(store: &dyn Store, worktree: &Path) -> anyhow::Result<()> {
    // Use `source: "github"` + a `/pull/` URL so `Workspace::classify`
    // treats this as a real PR — otherwise the task lands in
    // `TaskSlot::Unknown`, the workspace ends up with no
    // `primary_task()`, and the right pane renders the empty-header
    // placeholder. We want `--test` to look like a real workspace.
    let task = Task {
        author: String::new(),
        id: TaskId {
            source: "github".into(),
            key: "test/repo#1".into(),
        },
        title: "Test workspace".into(),
        body: Some(
            "Fake workspace created by `lazybox --test`. The bottom \
             hint bar shows what's available right now: `s` shell, \
             `a c` claude, `a x` codex, `a u` cursor."
                .into(),
        ),
        state: TaskState::Open,
        role: TaskRole::Author,
        ci: CiStatus::Success,
        review: ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: "https://github.com/test/repo/pull/1".into(),
        repo: Some("test/repo".into()),
        branch: Some("main".into()),
        base_branch: Some("main".into()),
        updated_at: Utc::now(),
        created_at: None,
        closed_at: None,
        labels: vec![lazybox_core::Label::new("test")],
        reviewers: vec![],
        reviews: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: lazybox_core::Mergeable::Mergeable,
        is_behind_base: false,
        merge_blocked: false,
        approval_policy: Default::default(),
        node_id: None,
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
        changed_files: 0,
        kind: None,
        closes_issues: vec![],
        linked_tasks: vec![],
        parent: None,
        priority: None,
        state_label: None,
    };
    let mut workspace = Workspace::from_task(task, Utc::now());
    // Seed a Session so `--test` boots with a usable folder under
    // the workspace. A workspace with no sessions has no on-disk
    // presence; --test wants something the user can immediately
    // spawn a shell in.
    let now = Utc::now();
    workspace.add_session(WorkspaceSession::new(
        workspace.key.clone(),
        SessionKind::Shell,
        worktree.to_path_buf(),
        now,
    ));
    let json = serde_json::to_string(&workspace)?;
    store
        .save_workspace(&WorkspaceRecord {
            key: workspace.key.as_str().to_string(),
            created_at: workspace.created_at,
            workspace_json: Some(json),
        })
        .map_err(|e| anyhow::anyhow!("save_workspace: {e}"))?;
    Ok(())
}

pub(crate) fn run_git_init(path: &Path) -> anyhow::Result<()> {
    let run = |args: &[&str]| -> anyhow::Result<()> {
        let output = Command::new("git")
            .args([
                "-c",
                "user.email=test@lazybox",
                "-c",
                "user.name=lazybox test",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
            ])
            .args(args)
            .current_dir(path)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .map_err(|e| anyhow::anyhow!("git {}: {e}", args.join(" ")))?;
        if !output.status.success() {
            anyhow::bail!(
                "git {} failed at {}: {}",
                args.join(" "),
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    };
    run(&["init", "-q"])?;
    // A first commit so the worktree isn't in the awkward "no HEAD"
    // state where some shell prompts get noisy.
    let readme = path.join("README.md");
    std::fs::write(&readme, "# lazybox test repo\n")?;
    run(&["add", "."])?;
    run(&["commit", "-qm", "init"])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_creates_a_real_git_repo() {
        let fx = TestFixture::new().expect("fixture builds");
        assert!(fx.repo.path().join(".git").is_dir(), "git init ran");
    }

    #[test]
    fn fixture_seeds_setup_kv_row_so_setup_screen_skips() {
        let fx = TestFixture::new().unwrap();
        let raw = fx
            .store
            .get_kv(KV_KEY_SETUP)
            .unwrap()
            .expect("kv populated");
        let p: PersistedSetup = serde_json::from_str(&raw).unwrap();
        assert!(p.enabled_providers.contains("github"));
        assert!(p.enabled_agents.contains("claude"));
        assert!(p.provider_filters.contains_key("github"));
    }

    #[test]
    fn default_fixture_seeds_no_workspaces() {
        // Per UX direction: --test boots with an empty sidebar so the
        // user creates workspaces by hand.
        let fx = TestFixture::new().unwrap();
        let rows = fx.store.list_workspaces().unwrap();
        assert!(rows.is_empty(), "no seeded workspaces in default mode");
    }

    #[test]
    fn seeded_variant_creates_one_workspace_pointing_at_temp_repo() {
        let fx = TestFixture::new_with_seeded_session().unwrap();
        let rows = fx.store.list_workspaces().unwrap();
        assert_eq!(rows.len(), 1, "exactly one seeded workspace");
        let json = rows[0].workspace_json.as_ref().expect("json present");
        let workspace: Workspace = serde_json::from_str(json).unwrap();
        // Hierarchy invariant: the workspace owns one Session, whose
        // worktree_path is the test fixture's tempdir.
        assert_eq!(workspace.session_count(), 1, "one session");
        assert_eq!(
            workspace.sessions[0].worktree_path.as_path(),
            fx.repo.path()
        );
        assert!(
            workspace.name.contains("Test"),
            "workspace name is recognizable"
        );
    }

    #[test]
    fn fixture_includes_the_builtin_audit_snippet() {
        let fx = TestFixture::new_with_seeded_session().unwrap();
        assert!(
            fx.snippets.get("audit").is_some(),
            "--test users can follow the broadcast quickstart"
        );
    }
}
