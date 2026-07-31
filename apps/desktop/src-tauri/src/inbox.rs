//! The desktop's state-of-record for the workspace inbox (#732).
//!
//! The gateway control stream and the config both feed this reducer;
//! the frontend never groups or sorts. On every workspace/agent change
//! it re-runs the shared `lazybox_tui_core::inbox::compute_inbox_view`
//! and emits an [`InboxView`] (grouped rows + per-row badge data) to the
//! thin TypeScript renderer. Sort mode and repo-collapse are the only
//! UI state the frontend drives back down, via Tauri commands.

use lazybox_config::AttentionConfig;
use lazybox_core::{Project, ProjectKey, SessionKey, Workspace};
use lazybox_ipc::AgentState;
use lazybox_server::api_gateway::DesktopEvent;
use lazybox_tui_core::inbox::{
    ComputeInputs, FilterSet, InboxView, Mailbox, SortMode, compute_inbox_view,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Everything needed to recompute the grouped inbox view. The workspace
/// and agent maps are reduced from gateway events; `attention` and
/// `projects` are derived once from the user's config at startup.
pub struct InboxModel {
    workspaces: HashMap<SessionKey, Workspace>,
    agents: HashMap<SessionKey, AgentState>,
    sort_mode: SortMode,
    collapsed: BTreeSet<String>,
    attention: AttentionConfig,
    projects: BTreeMap<ProjectKey, Project>,
    filters: FilterSet,
}

impl InboxModel {
    pub fn new(attention: AttentionConfig, projects: BTreeMap<ProjectKey, Project>) -> Self {
        Self {
            workspaces: HashMap::new(),
            agents: HashMap::new(),
            sort_mode: SortMode::default(),
            collapsed: BTreeSet::new(),
            attention,
            projects,
            filters: FilterSet::new(),
        }
    }

    /// Reduce one control event into the workspace/agent maps. Returns
    /// `true` when the maps changed and the view should be re-emitted.
    pub fn apply(&mut self, event: &DesktopEvent) -> bool {
        match event {
            DesktopEvent::Snapshot {
                workspaces,
                terminals,
            } => {
                self.workspaces = workspaces
                    .iter()
                    .map(|w| (SessionKey::from(&w.key), w.clone()))
                    .collect();
                self.agents = terminals
                    .iter()
                    .filter_map(|t| t.agent_state.map(|state| (t.session_key.clone(), state)))
                    .collect();
                true
            }
            DesktopEvent::WorkspaceUpserted(workspace) => {
                self.workspaces
                    .insert(SessionKey::from(&workspace.key), (**workspace).clone());
                true
            }
            DesktopEvent::WorkspaceRemoved(key) => {
                self.workspaces.remove(&SessionKey::from(key));
                true
            }
            DesktopEvent::AgentState {
                session_key, state, ..
            } => {
                let previous = self.agents.insert(session_key.clone(), *state);
                // The view only reflects one agent-derived signal: whether
                // the workspace is asking for input (attention). Re-emit
                // only when that membership flips, so a `Working`/`Done`
                // transition or a repeated reading doesn't rebuild the
                // whole inbox. Mirrors the TUI's `asking_changed` gate.
                let was_asking = previous == Some(AgentState::InputNeeded);
                let now_asking = *state == AgentState::InputNeeded;
                was_asking != now_asking
            }
            _ => false,
        }
    }

    /// Replace the workspace map from an initial `/v1/workspaces` fetch,
    /// so the first view isn't empty while waiting for the stream's
    /// opening snapshot.
    pub fn seed_workspaces(&mut self, workspaces: &[Workspace]) {
        self.workspaces = workspaces
            .iter()
            .map(|w| (SessionKey::from(&w.key), w.clone()))
            .collect();
    }

    pub fn cycle_sort_mode(&mut self) {
        self.sort_mode = self.sort_mode.next();
    }

    pub fn set_sort_mode(&mut self, mode: SortMode) {
        self.sort_mode = mode;
    }

    pub fn toggle_collapsed(&mut self, label: &str) {
        if !self.collapsed.remove(label) {
            self.collapsed.insert(label.to_string());
        }
    }

    pub fn view(&self, now: chrono::DateTime<chrono::Utc>) -> InboxView {
        compute_inbox_view(ComputeInputs {
            workspaces: &self.workspaces,
            mailbox: Mailbox::Inbox,
            filters: &self.filters,
            sort_mode: self.sort_mode,
            show_inactive_in_inbox: false,
            projects: &self.projects,
            collapsed_repos: &self.collapsed,
            attention: &self.attention,
            agents: &self.agents,
            now,
            search: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_core::{
        CiStatus, Mergeable, ReviewStatus, Task, TaskId, TaskKind, TaskRole, TaskState,
        WorkspaceKey,
    };
    use lazybox_tui_core::inbox::VisibleRow;

    fn now() -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap()
    }

    fn pr(key: &str, repo: &str, num: u32) -> Workspace {
        let task = Task {
            id: TaskId {
                source: "github".into(),
                key: format!("{repo}#{num}"),
            },
            title: format!("PR {num}"),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::Success,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "x".into(),
            repo: Some(repo.into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            kind: Some(TaskKind::Pr),
            closes_issues: vec![],
        };
        let mut ws = Workspace::from_task(task, now());
        ws.key = WorkspaceKey(key.into());
        ws
    }

    fn model() -> InboxModel {
        InboxModel::new(AttentionConfig::default(), BTreeMap::new())
    }

    #[test]
    fn snapshot_populates_then_upsert_and_remove_track_the_map() {
        let mut m = model();
        assert!(m.apply(&DesktopEvent::Snapshot {
            workspaces: vec![pr("k1", "owner/r", 1)],
            terminals: vec![],
        }));
        assert_eq!(m.view(now()).total, 1);

        assert!(m.apply(&DesktopEvent::WorkspaceUpserted(Box::new(pr(
            "k2", "owner/r", 2
        )))));
        assert_eq!(m.view(now()).total, 2);

        assert!(m.apply(&DesktopEvent::WorkspaceRemoved(WorkspaceKey("k1".into()))));
        assert_eq!(m.view(now()).total, 1);
    }

    #[test]
    fn seed_workspaces_replaces_the_map_so_a_refetch_view_is_current() {
        let mut m = model();
        m.seed_workspaces(&[pr("k1", "owner/r", 1)]);
        assert_eq!(m.view(now()).total, 1);
        // A later refetch is authoritative: it replaces the set wholesale.
        m.seed_workspaces(&[pr("k2", "owner/r", 2), pr("k3", "owner/r", 3)]);
        let view = m.view(now());
        assert_eq!(view.total, 2);
        assert!(view.workspaces.contains_key("k2"));
        assert!(!view.workspaces.contains_key("k1"));
    }

    #[test]
    fn cycle_sort_mode_matches_the_shared_taxonomy() {
        let mut m = model();
        assert_eq!(m.view(now()).sort_mode, SortMode::ByRoleSplit);
        m.cycle_sort_mode();
        assert_eq!(m.view(now()).sort_mode, SortMode::Recent);
        m.cycle_sort_mode();
        assert_eq!(m.view(now()).sort_mode, SortMode::ByRole);
    }

    #[test]
    fn toggle_collapsed_round_trips_and_hides_rows() {
        let mut m = model();
        m.apply(&DesktopEvent::Snapshot {
            workspaces: vec![pr("k1", "owner/r", 1)],
            terminals: vec![],
        });
        m.toggle_collapsed("owner/r");
        let view = m.view(now());
        assert_eq!(view.collapsed, vec!["owner/r".to_string()]);
        assert_eq!(
            view.total, 1,
            "collapsing must not shrink the workspace count"
        );
        assert!(
            !view
                .rows
                .iter()
                .any(|r| matches!(r, VisibleRow::Workspace(_))),
            "collapsed repo hides its workspace rows"
        );
        m.toggle_collapsed("owner/r");
        assert!(m.view(now()).collapsed.is_empty());
    }

    #[test]
    fn ignored_events_do_not_request_a_re_emit() {
        let mut m = model();
        assert!(!m.apply(&DesktopEvent::PollProgress {
            source: "github".into(),
            message: "page 2".into(),
        }));
    }

    #[test]
    fn agent_state_re_emits_only_on_the_asking_flip() {
        let mut m = model();
        let key = SessionKey::from("owner/r#1");
        let agent = |state| DesktopEvent::AgentState {
            session_key: key.clone(),
            terminal_id: lazybox_ipc::TerminalId(1),
            state,
        };

        // Non-asking transitions never rebuild the view.
        assert!(!m.apply(&agent(AgentState::Working)));
        assert!(!m.apply(&agent(AgentState::Working)), "repeat is a no-op");
        // Rising edge into InputNeeded flips attention -> re-emit.
        assert!(m.apply(&agent(AgentState::InputNeeded)));
        assert!(
            !m.apply(&agent(AgentState::InputNeeded)),
            "still asking -> no re-emit"
        );
        // Falling edge out of InputNeeded flips back -> re-emit.
        assert!(m.apply(&agent(AgentState::Done)));
        assert!(!m.apply(&agent(AgentState::Idle)));
    }
}
