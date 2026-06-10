//! `Workspace` and `Session` — lazybox's hierarchy.
//!
//! ## The hierarchy (canonical)
//!
//! ```text
//! Repo            owner/name string from the task's provider.
//!  └─ Workspace   one unit of work; one PR + linked issues.
//!      └─ Session = one folder worktree on disk.
//!          └─ Terminal  one PTY rooted in that folder.
//! ```
//!
//! Each layer has a single responsibility; deviations are bugs:
//!
//! - **Repo** isn't a struct — it's the `task.repo` string. Multiple
//!   workspaces can share a repo (different PRs in the same repo).
//! - **Workspace** is the unit of work. Plain serializable record;
//!   no behavior trait — variation lives in providers and backends.
//! - **Session** = **one folder worktree.** A workspace with no
//!   sessions has no worktree (purely tracking). Multiple sessions
//!   per workspace = multiple worktrees for the same PR (review
//!   folder + experiment folder, etc).
//! - **Terminal** is a PTY belonging to a session — never directly to
//!   a workspace. Without a session there's no folder, so there's
//!   nothing for a terminal to root in.
//!
//! Separating "the unit of work" (Workspace) from "the running thing"
//! (Session) lets a single workspace host parallel agents and shells —
//! e.g. Claude AND Codex on the same PR, or a long-running shell next
//! to an agent. Both persist across lazybox restarts: the daemon keeps
//! the worktree and the PTYs alive, the store remembers everything else.

use crate::task::{Activity, Task, TaskId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use uuid::Uuid;

/// Stable identifier for a workspace. Human-readable so it survives
/// renames and shows up well in logs / UIs ("fix-auth-2026-04").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceKey(pub String);

impl WorkspaceKey {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkspaceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable identifier for a session within a workspace. UUID so we can
/// allocate one client-side without round-tripping the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Upper bound on the merged activity feed. Generous — a feed this
/// long is far past anything the UI surfaces — but finite, so a
/// years-old PR with endless bot chatter can't grow the serialized
/// workspace JSON without bound. `merge_activity` drops the oldest
/// items past this.
pub const MAX_ACTIVITY_ITEMS: usize = 500;

/// Content identity used to carry read/seen state across re-sorts
/// and to de-duplicate node-id-less activity.
fn activity_key(a: &Activity) -> (String, String, DateTime<Utc>) {
    (a.author.clone(), a.body.clone(), a.created_at)
}

/// One workspace = one unit of work (PR + linked issues), holding
/// **zero or more sessions**. A session is one folder worktree on
/// disk; without sessions the workspace is purely a tracking row
/// with no on-disk presence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub key: WorkspaceKey,
    /// The Project this workspace lives under (sidebar grouping
    /// header). `None` only during back-compat reads of pre-Project
    /// records; the daemon back-fills via `project_key_from_task`
    /// before broadcasting. Treat absence as "infer from
    /// `primary_task().repo`" everywhere else.
    #[serde(default)]
    pub project_key: Option<crate::ProjectKey>,
    /// Display name. Defaults to the PR title or the first issue's
    /// title when first created; user can rename.
    pub name: String,
    /// Branch this workspace tracks. Required — even a "from scratch"
    /// workspace lives on a branch.
    pub branch: String,
    /// Live runtime sessions. **Each session = one folder worktree.**
    /// Zero sessions = no on-disk presence. Multiple sessions = the
    /// user opened separate worktrees for the same branch (review +
    /// experiment, agent A + agent B, etc.).
    #[serde(default)]
    pub sessions: Vec<Session>,
    /// At most one PR.
    pub pr: Option<Task>,
    pub gh_issues: Vec<Task>,
    pub linear_issues: Vec<Task>,
    /// Merged activity from every linked task, sorted newest-first.
    pub activity: Vec<Activity>,
    pub seen_count: usize,
    #[serde(default)]
    pub read_indices: HashSet<usize>,
    #[serde(default)]
    pub snoozed_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub last_viewed_at: Option<DateTime<Utc>>,
}

impl Workspace {
    /// Empty workspace on `branch` with no linked tasks. Used for the
    /// "create a workspace from scratch" path.
    pub fn empty(key: WorkspaceKey, branch: impl Into<String>, now: DateTime<Utc>) -> Self {
        let branch = branch.into();
        Self {
            name: key.as_str().to_string(),
            key,
            project_key: None,
            branch,
            sessions: Vec::new(),
            pr: None,
            gh_issues: Vec::new(),
            linear_issues: Vec::new(),
            activity: Vec::new(),
            seen_count: 0,
            read_indices: HashSet::new(),
            snoozed_until: None,
            created_at: now,
            last_viewed_at: None,
        }
    }

    /// Append a fresh session and return its id. Sessions own a
    /// worktree path; the workspace becomes "live on disk" only once
    /// at least one session has been added.
    ///
    /// If a session with the same id already exists, this is a
    /// no-op — protects against a daemon-side resend or a buggy
    /// caller pushing duplicates into the list.
    pub fn add_session(&mut self, session: Session) -> SessionId {
        let id = session.id;
        if !self.sessions.iter().any(|s| s.id == id) {
            self.sessions.push(session);
        }
        id
    }

    /// Drop the session with `id` if present. Returns `true` if a
    /// session was actually removed. The caller is responsible for
    /// cleaning up the worktree on disk.
    pub fn remove_session(&mut self, id: SessionId) -> bool {
        let before = self.sessions.len();
        self.sessions.retain(|s| s.id != id);
        before != self.sessions.len()
    }

    pub fn find_session(&self, id: SessionId) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// The session lazybox should target when the user issues a workspace-
    /// scoped action without picking a specific session. Today: the
    /// most recently created. None when the workspace has no sessions.
    pub fn default_session(&self) -> Option<&Session> {
        self.sessions.iter().max_by_key(|s| s.created_at)
    }

    /// Build a workspace from a single task as the seed. PRs become
    /// the workspace's PR slot; issues go into `gh_issues` /
    /// `linear_issues`. Used when the daemon discovers a new task
    /// that isn't yet attached to anything.
    pub fn from_task(task: Task, now: DateTime<Utc>) -> Self {
        let key = WorkspaceKey::new(workspace_key_for(&task));
        let branch = task
            .branch
            .clone()
            .unwrap_or_else(|| key.as_str().to_string());
        let project_key = project_key_for_task(&task);
        let mut ws = Self::empty(key, branch, now);
        ws.name = task.title.clone();
        ws.activity = task.recent_activity.clone();
        ws.project_key = project_key;
        ws.attach_task(task);
        ws
    }

    /// Sort the activity list by `created_at` descending. Idempotent.
    pub fn sort_activity(&mut self) {
        // `sort_by_key` with `Reverse` is what the modern clippy lint
        // wants — same descending order as the old `b.cmp(&a)` closure
        // but without the bare-comparison shape `unnecessary_sort_by`
        // denies.
        self.activity
            .sort_by_key(|a| std::cmp::Reverse(a.created_at));
    }

    /// Attach a task to this workspace. Routing rules:
    /// - PR → the `pr` slot (replaces the existing one if any). When
    ///   replacing, lazy-fetched fields on the existing PR are
    ///   preserved if the incoming task left them empty — see
    ///   `preserve_lazy_pr_fields` for the list.
    /// - GitHub issue → `gh_issues` (de-duped by `TaskId`).
    /// - Linear ticket → `linear_issues` (de-duped by `TaskId`).
    /// - Anything else → silently dropped (we don't have a slot).
    ///
    /// Activity from `task.recent_activity` is merged into the
    /// workspace's feed and de-duplicated.
    pub fn attach_task(&mut self, task: Task) {
        match classify(&task) {
            TaskSlot::Pr => {
                let merged = match self.pr.take() {
                    Some(existing) => preserve_lazy_pr_fields(task.clone(), &existing),
                    None => task.clone(),
                };
                self.pr = Some(merged);
            }
            TaskSlot::GhIssue => upsert_by_id(&mut self.gh_issues, task.clone()),
            TaskSlot::LinearIssue => upsert_by_id(&mut self.linear_issues, task.clone()),
            TaskSlot::Unknown => return,
        }
        self.merge_activity(&task.recent_activity);
    }

    /// Detach by id — works on any slot. Silently no-op if missing.
    pub fn detach_task(&mut self, id: &TaskId) {
        if self.pr.as_ref().map(|p| &p.id) == Some(id) {
            self.pr = None;
        }
        self.gh_issues.retain(|t| &t.id != id);
        self.linear_issues.retain(|t| &t.id != id);
    }

    /// Find an attached task by id across all slots (PR, GitHub issues,
    /// Linear issues). Returns `None` if no slot holds it.
    pub fn task_by_id(&self, id: &TaskId) -> Option<&Task> {
        self.pr
            .iter()
            .chain(self.gh_issues.iter())
            .chain(self.linear_issues.iter())
            .find(|t| &t.id == id)
    }

    /// Every linked task's id, deduplicated.
    pub fn linked_task_ids(&self) -> Vec<TaskId> {
        let mut out = Vec::new();
        if let Some(pr) = &self.pr {
            out.push(pr.id.clone());
        }
        out.extend(self.gh_issues.iter().map(|t| t.id.clone()));
        out.extend(self.linear_issues.iter().map(|t| t.id.clone()));
        out
    }

    /// Merge a slice of activity items into the feed, de-duping by
    /// `node_id` when both sides carry one (an *edited* comment keeps
    /// its node id but changes its body — upserting by node id
    /// replaces the stale copy instead of appending a duplicate
    /// forever), falling back to the (author, body, created_at)
    /// content tuple for node-id-less items. Re-sorts newest-first
    /// afterwards. Cheap to call repeatedly — provider polls produce
    /// overlapping feeds.
    ///
    /// Read/seen state is remapped *content-wise* across the merge:
    /// both `read_indices` (explicit per-item marks) and `seen_count`
    /// (the positional "oldest N items are seen" tail) store
    /// positions, but the list reshuffles when new items arrive — and
    /// not only at the top: the lazy PR-details backfill inserts
    /// review comments *older* than everything already seen. A purely
    /// positional `seen_count` would shift across such a merge,
    /// flipping already-seen newest items back to unread while the
    /// backfilled items silently land inside the seen tail. Instead
    /// we snapshot the content keys of the read set AND the seen tail
    /// before mutating, then reconstruct: an item is read iff its key
    /// was read or seen before. `seen_count` is recomputed as the
    /// longest all-previously-seen suffix; previously-seen items
    /// stranded above that suffix move into `read_indices`.
    ///
    /// An edited comment's key changes (new body), so it deliberately
    /// resurfaces as unread — the content changed, the user should
    /// see it.
    ///
    /// The feed is capped at [`MAX_ACTIVITY_ITEMS`], dropping the
    /// oldest entries, so a long-lived PR can't grow the serialized
    /// workspace without bound.
    pub fn merge_activity(&mut self, incoming: &[Activity]) {
        // A no-op merge must not disturb anything — in particular not
        // re-derive `seen_count`/`read_indices`, which callers may
        // have set on a workspace whose activity hasn't been fetched
        // yet (every poll upserts tasks with empty `recent_activity`).
        if incoming.is_empty() {
            return;
        }
        // Snapshot read + seen state by content key BEFORE we mutate.
        let read_keys: HashSet<(String, String, DateTime<Utc>)> = self
            .read_indices
            .iter()
            .filter_map(|i| self.activity.get(*i))
            .map(activity_key)
            .collect();
        let seen_start = self.activity.len().saturating_sub(self.seen_count);
        let seen_keys: HashSet<(String, String, DateTime<Utc>)> = self
            .activity
            .get(seen_start..)
            .unwrap_or(&[])
            .iter()
            .map(activity_key)
            .collect();

        for act in incoming {
            // Upsert by node id when both sides have one — replaces
            // the body/edited fields of the stored copy.
            if let Some(nid) = act.node_id.as_deref()
                && let Some(existing) = self
                    .activity
                    .iter_mut()
                    .find(|a| a.node_id.as_deref() == Some(nid))
            {
                *existing = act.clone();
                continue;
            }
            // Tuple fallback. Upsert rather than skip so a re-poll
            // that gained a node_id migrates it onto the stored item.
            if let Some(existing) = self.activity.iter_mut().find(|a| {
                a.author == act.author && a.body == act.body && a.created_at == act.created_at
            }) {
                *existing = act.clone();
            } else {
                self.activity.push(act.clone());
            }
        }
        self.sort_activity();
        // Sorted newest-first, so truncation drops the oldest items.
        self.activity.truncate(MAX_ACTIVITY_ITEMS);

        // Reconstruct seen/read content-wise. `seen_count` becomes
        // the longest suffix (= oldest run) of previously-seen items;
        // everything read-or-seen above that suffix gets an explicit
        // read mark instead.
        let mut suffix = 0usize;
        for a in self.activity.iter().rev() {
            if seen_keys.contains(&activity_key(a)) {
                suffix += 1;
            } else {
                break;
            }
        }
        self.seen_count = suffix;
        let cut = self.activity.len() - suffix;
        self.read_indices = self
            .activity
            .get(..cut)
            .unwrap_or(&[])
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                let key = activity_key(a);
                (read_keys.contains(&key) || seen_keys.contains(&key)).then_some(i)
            })
            .collect();
    }

    /// The "headline" task for this workspace — the one components
    /// like the sidebar row and the right-pane header render. PRs
    /// always win over issues; among issues we pick the first GitHub
    /// issue, then the first Linear issue. None only when the
    /// workspace was created empty (`Workspace::empty`) and nothing
    /// has been attached yet.
    pub fn primary_task(&self) -> Option<&Task> {
        self.pr
            .as_ref()
            .or_else(|| self.gh_issues.first())
            .or_else(|| self.linear_issues.first())
    }

    /// Number of activity items the user hasn't seen.
    pub fn unread_count(&self) -> usize {
        (0..self.activity.len().saturating_sub(self.seen_count))
            .filter(|i| !self.read_indices.contains(i))
            .count()
    }

    /// Indices of activity items the user hasn't seen yet, in
    /// activity-list order (newest first since `sort_activity`
    /// sorts descending). Used by `resolve_work` to auto-fill the
    /// "address comments" prompt when the user hasn't explicitly
    /// selected any but the row has unread activity.
    pub fn unread_activity_indices(&self) -> Vec<usize> {
        (0..self.activity.len().saturating_sub(self.seen_count))
            .filter(|i| !self.read_indices.contains(i))
            .collect()
    }

    /// Whether the activity at `index` is currently unread.
    pub fn is_activity_unread(&self, index: usize) -> bool {
        index < self.activity.len().saturating_sub(self.seen_count)
            && !self.read_indices.contains(&index)
    }

    /// Mark every currently-known activity item as read. Called when
    /// the user opens the workspace and all items become "seen".
    pub fn mark_read_all(&mut self) {
        self.seen_count = self.activity.len();
        self.read_indices.clear();
    }

    /// Mark exactly one activity as read. Used by the auto-mark-on-
    /// hover feature — landing the cursor on an unread row arms a
    /// short timer; on expiry the App calls this. Idempotent: marking
    /// an already-read index is a no-op.
    pub fn mark_activity_read(&mut self, index: usize) {
        if index < self.activity.len() {
            self.read_indices.insert(index);
        }
    }

    /// Reverse of `mark_activity_read`. Bound to the `z` undo key —
    /// pulls the index back into the unread set without disturbing
    /// other read state. No-op if the index wasn't in the set.
    pub fn unmark_activity_read(&mut self, index: usize) {
        self.read_indices.remove(&index);
        // Also reduce seen_count if this index was inside the auto-
        // seen tail (`activity.len() - seen_count`). Without this, an
        // undo immediately after a snapshot-driven seen bump would
        // not restore the unread state. Shrinking the tail to start
        // below `index` also un-seens every newer item that shared
        // the tail, so those displaced indices get explicit read
        // marks — only the target index becomes unread.
        let auto_seen_threshold = self.activity.len().saturating_sub(self.seen_count);
        if index >= auto_seen_threshold {
            self.seen_count = self.activity.len().saturating_sub(index + 1);
            for displaced in auto_seen_threshold..index {
                self.read_indices.insert(displaced);
            }
        }
    }

    pub fn is_snoozed(&self, now: DateTime<Utc>) -> bool {
        match self.snoozed_until {
            Some(until) => until > now,
            None => false,
        }
    }

    /// On-disk identifier for this workspace's worktrees. Human-
    /// readable so a shell prompt sitting in the worktree is
    /// instantly recognisable.
    ///
    /// Resolution order:
    /// - PR attached → `PR-{number}-{slug-of-title}` (capped at 8
    ///   words so it stays scannable).
    /// - No PR but a custom workspace name → slug of `name`.
    /// - Both empty → fall back to a stable `workspace_{key-suffix}`
    ///   placeholder so the path is always non-empty.
    pub fn worktree_slug(&self) -> String {
        if let Some(pr) = self.pr.as_ref()
            && let Some((_, num_str)) = pr.id.key.rsplit_once('#')
            && let Ok(num) = num_str.parse::<u64>()
        {
            return crate::slug::pr_slug(num, &pr.title);
        }
        let name_slug = crate::slug::slugify(&self.name);
        if !name_slug.is_empty() {
            return name_slug;
        }
        // Fall-back: avoid empty paths even on a fully-anonymous
        // workspace. The key's tail keeps it unique across siblings.
        let suffix = self
            .key
            .as_str()
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        format!("workspace-{suffix}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskSlot {
    Pr,
    GhIssue,
    LinearIssue,
    Unknown,
}

fn classify(task: &Task) -> TaskSlot {
    if task.id.source == "linear" {
        return TaskSlot::LinearIssue;
    }
    // Provider-aware "is this a PR?" check — recognizes GitHub,
    // GitLab, and Bitbucket URL shapes via one shared method on
    // Task. New providers extend `Task::is_pr` rather than this
    // function.
    if task.is_pr() {
        return TaskSlot::Pr;
    }
    // Anything else from a known issue-tracking source is treated
    // as an issue. The explicit `/issues/` check catches paths even
    // if `source` is set to something custom.
    if task.url.contains("/issues/") || task.id.source == "github" {
        return TaskSlot::GhIssue;
    }
    TaskSlot::Unknown
}

fn upsert_by_id(list: &mut Vec<Task>, task: Task) {
    if let Some(slot) = list.iter_mut().find(|t| t.id == task.id) {
        *slot = task;
    } else {
        list.push(task);
    }
}

/// Preserve fields that the gh-provider's inbox-scan query no longer
/// fetches — `closes_issues` (from `closingIssuesReferences`) and
/// `checks` (from `statusCheckRollup.contexts`). Both are populated
/// by the lazy `PR_DETAILS_QUERY`; without this preservation, every
/// subsequent inbox poll would clobber them back to empty and the
/// issue↔PR collapse / per-check sidebar would flicker off.
///
/// Rule: incoming wins for any field it has populated; existing
/// wins only for the listed lazy fields when incoming is empty.
/// This keeps "PR actually has no closing refs" working correctly
/// — the inbox-scan path always emits empty so we'd never know the
/// difference, but the post-workspace-open lazy-fetch IS authoritative
/// (it pulls the field directly) and a refresh there will reset to
/// the new truth.
///
/// `mergeable` follows the same shape with `Unknown` as its "empty"
/// sentinel. GitHub computes mergeability lazily and *evicts* it
/// between requests: a PR we last saw as `Conflicting` comes back
/// `mergeable: UNKNOWN` on the next inbox scan until GitHub
/// recomputes. Letting that `Unknown` win clobbers the conflict
/// verdict, so the CONFLICT badge blinks off on the very next poll
/// even though nothing changed. Keep the last known verdict until a
/// poll reports a real one. `is_behind_base` rides the same
/// `mergeStateStatus` computation (`UNKNOWN` for both at once), so it
/// is only meaningful when `mergeable` resolved — preserve it on the
/// same condition.
fn preserve_lazy_pr_fields(mut incoming: Task, existing: &Task) -> Task {
    if incoming.closes_issues.is_empty() && !existing.closes_issues.is_empty() {
        incoming.closes_issues = existing.closes_issues.clone();
    }
    if incoming.checks.is_empty() && !existing.checks.is_empty() {
        incoming.checks = existing.checks.clone();
    }
    if incoming.mergeable == crate::Mergeable::Unknown
        && existing.mergeable != crate::Mergeable::Unknown
    {
        incoming.mergeable = existing.mergeable;
        incoming.is_behind_base = existing.is_behind_base;
    }
    incoming
}

/// Stable per-task workspace key generator. PR `o/r#123` → "o/r-123".
/// Used so that "the workspace for this PR" resolves predictably even
/// before the user gives the workspace a custom name.
pub fn workspace_key_for(task: &Task) -> String {
    sanitize_key(&format!("{}-{}", task.id.source, task.id.key))
}

/// The Project a workspace belongs to. Prefers the stored
/// `project_key` field (populated by `from_task` and by the
/// `n` create flow); falls back to deriving from the primary
/// task's repo so pre-Stage-1 records still group correctly.
/// Returns `None` only for orphaned workspaces with no
/// project_key AND no upstream task — those land under the
/// legacy "(no repo)" header.
pub fn workspace_project_key(w: &Workspace) -> Option<crate::ProjectKey> {
    if let Some(pk) = &w.project_key {
        return Some(pk.clone());
    }
    project_key_for_task(w.primary_task()?)
}

/// Derive the Project key a task should live under. GitHub tasks
/// with a `owner/repo` string become `ProjectKey::github(owner,
/// repo)`. Linear tasks with a repo (used as team-id) become
/// `ProjectKey::linear(team)`. Tasks without an upstream repo
/// return `None`; the daemon's polling loop is responsible for
/// not registering a project in that case.
pub fn project_key_for_task(task: &Task) -> Option<crate::ProjectKey> {
    let repo = task.repo.as_deref()?.trim();
    if repo.is_empty() {
        return None;
    }
    match task.id.source.as_str() {
        "github" => {
            // `owner/repo` → ProjectKey::github(owner, repo). Repos
            // without a slash (defensive) are stored as a single-
            // segment github key — better than dropping them.
            if let Some((owner, name)) = repo.split_once('/') {
                Some(crate::ProjectKey::github(owner, name))
            } else {
                Some(crate::ProjectKey::new(format!("github-{repo}")))
            }
        }
        "linear" => Some(crate::ProjectKey::linear(repo)),
        _ => Some(crate::ProjectKey::new(format!(
            "{}-{}",
            task.id.source,
            sanitize_key(repo)
        ))),
    }
}

fn sanitize_key(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '-',
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

// ─────────────────────────────────────────────────────────────────────
// Sessions
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionKind {
    /// `claude`, `codex`, `cursor`, etc. The agent registry resolves
    /// `agent_id` to argv at spawn time.
    Agent { agent_id: String },
    /// Plain login shell (bash/zsh).
    Shell,
    /// A view that compares the live output of two or more other
    /// sessions in the SAME workspace. Implemented as a real process
    /// the daemon spawns; survives restart like any other session.
    Compare { source_sessions: Vec<SessionId> },
    /// Tail a file (build log, test output) inside the worktree.
    LogTail { path: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRunState {
    /// Process is running and producing output.
    Active,
    /// Process exists but is idle (no recent output).
    Idle,
    /// Agent waiting on the user (Claude's "Are you sure?" prompts).
    Asking,
    /// Process exited.
    Stopped,
}

/// How runners are arranged inside a session's surface area.
///
/// Default `Tabs` is what shipped first: one runner full-pane with a
/// tab strip on top, switch with the next-tab key. `Splits` is the
/// tile-manager variant: a tree of horizontal/vertical splits with
/// runners at the leaves, mirroring tmux panes.
///
/// The `Splits` variant is wired through persistence + IPC but the
/// renderer + key bindings still default to `Tabs`. Migration path:
/// the App reads `Session.layout`, picks Tabs rendering until the
/// tile renderer is wired, at which point the same data model works
/// without a schema change.
// External tagging (the serde default) is what bincode supports —
// internally-tagged enums fail `bincode::deserialize` because the
// format isn't self-describing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionLayout {
    Tabs {
        /// Index into `Session.runners`. Clamped on save.
        active: usize,
    },
    Splits {
        tree: TileTree,
        /// Path through `tree` to the focused leaf (0 = first child
        /// at each level, 1 = second). Empty when the tree is just
        /// a leaf.
        focused: Vec<u8>,
    },
}

impl Default for SessionLayout {
    fn default() -> Self {
        Self::Tabs { active: 0 }
    }
}

/// Failure modes for `TileTree::remove_at`. Callers today don't
/// branch on the variant — the enum exists so the function returns
/// a proper error type instead of `Result<_, ()>`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RemoveAtError {
    /// Caller asked to remove the root tile. We refuse because the
    /// session needs at least one runner visible.
    #[error("cannot remove the root tile")]
    CannotRemoveRoot,
    /// Path descends into a leaf or otherwise doesn't exist.
    #[error("tile path not found")]
    PathNotFound,
}

/// One node in the per-session tile tree. Leaves point to a runner
/// by terminal id (numeric, daemon-allocated). Splits hold a 0-100
/// `ratio` for the first child's share of the available space.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TileTree {
    Leaf {
        terminal_id: u64,
    },
    HSplit {
        left: Box<TileTree>,
        right: Box<TileTree>,
        ratio: u8,
    },
    VSplit {
        top: Box<TileTree>,
        bottom: Box<TileTree>,
        ratio: u8,
    },
}

/// Direction for spatial navigation between tiles. Maps onto vim
/// `Ctrl-w h/j/k/l`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileDirection {
    Left,
    Right,
    Up,
    Down,
}

impl TileTree {
    /// Every leaf's terminal id, in pre-order. Stable ordering — the
    /// renderer relies on this for the focused-tile highlight.
    pub fn leaves(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<u64>) {
        match self {
            TileTree::Leaf { terminal_id } => out.push(*terminal_id),
            TileTree::HSplit { left, right, .. } => {
                left.collect_leaves(out);
                right.collect_leaves(out);
            }
            TileTree::VSplit { top, bottom, .. } => {
                top.collect_leaves(out);
                bottom.collect_leaves(out);
            }
        }
    }

    /// Path through the tree to the leaf carrying `terminal_id`.
    /// Returns the steps as 0/1 (left-or-top vs. right-or-bottom).
    pub fn path_to(&self, terminal_id: u64) -> Option<Vec<u8>> {
        let mut path = Vec::new();
        if self.find_path(terminal_id, &mut path) {
            Some(path)
        } else {
            None
        }
    }

    fn find_path(&self, terminal_id: u64, path: &mut Vec<u8>) -> bool {
        match self {
            TileTree::Leaf { terminal_id: id } => *id == terminal_id,
            TileTree::HSplit { left, right, .. }
            | TileTree::VSplit {
                top: left,
                bottom: right,
                ..
            } => {
                path.push(0);
                if left.find_path(terminal_id, path) {
                    return true;
                }
                path.pop();
                path.push(1);
                if right.find_path(terminal_id, path) {
                    return true;
                }
                path.pop();
                false
            }
        }
    }

    /// Replace the leaf at `path` with `new` and return the previous
    /// subtree there. Used by split operations: take the focused
    /// leaf, wrap it in a Split with a new sibling.
    pub fn replace_at(&mut self, path: &[u8], new: TileTree) -> Option<TileTree> {
        if path.is_empty() {
            return Some(std::mem::replace(self, new));
        }
        let head = path[0];
        let rest = &path[1..];
        let next = match self {
            TileTree::HSplit { left, right, .. }
            | TileTree::VSplit {
                top: left,
                bottom: right,
                ..
            } => {
                if head == 0 {
                    left.as_mut()
                } else {
                    right.as_mut()
                }
            }
            TileTree::Leaf { .. } => return None,
        };
        next.replace_at(rest, new)
    }

    /// Remove the leaf at `path`, collapsing its parent split into
    /// the surviving sibling. Returns Ok with the new path of focus
    /// (the sibling's path) on success. Errors when the path points
    /// at the root (can't collapse the only tile) or doesn't exist.
    pub fn remove_at(&mut self, path: &[u8]) -> Result<Vec<u8>, RemoveAtError> {
        if path.is_empty() {
            // Caller is trying to delete the only tile. Refuse — the
            // session needs at least one runner visible.
            return Err(RemoveAtError::CannotRemoveRoot);
        }
        if path.len() == 1 {
            // Collapse the parent (which is `self`) into the sibling.
            let head = path[0];
            let new_root = match self {
                TileTree::HSplit { left, right, .. }
                | TileTree::VSplit {
                    top: left,
                    bottom: right,
                    ..
                } => {
                    if head == 0 {
                        std::mem::replace(right.as_mut(), TileTree::Leaf { terminal_id: 0 })
                    } else {
                        std::mem::replace(left.as_mut(), TileTree::Leaf { terminal_id: 0 })
                    }
                }
                TileTree::Leaf { .. } => return Err(RemoveAtError::PathNotFound),
            };
            *self = new_root;
            // After collapse, focus lands at the new root (no path).
            return Ok(Vec::new());
        }
        let head = path[0];
        let rest = &path[1..];
        let next = match self {
            TileTree::HSplit { left, right, .. }
            | TileTree::VSplit {
                top: left,
                bottom: right,
                ..
            } => {
                if head == 0 {
                    left.as_mut()
                } else {
                    right.as_mut()
                }
            }
            TileTree::Leaf { .. } => return Err(RemoveAtError::PathNotFound),
        };
        let mut sub_path = next.remove_at(rest)?;
        // Prefix the parent step so the returned focus path is full.
        sub_path.insert(0, head);
        Ok(sub_path)
    }

    /// Spatial neighbor of the leaf at `path` in the given direction.
    /// Returns the path to that neighbor leaf, or None if nothing
    /// lies in that direction (e.g. moving Left from the leftmost
    /// tile). Walks up to find an ancestor split that goes against
    /// the requested axis, then descends.
    pub fn neighbor(&self, path: &[u8], dir: TileDirection) -> Option<Vec<u8>> {
        // Walk up from the leaf until we find a split whose axis
        // matches `dir` AND we came from the "wrong" side (so we can
        // jump to the other side).
        let want_horizontal = matches!(dir, TileDirection::Left | TileDirection::Right);
        let want_first = matches!(dir, TileDirection::Left | TileDirection::Up);
        for i in (0..path.len()).rev() {
            let prefix = &path[..i];
            let step = path[i];
            let node = self.subtree_at(prefix)?;
            let split_is_horizontal = matches!(node, TileTree::HSplit { .. });
            if split_is_horizontal != want_horizontal {
                continue;
            }
            // We're inside a split whose axis matches the request.
            // Did we come from the "near" side (so `dir` would jump
            // us across), or from the "far" side (no neighbor here,
            // keep walking)?
            let came_from_near = (step == 1) == want_first;
            if !came_from_near {
                continue;
            }
            let mut new_path = prefix.to_vec();
            new_path.push(if want_first { 0 } else { 1 });
            // Descend into the chosen side's deepest leaf along the
            // SAME axis (so the cursor lands on a visible leaf).
            return Some(self.descend_to_leaf(&mut new_path));
        }
        None
    }

    fn subtree_at(&self, path: &[u8]) -> Option<&TileTree> {
        let mut node = self;
        for &step in path {
            node = match node {
                TileTree::HSplit { left, right, .. }
                | TileTree::VSplit {
                    top: left,
                    bottom: right,
                    ..
                } => {
                    if step == 0 {
                        left.as_ref()
                    } else {
                        right.as_ref()
                    }
                }
                TileTree::Leaf { .. } => return None,
            };
        }
        Some(node)
    }

    /// From the subtree at `path`, walk down to its first leaf along
    /// the natural pre-order traversal. Mutates `path` in place,
    /// extending it. Returns the extended path.
    fn descend_to_leaf(&self, path: &mut Vec<u8>) -> Vec<u8> {
        let mut node = self.subtree_at(path);
        while let Some(n) = node {
            match n {
                TileTree::Leaf { .. } => break,
                TileTree::HSplit { .. } | TileTree::VSplit { .. } => {
                    path.push(0);
                    node = self.subtree_at(path);
                }
            }
        }
        path.clone()
    }
}

/// One running thing inside a workspace.
///
/// **A session IS a folder worktree.** It must point at a directory
/// on disk where its agent / shell / log-tail process runs. Without
/// a session there's no folder, so a workspace with `sessions = []`
/// is a pure tracking row with no on-disk presence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub workspace_key: WorkspaceKey,
    /// User-visible name. Defaults to the agent id ("claude") or
    /// "shell" / "compare" / "log: build.log".
    pub name: String,
    pub kind: SessionKind,
    pub state: SessionRunState,
    /// On-disk worktree this session lives in. Required: every
    /// session has a folder. Created lazily by the daemon's worktree
    /// manager when the session is first spawned and reused on
    /// subsequent agent runs in the same session.
    pub worktree_path: PathBuf,
    pub created_at: DateTime<Utc>,
    /// When the daemon last saw output from this session's PTY. None
    /// for compare/log sessions whose state model is different.
    #[serde(default)]
    pub last_output_at: Option<DateTime<Utc>>,
    /// Tile/tab arrangement for this session. Defaults to Tabs.
    /// Persisted so the user's layout survives restart.
    #[serde(default)]
    pub layout: SessionLayout,
}

impl Session {
    pub fn new(
        workspace_key: WorkspaceKey,
        kind: SessionKind,
        worktree_path: PathBuf,
        now: DateTime<Utc>,
    ) -> Self {
        let name = default_name_for(&kind);
        Self {
            id: SessionId::new(),
            workspace_key,
            name,
            kind,
            state: SessionRunState::Active,
            worktree_path,
            created_at: now,
            last_output_at: None,
            layout: SessionLayout::default(),
        }
    }
}

fn default_name_for(kind: &SessionKind) -> String {
    match kind {
        SessionKind::Agent { agent_id } => agent_id.clone(),
        SessionKind::Shell => "shell".into(),
        SessionKind::Compare { .. } => "compare".into(),
        SessionKind::LogTail { path } => format!("log: {path}"),
    }
}

#[cfg(test)]
mod tile_tree_tests {
    use super::*;

    fn leaf(id: u64) -> TileTree {
        TileTree::Leaf { terminal_id: id }
    }
    fn hsplit(left: TileTree, right: TileTree) -> TileTree {
        TileTree::HSplit {
            left: Box::new(left),
            right: Box::new(right),
            ratio: 50,
        }
    }
    fn vsplit(top: TileTree, bottom: TileTree) -> TileTree {
        TileTree::VSplit {
            top: Box::new(top),
            bottom: Box::new(bottom),
            ratio: 50,
        }
    }

    #[test]
    fn leaves_traverses_in_preorder() {
        // Tree: H(L=1, V(T=2, B=3))
        let t = hsplit(leaf(1), vsplit(leaf(2), leaf(3)));
        assert_eq!(t.leaves(), vec![1, 2, 3]);
    }

    #[test]
    fn path_to_finds_each_leaf() {
        let t = hsplit(leaf(1), vsplit(leaf(2), leaf(3)));
        assert_eq!(t.path_to(1), Some(vec![0]));
        assert_eq!(t.path_to(2), Some(vec![1, 0]));
        assert_eq!(t.path_to(3), Some(vec![1, 1]));
        assert_eq!(t.path_to(99), None);
    }

    #[test]
    fn replace_at_swaps_leaf_for_split() {
        let mut t = leaf(1);
        // Wrap leaf 1 in HSplit(1, 2).
        let prev = t.replace_at(&[], hsplit(leaf(1), leaf(2))).unwrap();
        assert_eq!(prev, leaf(1));
        assert_eq!(t.leaves(), vec![1, 2]);
    }

    #[test]
    fn remove_at_collapses_parent_split() {
        let mut t = hsplit(leaf(1), vsplit(leaf(2), leaf(3)));
        // Remove leaf 2 — VSplit collapses to leaf 3.
        let new_focus = t.remove_at(&[1, 0]).unwrap();
        assert_eq!(t.leaves(), vec![1, 3]);
        assert_eq!(new_focus, vec![1]);
    }

    #[test]
    fn remove_at_root_path_errors() {
        let mut t = leaf(1);
        assert!(t.remove_at(&[]).is_err(), "can't remove the only tile");
    }

    #[test]
    fn neighbor_right_jumps_to_sibling() {
        // H(1, 2): from 1, Right → 2.
        let t = hsplit(leaf(1), leaf(2));
        let path1 = t.path_to(1).unwrap();
        let n = t.neighbor(&path1, TileDirection::Right);
        assert_eq!(n, Some(vec![1]));
    }

    #[test]
    fn neighbor_left_at_leftmost_returns_none() {
        let t = hsplit(leaf(1), leaf(2));
        let path1 = t.path_to(1).unwrap();
        assert_eq!(t.neighbor(&path1, TileDirection::Left), None);
    }

    #[test]
    fn neighbor_up_in_vsplit() {
        // V(1, 2): from 2, Up → 1.
        let t = vsplit(leaf(1), leaf(2));
        let path2 = t.path_to(2).unwrap();
        assert_eq!(t.neighbor(&path2, TileDirection::Up), Some(vec![0]));
    }

    #[test]
    fn neighbor_walks_up_through_unrelated_split() {
        // H(1, V(2, 3)): from 1, Right should land on the deepest
        // first-leaf of the right subtree (= 2).
        let t = hsplit(leaf(1), vsplit(leaf(2), leaf(3)));
        let path1 = t.path_to(1).unwrap();
        let n = t.neighbor(&path1, TileDirection::Right);
        assert_eq!(n, Some(vec![1, 0]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{
        Activity, ActivityKind, CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState,
    };

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-04-28T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn pr(key: &str) -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: format!("PR {key}"),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Reviewer,
            ci: CiStatus::Success,
            review: ReviewStatus::Pending,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{key}").replace('#', "/pull/"),
            repo: Some("o/r".into()),
            branch: Some("feature/x".into()),
            base_branch: Some("main".into()),
            updated_at: now(),
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: crate::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
        }
    }

    fn issue(source: &str, key: &str) -> Task {
        let mut t = pr(key);
        t.id.source = source.into();
        t.url = if source == "linear" {
            format!("https://linear.app/team/issue/{key}")
        } else {
            format!("https://github.com/{key}").replace("/pull/", "/issues/")
        };
        t
    }

    fn activity_at(seconds: i64, body: &str) -> Activity {
        Activity {
            author: "alice".into(),
            body: body.into(),
            created_at: now() + chrono::Duration::seconds(seconds),
            kind: ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        }
    }

    #[test]
    fn from_task_makes_pr_workspace() {
        let ws = Workspace::from_task(pr("o/r#1"), now());
        assert!(ws.pr.is_some());
        assert!(ws.gh_issues.is_empty());
        assert!(ws.linear_issues.is_empty());
    }

    #[test]
    fn from_task_makes_issue_workspace() {
        let ws = Workspace::from_task(issue("github", "o/r#42"), now());
        assert!(ws.pr.is_none());
        assert_eq!(ws.gh_issues.len(), 1);
    }

    #[test]
    fn classify_routes_gitlab_merge_request_to_pr_slot() {
        // Future-provider regression: GitLab's merge-request URLs
        // must classify as PR via the shared `Task::is_pr` knob.
        // Today no provider crate produces these, but the model
        // is expected to route them correctly when one does.
        let mut t = pr("o/r#1");
        t.id.source = "gitlab".into();
        t.url = "https://gitlab.com/group/project/-/merge_requests/1".into();
        let ws = Workspace::from_task(t, now());
        assert!(
            ws.pr.is_some(),
            "GitLab merge_request URL must land in the PR slot",
        );
    }

    #[test]
    fn classify_routes_bitbucket_pull_request_to_pr_slot() {
        let mut t = pr("o/r#1");
        t.id.source = "bitbucket".into();
        t.url = "https://bitbucket.org/team/project/pull-requests/1".into();
        let ws = Workspace::from_task(t, now());
        assert!(
            ws.pr.is_some(),
            "Bitbucket pull-request URL must land in PR slot"
        );
    }

    #[test]
    fn attach_pr_replaces_existing_pr() {
        let mut ws = Workspace::from_task(pr("o/r#1"), now());
        ws.attach_task(pr("o/r#2"));
        assert_eq!(
            ws.pr.as_ref().unwrap().id.key,
            "o/r#2",
            "second PR replaces first"
        );
    }

    /// Regression for the GraphQL trim: the inbox-scan query no
    /// longer fetches `closingIssuesReferences` or
    /// `statusCheckRollup.contexts`, so the incoming PR's
    /// `closes_issues` / `checks` are empty. Without preservation,
    /// every poll cycle would wipe out the lazy-fetched values and
    /// the issue↔PR collapse would flicker off on every tick.
    #[test]
    fn attach_pr_preserves_lazy_fields_when_incoming_is_empty() {
        let mut first = pr("o/r#1");
        first.closes_issues = vec![TaskId {
            source: "github".into(),
            key: "o/r#42".into(),
        }];
        first.checks = vec![crate::CheckRun {
            name: "lint".into(),
            status: CiStatus::Success,
            url: None,
        }];
        let mut ws = Workspace::from_task(first, now());

        // Subsequent poll: same PR id, empty lazy fields (inbox-scan shape).
        let next = pr("o/r#1");
        ws.attach_task(next);

        let pr_ref = ws.pr.as_ref().unwrap();
        assert_eq!(
            pr_ref.closes_issues.len(),
            1,
            "closes_issues must survive an inbox-scan-shaped re-poll",
        );
        assert_eq!(pr_ref.closes_issues[0].key, "o/r#42");
        assert_eq!(
            pr_ref.checks.len(),
            1,
            "checks must survive an inbox-scan-shaped re-poll",
        );
        assert_eq!(pr_ref.checks[0].name, "lint");
    }

    /// Regression for the "merge conflicts are not detected
    /// correctly" bug: GitHub evicts computed mergeability between
    /// polls, so a conflicting PR comes back `mergeable: UNKNOWN` on
    /// the next inbox scan. Without preservation the CONFLICT verdict
    /// (and the BEHIND signal it shares a computation with) blinks off
    /// every other poll. The last known verdict must survive an
    /// `Unknown` re-poll.
    #[test]
    fn attach_pr_preserves_mergeable_when_incoming_is_unknown() {
        let mut first = pr("o/r#1");
        first.mergeable = crate::Mergeable::Conflicting;
        first.is_behind_base = true;
        let mut ws = Workspace::from_task(first, now());

        // Next poll: GitHub hasn't recomputed mergeability yet.
        let mut next = pr("o/r#1");
        next.mergeable = crate::Mergeable::Unknown;
        next.is_behind_base = false;
        ws.attach_task(next);

        let pr_ref = ws.pr.as_ref().unwrap();
        assert_eq!(
            pr_ref.mergeable,
            crate::Mergeable::Conflicting,
            "conflict verdict must survive an UNKNOWN re-poll",
        );
        assert!(
            pr_ref.is_behind_base,
            "behind-base must ride the same UNKNOWN preservation",
        );
    }

    /// Inverse: a real verdict (even `Mergeable`, the "no conflict"
    /// answer) always wins over the stored one. Preservation only
    /// guards against `Unknown` clobbering known state — it is not a
    /// sticky cache that pins the first conflict forever.
    #[test]
    fn attach_pr_mergeable_known_verdict_overwrites_stored() {
        let mut first = pr("o/r#1");
        first.mergeable = crate::Mergeable::Conflicting;
        first.is_behind_base = true;
        let mut ws = Workspace::from_task(first, now());

        // Next poll: conflict resolved, GitHub reports a real verdict.
        let mut next = pr("o/r#1");
        next.mergeable = crate::Mergeable::Mergeable;
        next.is_behind_base = false;
        ws.attach_task(next);

        let pr_ref = ws.pr.as_ref().unwrap();
        assert_eq!(
            pr_ref.mergeable,
            crate::Mergeable::Mergeable,
            "a resolved verdict must replace the stale conflict",
        );
        assert!(!pr_ref.is_behind_base);
    }

    /// Inverse: when the new PR DOES carry lazy fields (lazy-fetch
    /// or fully-eager fixture), incoming wins. Preservation is a
    /// one-way "don't clobber to empty" guard, not a sticky cache.
    #[test]
    fn attach_pr_lazy_fields_get_overwritten_when_incoming_has_them() {
        let mut first = pr("o/r#1");
        first.closes_issues = vec![TaskId {
            source: "github".into(),
            key: "o/r#42".into(),
        }];
        let mut ws = Workspace::from_task(first, now());

        let mut next = pr("o/r#1");
        next.closes_issues = vec![TaskId {
            source: "github".into(),
            key: "o/r#99".into(),
        }];
        ws.attach_task(next);

        let pr_ref = ws.pr.as_ref().unwrap();
        assert_eq!(pr_ref.closes_issues.len(), 1);
        assert_eq!(
            pr_ref.closes_issues[0].key, "o/r#99",
            "incoming wins when it has data",
        );
    }

    #[test]
    fn attach_routes_each_task_to_its_slot() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.attach_task(pr("o/r#1"));
        ws.attach_task(issue("github", "o/r#42"));
        ws.attach_task(issue("linear", "ENG-7"));
        assert!(ws.pr.is_some());
        assert_eq!(ws.gh_issues.len(), 1);
        assert_eq!(ws.linear_issues.len(), 1);
    }

    #[test]
    fn attaching_same_issue_twice_dedupes_by_id() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.attach_task(issue("github", "o/r#42"));
        ws.attach_task(issue("github", "o/r#42"));
        assert_eq!(
            ws.gh_issues.len(),
            1,
            "duplicate attaches replace, not append"
        );
    }

    #[test]
    fn detach_removes_from_any_slot() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.attach_task(pr("o/r#1"));
        ws.attach_task(issue("linear", "ENG-7"));
        let pr_id = TaskId {
            source: "github".into(),
            key: "o/r#1".into(),
        };
        let lin_id = TaskId {
            source: "linear".into(),
            key: "ENG-7".into(),
        };
        ws.detach_task(&pr_id);
        ws.detach_task(&lin_id);
        assert!(ws.pr.is_none());
        assert!(ws.linear_issues.is_empty());
    }

    #[test]
    fn merge_activity_dedupes_and_sorts_newest_first() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.merge_activity(&[activity_at(10, "second"), activity_at(0, "first")]);
        ws.merge_activity(&[activity_at(0, "first"), activity_at(20, "third")]);
        assert_eq!(ws.activity.len(), 3);
        assert_eq!(ws.activity[0].body, "third");
        assert_eq!(ws.activity[1].body, "second");
        assert_eq!(ws.activity[2].body, "first");
    }

    /// Regression: read-marks lived as Vec indices and got silently
    /// reattached to whichever activity sorted into that slot after a
    /// new item arrived. Mark the second-newest item read, then have
    /// the poll discover a newer item — the mark should follow the
    /// original content, not stay glued to index 1.
    #[test]
    fn merge_activity_preserves_read_marks_across_resort() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.merge_activity(&[activity_at(20, "third"), activity_at(10, "second")]);
        // Sanity: [third, second] in newest-first order.
        assert_eq!(ws.activity[0].body, "third");
        assert_eq!(ws.activity[1].body, "second");

        // Mark "second" (index 1) read.
        ws.mark_activity_read(1);
        assert!(ws.read_indices.contains(&1));

        // Poll discovers a brand-new activity item. After merge+sort,
        // "fresh" is index 0 → "third" shifts to 1 → "second" to 2.
        ws.merge_activity(&[activity_at(30, "fresh")]);
        assert_eq!(ws.activity[0].body, "fresh");
        assert_eq!(ws.activity[1].body, "third");
        assert_eq!(ws.activity[2].body, "second");

        // The read mark must follow "second" (now at index 2), not
        // stay on index 1 (which would falsely mark "third" read).
        assert!(!ws.read_indices.contains(&0), "fresh inherited a read mark");
        assert!(
            !ws.read_indices.contains(&1),
            "third inherited second's read mark"
        );
        assert!(
            ws.read_indices.contains(&2),
            "second's read mark followed it to its new position"
        );
    }

    fn activity_with_node(seconds: i64, body: &str, node_id: &str) -> Activity {
        let mut a = activity_at(seconds, body);
        a.node_id = Some(node_id.into());
        a
    }

    /// Regression: `seen_count` is positional ("oldest N are seen"),
    /// so merging OLDER items (the lazy PR-details backfill) used to
    /// shift the threshold — already-seen newest items flipped back
    /// to unread while the backfilled items landed inside the seen
    /// tail and never surfaced. After `mark_read_all`, a backfilled
    /// older item must be the ONLY unread one.
    #[test]
    fn merge_activity_backfilled_older_items_after_mark_read_all() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.merge_activity(&[activity_at(10, "a"), activity_at(20, "b")]);
        ws.mark_read_all();
        assert_eq!(ws.unread_count(), 0);

        // Lazy backfill discovers a review comment OLDER than
        // everything in the feed.
        ws.merge_activity(&[activity_at(0, "old-review-comment")]);
        assert_eq!(ws.activity[0].body, "b");
        assert_eq!(ws.activity[1].body, "a");
        assert_eq!(ws.activity[2].body, "old-review-comment");

        assert!(!ws.is_activity_unread(0), "'b' was seen — must stay read");
        assert!(!ws.is_activity_unread(1), "'a' was seen — must stay read");
        assert!(
            ws.is_activity_unread(2),
            "the backfilled item is new content — must surface as unread"
        );
        assert_eq!(ws.unread_count(), 1);
    }

    /// Interleaved ages: seen items on both sides of a backfilled
    /// one. Only the backfilled item is unread; both seen neighbors
    /// keep their read state regardless of position.
    #[test]
    fn merge_activity_interleaved_backfill_keeps_seen_state() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.merge_activity(&[activity_at(0, "oldest"), activity_at(20, "newest")]);
        ws.mark_read_all();

        ws.merge_activity(&[activity_at(10, "middle")]);
        assert_eq!(ws.activity[0].body, "newest");
        assert_eq!(ws.activity[1].body, "middle");
        assert_eq!(ws.activity[2].body, "oldest");

        assert!(!ws.is_activity_unread(0), "newest stays read");
        assert!(ws.is_activity_unread(1), "middle is the new content");
        assert!(!ws.is_activity_unread(2), "oldest stays read");
        assert_eq!(ws.unread_count(), 1);
        assert_eq!(ws.unread_activity_indices(), vec![1]);
    }

    /// Explicit per-item read marks (not mark_read_all) also survive
    /// an older-item backfill.
    #[test]
    fn merge_activity_backfill_preserves_explicit_read_marks() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.merge_activity(&[activity_at(10, "a"), activity_at(20, "b")]);
        // Mark only "a" (index 1) read; "b" stays unread.
        ws.mark_activity_read(1);

        ws.merge_activity(&[activity_at(0, "older")]);
        // [b, a, older]
        assert!(ws.is_activity_unread(0), "'b' was never read");
        assert!(!ws.is_activity_unread(1), "'a' keeps its explicit mark");
        assert!(ws.is_activity_unread(2), "backfilled item is unread");
    }

    /// An edited comment (same node_id, new body) replaces the stored
    /// copy instead of appending a duplicate forever.
    #[test]
    fn merge_activity_edit_replaces_by_node_id_not_duplicates() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.merge_activity(&[activity_with_node(10, "original text", "n1")]);
        assert_eq!(ws.activity.len(), 1);

        ws.merge_activity(&[activity_with_node(10, "edited text", "n1")]);
        assert_eq!(ws.activity.len(), 1, "edit must upsert, not append");
        assert_eq!(ws.activity[0].body, "edited text");
    }

    /// Pinned decision: an edited comment resurfaces as unread (the
    /// content changed, the user should see it), while a re-poll of
    /// the SAME body keeps the read state.
    #[test]
    fn merge_activity_edit_resurfaces_as_unread_unchanged_stays_read() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.merge_activity(&[activity_with_node(10, "v1", "n1")]);
        ws.mark_read_all();
        assert_eq!(ws.unread_count(), 0);

        // Re-poll with identical content → still read.
        ws.merge_activity(&[activity_with_node(10, "v1", "n1")]);
        assert_eq!(ws.unread_count(), 0, "unchanged re-poll keeps read state");

        // Edit arrives → unread again.
        ws.merge_activity(&[activity_with_node(10, "v2", "n1")]);
        assert_eq!(ws.activity.len(), 1);
        assert!(
            ws.is_activity_unread(0),
            "edited comment must resurface as unread"
        );
    }

    /// Node-id-less items keep the (author, body, created_at) tuple
    /// dedup, including against a node-id-carrying twin (tuple match).
    #[test]
    fn merge_activity_tuple_fallback_still_dedupes() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.merge_activity(&[activity_at(10, "same")]);
        ws.merge_activity(&[activity_at(10, "same")]);
        assert_eq!(ws.activity.len(), 1);

        // A later poll attaches a node_id to the same content — the
        // stored item gains it rather than duplicating.
        ws.merge_activity(&[activity_with_node(10, "same", "n9")]);
        assert_eq!(ws.activity.len(), 1);
        assert_eq!(ws.activity[0].node_id.as_deref(), Some("n9"));
    }

    /// The feed is capped at `MAX_ACTIVITY_ITEMS`, dropping oldest.
    #[test]
    fn merge_activity_caps_feed_dropping_oldest() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        let many: Vec<Activity> = (0..(MAX_ACTIVITY_ITEMS as i64 + 50))
            .map(|i| activity_at(i, &format!("item-{i}")))
            .collect();
        ws.merge_activity(&many);
        assert_eq!(ws.activity.len(), MAX_ACTIVITY_ITEMS);
        // Newest survives, oldest dropped.
        assert_eq!(
            ws.activity[0].body,
            format!("item-{}", MAX_ACTIVITY_ITEMS as i64 + 49)
        );
        assert!(ws.activity.iter().all(|a| a.body != "item-0"));
    }

    /// Regression: lowering `seen_count` in `unmark_activity_read`
    /// used to un-read every NEWER item that shared the seen tail.
    /// Only the target index may become unread.
    #[test]
    fn unmark_activity_read_unreads_only_the_target() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.merge_activity(&[
            activity_at(30, "newest"),
            activity_at(20, "middle"),
            activity_at(10, "oldest"),
        ]);
        ws.mark_read_all();
        assert_eq!(ws.unread_count(), 0);

        // Undo the middle item (index 1).
        ws.unmark_activity_read(1);
        assert!(!ws.is_activity_unread(0), "newest must stay read");
        assert!(ws.is_activity_unread(1), "target becomes unread");
        assert!(!ws.is_activity_unread(2), "oldest must stay read");
        assert_eq!(ws.unread_count(), 1);

        // Re-mark and the workspace is fully read again.
        ws.mark_activity_read(1);
        assert_eq!(ws.unread_count(), 0);
    }

    #[test]
    fn linked_task_ids_reports_every_attached_task() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.attach_task(pr("o/r#1"));
        ws.attach_task(issue("github", "o/r#42"));
        ws.attach_task(issue("linear", "ENG-7"));
        let ids = ws.linked_task_ids();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn task_by_id_finds_tasks_in_every_slot() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.attach_task(pr("o/r#1"));
        ws.attach_task(issue("github", "o/r#42"));
        ws.attach_task(issue("linear", "ENG-7"));

        for (source, key) in [
            ("github", "o/r#1"),
            ("github", "o/r#42"),
            ("linear", "ENG-7"),
        ] {
            let id = TaskId {
                source: source.into(),
                key: key.into(),
            };
            assert_eq!(ws.task_by_id(&id).map(|t| &t.id), Some(&id));
        }

        let missing = TaskId {
            source: "github".into(),
            key: "o/r#999".into(),
        };
        assert!(ws.task_by_id(&missing).is_none());
    }

    #[test]
    fn workspace_key_for_a_pr_is_stable_and_filesystem_safe() {
        let task = pr("owner/repo#123");
        let key = workspace_key_for(&task);
        assert!(!key.contains('#'));
        assert!(!key.contains('/'));
        // Same task → same key.
        assert_eq!(workspace_key_for(&task), key);
    }

    #[test]
    fn session_id_is_unique_per_call() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn session_default_name_matches_kind() {
        assert_eq!(
            default_name_for(&SessionKind::Agent {
                agent_id: "claude".into()
            }),
            "claude"
        );
        assert_eq!(default_name_for(&SessionKind::Shell), "shell");
        assert_eq!(
            default_name_for(&SessionKind::Compare {
                source_sessions: vec![]
            }),
            "compare"
        );
        assert_eq!(
            default_name_for(&SessionKind::LogTail {
                path: "build.log".into()
            }),
            "log: build.log"
        );
    }

    /// `add_session` used to blindly push, so a daemon resend or a
    /// buggy caller could leave the workspace with two `Session`s
    /// sharing the same `SessionId`. Guard added in this commit:
    /// adding a session that already exists is a no-op.
    #[test]
    fn add_session_dedupes_by_id() {
        let mut w = Workspace::empty(
            crate::WorkspaceKey::new("github:owner/repo"),
            "main",
            chrono::Utc::now(),
        );
        let id = SessionId::new();
        let key = w.key.clone();
        let make = || Session {
            id,
            workspace_key: key.clone(),
            name: "claude".into(),
            kind: SessionKind::Agent {
                agent_id: "claude".into(),
            },
            state: SessionRunState::Idle,
            worktree_path: "/tmp/wt".into(),
            created_at: chrono::Utc::now(),
            last_output_at: None,
            layout: SessionLayout::default(),
        };
        w.add_session(make());
        w.add_session(make());
        assert_eq!(w.sessions.len(), 1, "second add with same id is a no-op");
    }

    // ── Worktree slug stability ───────────────────────────────────
    //
    // `worktree_slug` decides the on-disk directory name for every
    // worktree lazybox creates. The whole session-recovery model relies
    // on calling the function with the same workspace state +
    // session-index returning the SAME path, otherwise a restart
    // would orphan every existing worktree.
    //
    // These tests pin the parts of the contract callers depend on:
    //   1. Pure function — same input, same output.
    //   2. PR-number prefix is shared across all renames of the same
    //      PR. Lets us locate (and migrate) the on-disk folder when
    //      the title is edited upstream.
    //   3. Unicode/emoji handled without panic.
    //   4. Fallback chain (PR → name → key-suffix) covers every
    //      possible workspace state — `worktree_slug` never returns
    //      an empty string, even on a fully-anonymous workspace.

    fn ws_with_pr(num: u64, title: &str) -> Workspace {
        let mut t = pr(&format!("o/r#{num}"));
        t.title = title.into();
        Workspace::from_task(t, now())
    }

    #[test]
    fn slug_is_deterministic_for_the_same_workspace() {
        // Bedrock contract: lazybox calls `worktree_slug` on every
        // session bring-up. Two calls in a row, no state change in
        // between, must produce identical strings.
        let w = ws_with_pr(7413, "Propagate status code");
        assert_eq!(w.worktree_slug(), w.worktree_slug());
    }

    #[test]
    fn pr_slug_uses_pr_number_prefix() {
        // The `PR-{num}-` prefix is the stable anchor used by the
        // worktree manager for cross-rename lookups (today
        // aspirationally; tomorrow when we wire the migration). Any
        // tweak to `pr_slug` that drops this prefix breaks that
        // recovery path.
        let w = ws_with_pr(7413, "Propagate status code");
        assert!(
            w.worktree_slug().starts_with("PR-7413-"),
            "got {}",
            w.worktree_slug()
        );
    }

    #[test]
    fn pr_rename_keeps_the_pr_number_prefix() {
        // Two workspaces representing the same PR with different
        // titles must share the `PR-{num}-` prefix. The full slug
        // differs (that's the orphan-worktree footgun), but the
        // prefix is the stable shared component the worktree manager
        // can key on.
        let before = ws_with_pr(7413, "Propagate status code").worktree_slug();
        let after = ws_with_pr(7413, "Fix propagation bug").worktree_slug();
        assert_ne!(before, after, "renames change the trailing slug body");
        assert!(before.starts_with("PR-7413-"));
        assert!(after.starts_with("PR-7413-"));
    }

    #[test]
    fn different_prs_with_same_title_produce_distinct_slugs() {
        // Sanity: no collision between sibling PRs that happen to
        // share a title. The PR number is what disambiguates.
        let a = ws_with_pr(1, "Fix bug").worktree_slug();
        let b = ws_with_pr(2, "Fix bug").worktree_slug();
        assert_ne!(a, b);
    }

    #[test]
    fn pr_with_empty_title_falls_back_to_pr_number_only() {
        // Emoji-only or whitespace-only titles must NOT produce a
        // trailing dash (`PR-42-`) — that breaks filesystem hygiene
        // on case-insensitive volumes and looks broken.
        let w = ws_with_pr(42, "🚀");
        let slug = w.worktree_slug();
        assert_eq!(slug, "PR-42");
        assert!(!slug.ends_with('-'));
    }

    #[test]
    fn workspace_with_no_pr_falls_back_to_name_slug() {
        // A pre-PR workspace (user pressed `n` to create a fresh
        // branch) has no `pr` slot. The slug comes from the
        // workspace's name instead, lowercased + dashed.
        let mut w = Workspace::empty(crate::WorkspaceKey::new("github:owner/repo"), "main", now());
        w.name = "Hotfix Auth".into();
        assert_eq!(w.worktree_slug(), "hotfix-auth");
    }

    #[test]
    fn fully_anonymous_workspace_falls_back_to_stable_placeholder() {
        // Workspace with no PR AND a name that slugifies to empty
        // (emoji-only / whitespace) must still produce a non-empty
        // slug so the on-disk path is valid. The key-suffix tail
        // keeps it unique across siblings in the same repo.
        let mut w = Workspace::empty(
            crate::WorkspaceKey::new("github:owner/repo#xyz9"),
            "main",
            now(),
        );
        w.name = "🚀✨".into();
        let slug = w.worktree_slug();
        assert!(
            slug.starts_with("workspace-"),
            "fallback path must start with `workspace-`, got {slug}",
        );
        assert!(!slug.ends_with('-'), "no trailing dash");
        // Stable across calls — the key is fixed.
        assert_eq!(slug, w.worktree_slug());
    }

    #[test]
    fn slug_never_returns_empty() {
        // Hard invariant across the entire fallback chain. Many
        // callers `.join(slug)` directly — an empty slug would
        // produce `<root>/` (i.e., the root itself) and writes
        // would land in the wrong place.
        for w in [
            ws_with_pr(1, "Has title"),
            ws_with_pr(2, "🚀"),
            {
                let mut w =
                    Workspace::empty(crate::WorkspaceKey::new("github:a/b#1"), "main", now());
                w.name = "Named workspace".into();
                w
            },
            {
                let mut w =
                    Workspace::empty(crate::WorkspaceKey::new("github:a/b#xyz"), "main", now());
                w.name = "🚀".into();
                w
            },
        ] {
            assert!(!w.worktree_slug().is_empty());
        }
    }

    #[test]
    fn slug_is_lowercase_and_dash_separated() {
        // Filesystem-portable invariant. Some volumes are case-
        // insensitive (macOS default) and an uppercase slug would
        // collide with a different-cased one. Dashes only — no
        // spaces, no underscores, no other punctuation.
        let w = ws_with_pr(7, "Add Multi-Word Title!");
        let slug = w.worktree_slug();
        for ch in slug.chars() {
            assert!(
                ch.is_ascii_alphanumeric() || ch == '-',
                "{slug} contains non-portable char {ch:?}",
            );
            assert!(
                !ch.is_ascii_uppercase() || slug.starts_with("PR-"),
                "{slug} has uppercase outside the PR- anchor",
            );
        }
    }
}
