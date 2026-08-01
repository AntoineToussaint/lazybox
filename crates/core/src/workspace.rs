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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use uuid::Uuid;

/// Stable identifier for a workspace. Human-readable so it survives
/// renames and shows up well in logs / UIs ("fix-auth-2026-04").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
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
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
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

/// Cap on [`Workspace::sent_snippets`] — enough to re-orient on what
/// you've told an agent without letting the MRU grow the workspace JSON
/// blob unbounded.
pub const SENT_SNIPPETS_MAX: usize = 12;

/// Stable identity of an activity item used to carry read/seen state
/// across the re-sort a merge triggers.
///
/// It is the `(author, body, created_at)` content tuple PLUS an
/// occurrence index. Keeping `body` in the key is deliberate: an edited
/// comment changes its body, so its identity changes and it correctly
/// resurfaces as unread. The occurrence index disambiguates genuinely
/// distinct events that share the *same* tuple — e.g. two identical bot
/// posts landing in the same second. Keying purely on the tuple
/// collapsed such twins onto one identity, so read-state remapped onto a
/// single survivor and the other silently lost its state. Occurrence is
/// stable across re-polls because Rust's `sort_by_key` is a stable sort:
/// equal-`created_at` items keep their relative order.
type ActivityIdentity = (String, String, DateTime<Utc>, usize);

/// Per-index identities for `list`, assigning occurrence indices (in
/// list order) to items that share a content tuple. The returned vector
/// is aligned with `list` by position.
fn activity_identities(list: &[Activity]) -> Vec<ActivityIdentity> {
    let mut seen: HashMap<(String, String, DateTime<Utc>), usize> = HashMap::new();
    list.iter()
        .map(|a| {
            let tuple = (a.author.clone(), a.body.clone(), a.created_at);
            let slot = seen.entry(tuple).or_insert(0);
            let occurrence = *slot;
            *slot += 1;
            (a.author.clone(), a.body.clone(), a.created_at, occurrence)
        })
        .collect()
}

/// Durable state of the "this PR merged / issue closed — clean up the
/// workspace?" prompt. Persisted on the workspace (issue #499) so the
/// decision survives a daemon restart, unlike the old per-process pin.
///
/// The prompt itself is level-triggered off the primary task's terminal
/// state (a merged PR or a closed issue) — this field only records the
/// user's *answer*, so a "keep" doesn't have to be re-derived every
/// launch. Removal deletes the whole row, so there's no "done" state to
/// persist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum CleanupPrompt {
    /// No answer recorded. While the primary task sits in a terminal
    /// state, the removal sweep keeps offering cleanup.
    #[default]
    Unresolved,
    /// The user answered "keep". Never prompt again for this
    /// workspace, across restarts.
    Declined,
}

/// Version stamped into every persisted `Workspace` JSON blob.
///
/// Bump this when a field is renamed, retyped, or otherwise changed in
/// a way that a lenient `#[serde(default)]` read cannot round-trip
/// losslessly. Readers compare a stored row's `schema` against this
/// constant: a row stamped NEWER than the running build must be
/// preserved untouched (never lenient-parsed and rewritten), because a
/// rewrite by an older build silently drops every field it doesn't
/// know about. See [`Workspace::decode_persisted`].
///
/// History:
/// - 0: every record written before the field existed (reads back via
///   `#[serde(default)]`).
/// - 1: the `schema` field itself.
/// - 2: `Session::worktree_branch`.
pub const WORKSPACE_SCHEMA_VERSION: u32 = 2;

/// Serialize hook for [`Workspace::schema`]: always stamp the CURRENT
/// version on save, regardless of what version the row was loaded at.
/// A row we were able to decode and are willing to rewrite is, by
/// definition, now in the running build's shape.
fn stamp_current_schema<S>(_loaded: &u32, s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    s.serialize_u32(WORKSPACE_SCHEMA_VERSION)
}

/// Why a persisted workspace blob was refused by
/// [`Workspace::decode_persisted`]. Either way the caller must treat
/// the stored row as present-but-unreadable: preserve it, never
/// overwrite it with freshly-derived state.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceDecodeError {
    #[error("invalid workspace JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "workspace schema v{found} is newer than this build supports (v{supported}); \
         refusing lenient decode so a downgraded build cannot rewrite the row and \
         erase fields it does not know about"
    )]
    NewerSchema { found: u32, supported: u32 },
}

/// One workspace = one unit of work (PR + linked issues), holding
/// **zero or more sessions**. A session is one folder worktree on
/// disk; without sessions the workspace is purely a tracking row
/// with no on-disk presence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct Workspace {
    /// Persistence schema version this row was LOADED at (0 for
    /// records predating the field). Serialization always stamps
    /// [`WORKSPACE_SCHEMA_VERSION`] via the `stamp_current_schema` hook.
    /// Compared by [`Workspace::decode_persisted`] to refuse rows
    /// written by a newer build.
    #[serde(default, serialize_with = "stamp_current_schema")]
    pub schema: u32,
    pub key: WorkspaceKey,
    /// The Project this workspace lives under (sidebar grouping
    /// header). `None` only during back-compat reads of pre-Project
    /// records; the daemon back-fills via `project_key_from_task`
    /// before broadcasting. Treat absence as "infer from
    /// `primary_task().repo`" everywhere else.
    #[serde(default)]
    pub project_key: Option<crate::ProjectKey>,
    /// `true` when the user created this workspace by hand (the `n`
    /// flow), not from a provider task. Locally-authored workspaces
    /// never appear in a provider poll, so the reconcile sweep must
    /// never prune them — even after they gain a PR/issue. Provider-
    /// derived workspaces leave this `false`.
    #[serde(default)]
    pub local: bool,
    /// When `Some`, this is a **linked (no-worktree) checkout**: the
    /// workspace points directly at an existing clone on disk (a
    /// canonical `~/development/<owner>/<repo>` folder imported via the
    /// dev-folder scan) rather than a lazybox-provisioned worktree.
    /// Every session spawns straight into this path on whatever branch
    /// it already sits on — lazybox never provisions a worktree, never
    /// bare-clones, and never switches the branch. Always paired with
    /// `local = true` so the reconcile sweep can't prune it. `None` for
    /// every ordinary workspace, whose sessions get isolated worktrees.
    #[serde(default)]
    pub linked_checkout: Option<PathBuf>,
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
    /// Per-workspace "auto-merge on green" arm. When `true`, the
    /// **daemon's** polling commit path auto-fires a merge the moment
    /// this workspace's own PR becomes merge-ready (green CI, no
    /// conflict, no changes requested — see
    /// [`crate::should_auto_merge`]). User-toggled, persisted in the
    /// workspace JSON blob alongside [`Workspace::snoozed_until`].
    /// Distinct from the PR's `Task::auto_merge_enabled` — that's
    /// GitHub's native server-side "merge when ready"; this is
    /// lazybox's arm that only acts while the lazybox daemon runs.
    #[serde(default)]
    pub auto_merge_on_green: bool,
    /// Per-workspace "track main" arm (issue #535). When `true`, the
    /// daemon's background sweep keeps this workspace's worktree
    /// fast-forwarded to `origin/<base_branch>` whenever the tree is
    /// clean, so a persistent scratch workspace ("Issue" / "Work") stays
    /// based on the default branch without the user prompting a rebase
    /// every session. User-toggled, persisted in the workspace JSON blob
    /// alongside [`Workspace::auto_merge_on_green`]. The sync is always
    /// fast-forward-only: a dirty or diverged tree is skipped, never
    /// reset, so in-progress work is never destroyed.
    #[serde(default)]
    pub track_main: bool,
    /// The resolved default branch this workspace is based on
    /// (`main` / `master` / …), persisted so "track main" doesn't
    /// re-derive it every sweep and so the exact branch survives a
    /// restart. Populated lazily by the sweep (or the toggle handler)
    /// the first time it resolves the repo's default branch. `None`
    /// until then, and on workspaces that never opt into tracking.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Last sweep verdict for a [`Workspace::track_main`] workspace:
    /// `true` when the worktree is behind `origin/<base_branch>` but
    /// couldn't be fast-forwarded automatically (uncommitted changes or
    /// a diverged history). Drives the sidebar's "behind" badge and is
    /// persisted so the state survives a restart instead of flashing
    /// wrong until the next sweep. Always `false` when not tracking.
    #[serde(default)]
    pub track_main_behind: bool,
    /// Unified per-session automation policies (issue #363) — today the
    /// per-workspace auto-fix arm/disarm overrides. merge-on-green stays
    /// in [`Workspace::auto_merge_on_green`] above for back-compat but is
    /// presented in the same policies surface. Serde-defaulted so
    /// pre-#363 records read back as all-`Default` and behave unchanged.
    #[serde(default)]
    pub policies: crate::AutomationPolicies,
    /// Free-form local scratchpad the user attaches to this workspace
    /// (issue #458). Purely a lazybox concept — never synced to a
    /// provider. Persisted in the workspace JSON blob alongside the
    /// other user-owned fields above, so providers overwrite only
    /// upstream-derived state and leave this intact across polls.
    #[serde(default)]
    pub notes: String,
    /// MRU of snippet shortcut keys sent to this workspace's agent(s)
    /// (issue #463) — a per-session record of "what I've already told
    /// this agent" so switching back is cheap. Most-recent first,
    /// de-duplicated (a re-send moves the key to the front), capped at
    /// [`SENT_SNIPPETS_MAX`]. Persisted in the workspace JSON blob
    /// alongside [`Workspace::notes`]; never synced to any provider.
    #[serde(default)]
    pub sent_snippets: Vec<String>,
    /// Durable answer to the merged/closed cleanup prompt (issue #499).
    /// Serde-defaulted so pre-#499 records read back as `Unresolved`.
    #[serde(default)]
    pub cleanup_prompt: CleanupPrompt,
    pub created_at: DateTime<Utc>,
    pub last_viewed_at: Option<DateTime<Utc>>,
}

impl Workspace {
    /// Empty workspace on `branch` with no linked tasks. Used for the
    /// "create a workspace from scratch" path.
    pub fn empty(key: WorkspaceKey, branch: impl Into<String>, now: DateTime<Utc>) -> Self {
        let branch = branch.into();
        Self {
            schema: WORKSPACE_SCHEMA_VERSION,
            name: key.as_str().to_string(),
            key,
            project_key: None,
            local: false,
            linked_checkout: None,
            branch,
            sessions: Vec::new(),
            pr: None,
            gh_issues: Vec::new(),
            linear_issues: Vec::new(),
            activity: Vec::new(),
            seen_count: 0,
            read_indices: HashSet::new(),
            snoozed_until: None,
            auto_merge_on_green: false,
            track_main: false,
            base_branch: None,
            track_main_behind: false,
            policies: crate::AutomationPolicies::default(),
            notes: String::new(),
            sent_snippets: Vec::new(),
            cleanup_prompt: CleanupPrompt::default(),
            created_at: now,
            last_viewed_at: None,
        }
    }

    /// Decode a persisted workspace JSON blob, refusing rows this
    /// build cannot round-trip losslessly.
    ///
    /// Two failure modes, both meaning "the row is present but
    /// unreadable — preserve it, do not overwrite it":
    /// - the JSON doesn't parse (corruption, or a shape change a
    ///   lenient serde read chokes on);
    /// - the row's [`Workspace::schema`] stamp is newer than
    ///   [`WORKSPACE_SCHEMA_VERSION`]. Lenient serde would *parse* such
    ///   a row fine (unknown fields are ignored), but any subsequent
    ///   save would rewrite it minus the newer build's fields — the
    ///   downgrade-erases-data hole. Refusing here routes the row into
    ///   the same preserve-and-report machinery as corruption.
    ///
    /// Every load that can lead to a save of the same row must go
    /// through this instead of a bare `serde_json::from_str`.
    pub fn decode_persisted(json: &str) -> Result<Self, WorkspaceDecodeError> {
        let ws: Self = serde_json::from_str(json)?;
        if ws.schema > WORKSPACE_SCHEMA_VERSION {
            return Err(WorkspaceDecodeError::NewerSchema {
                found: ws.schema,
                supported: WORKSPACE_SCHEMA_VERSION,
            });
        }
        Ok(ws)
    }

    /// Whether this workspace carries a non-empty local note. Drives the
    /// sidebar's notes indicator; whitespace-only notes don't count.
    pub fn has_notes(&self) -> bool {
        !self.notes.trim().is_empty()
    }

    /// Whether "track main" (issue #535) can apply to this workspace.
    /// Requirements:
    /// - a GitHub upstream, to resolve a default branch and fetch against;
    /// - a lazybox-provisioned worktree — a linked checkout already sits
    ///   on the user's own branch in their own clone, which lazybox never
    ///   fast-forwards for them;
    /// - **no PR**. Track-main is for persistent scratch / branchless
    ///   workspaces cut off `main`. A PR branch carries its own commits,
    ///   so it's simultaneously ahead of and behind `main` — a
    ///   fast-forward can never apply, and offering the toggle there would
    ///   only paint a permanent, misleading "behind" badge on the PR row.
    pub fn supports_track_main(&self) -> bool {
        self.pr.is_none()
            && !self.is_linked()
            && workspace_project_key(self).is_some_and(|k| k.source_prefix() == "github")
    }

    /// Record `key` as just-sent to this workspace's agent: move it to
    /// the front of [`Workspace::sent_snippets`], de-duplicating and
    /// capping at [`SENT_SNIPPETS_MAX`]. Mirrors the global picker MRU
    /// but scoped per workspace, so the sidebar can show what you've
    /// already told each agent (issue #463).
    pub fn record_sent_snippet(&mut self, key: String) {
        self.sent_snippets.retain(|k| k != &key);
        self.sent_snippets.insert(0, key);
        self.sent_snippets.truncate(SENT_SNIPPETS_MAX);
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

    /// `true` when this workspace is a linked (no-worktree) checkout —
    /// its sessions run in the user's existing clone on disk, not in a
    /// lazybox-provisioned worktree. See [`Workspace::linked_checkout`].
    pub fn is_linked(&self) -> bool {
        self.linked_checkout.is_some()
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
        // Snapshot read + seen state by stable identity BEFORE we
        // mutate. Identities are occurrence-aware so two distinct
        // node-id-less twins never share a key.
        let identities = activity_identities(&self.activity);
        let read_keys: HashSet<ActivityIdentity> = self
            .read_indices
            .iter()
            .filter_map(|i| identities.get(*i).cloned())
            .collect();
        let seen_start = self.activity.len().saturating_sub(self.seen_count);
        let seen_keys: HashSet<ActivityIdentity> = identities
            .get(seen_start..)
            .unwrap_or(&[])
            .iter()
            .cloned()
            .collect();

        // Occurrence counter for the tuple fallback: the k-th incoming
        // item with a given content tuple maps to the k-th stored item
        // with that tuple, so a batch of identical-tuple events doesn't
        // all fold onto the first match — genuinely distinct twins both
        // survive instead of collapsing into one.
        let mut claimed: HashMap<(String, String, DateTime<Utc>), usize> = HashMap::new();
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
            // that gained a node_id migrates it onto the stored item —
            // but claim stored occurrences in order so distinct twins
            // don't all target the same slot.
            let tuple = (act.author.clone(), act.body.clone(), act.created_at);
            let skip = claimed.get(&tuple).copied().unwrap_or(0);
            let target = self
                .activity
                .iter()
                .enumerate()
                .filter(|(_, a)| {
                    a.author == act.author && a.body == act.body && a.created_at == act.created_at
                })
                .map(|(i, _)| i)
                .nth(skip);
            match target {
                Some(i) => self.activity[i] = act.clone(),
                None => self.activity.push(act.clone()),
            }
            *claimed.entry(tuple).or_insert(0) += 1;
        }
        self.sort_activity();
        // Sorted newest-first, so truncation drops the oldest items.
        self.activity.truncate(MAX_ACTIVITY_ITEMS);

        // Reconstruct seen/read by identity. `seen_count` becomes the
        // longest suffix (= oldest run) of previously-seen items;
        // everything read-or-seen above that suffix gets an explicit
        // read mark instead.
        let identities = activity_identities(&self.activity);
        let mut suffix = 0usize;
        for id in identities.iter().rev() {
            if seen_keys.contains(id) {
                suffix += 1;
            } else {
                break;
            }
        }
        self.seen_count = suffix;
        let cut = self.activity.len() - suffix;
        self.read_indices = identities
            .get(..cut)
            .unwrap_or(&[])
            .iter()
            .enumerate()
            .filter_map(|(i, id)| (read_keys.contains(id) || seen_keys.contains(id)).then_some(i))
            .collect();
    }

    /// Absorb another workspace's activity feed *and its read/seen
    /// state*. Used by the issue→PR collapse: the folded issue's
    /// comment history must survive on the PR workspace, and the rows
    /// the user already read there must not resurface as unread
    /// (docs/resiliency-review.md flagged the collapse as dropping
    /// both).
    ///
    /// Built on [`Self::merge_activity`], which already preserves
    /// *this* workspace's read/seen marks content-wise across the
    /// merge; this method additionally snapshots `other`'s read and
    /// seen identity keys up front and, after the merge, flips every
    /// merged row carrying one of those identities to read. Rows that
    /// were unread in `other` stay unread here. `other` is untouched
    /// — the caller deletes its row separately.
    pub fn absorb_activity_from(&mut self, other: &Workspace) {
        if other.activity.is_empty() {
            return;
        }
        // Snapshot the absorbed workspace's read + seen state by the
        // same occurrence-aware identity `merge_activity` uses, so
        // remapping follows identical rules on both sides of the
        // merge (including the twin/occurrence disambiguation).
        let other_ids = activity_identities(&other.activity);
        let mut carried: HashSet<ActivityIdentity> = other
            .read_indices
            .iter()
            .filter_map(|i| other_ids.get(*i).cloned())
            .collect();
        let seen_start = other.activity.len().saturating_sub(other.seen_count);
        carried.extend(other_ids.get(seen_start..).unwrap_or(&[]).iter().cloned());

        self.merge_activity(&other.activity);

        // Re-derive identities on the merged list and mark read every
        // row the absorbed workspace had read-or-seen. Rows already
        // inside this workspace's seen suffix are implicitly read and
        // need no explicit mark.
        let ids = activity_identities(&self.activity);
        let cut = self.activity.len() - self.seen_count;
        for (i, id) in ids.iter().take(cut).enumerate() {
            if carried.contains(id) {
                self.read_indices.insert(i);
            }
        }
    }

    /// Fold every **user-owned** field of `other` into this workspace,
    /// each by an explicit merge rule. The single source of truth for
    /// what survives when a session moves between workspaces — the
    /// issue→PR collapse and the `Shift-A` adopt both route through here
    /// (issue #554), so the two flows can't diverge and neither silently
    /// drops state the way the old hand-copied allowlist did.
    ///
    /// The `match`-style destructure is load-bearing: adding **any** field
    /// to [`Workspace`] is a compile error here until the field is
    /// classified — given a merge rule (user-owned state) or bound to `_`
    /// (identity, structure, or provider-derived state that is re-synced,
    /// not transferred). New session state therefore transfers
    /// automatically, or forces a decision; it can no longer be lost by
    /// omission.
    ///
    /// Out of scope, by design: **terminal-keyed** state (prompt history,
    /// composing draft, no-perm flag, PTY generation) already transfers
    /// automatically. It's keyed by the terminal's `backend_key` — a stable
    /// tmux identity that doesn't change on a move — and centralized behind
    /// `TerminalPersistedField::ALL` in the server. Nothing here touches it.
    ///
    /// Activity + read/seen state has its own carrier,
    /// [`Self::absorb_activity_from`] (it remaps read marks content-wise),
    /// so `activity`/`seen_count`/`read_indices` are intentionally ignored
    /// below — the caller invokes both. `other` is left untouched; the
    /// caller decides whether to keep or delete its row.
    ///
    /// Several arms are **conditional on the destination**, not a blind
    /// OR: a flag that only means something on a particular kind of
    /// workspace (track-main on a non-PR worktree, merge-on-green on a
    /// PR) is carried only where it applies, and a snooze never newly
    /// *hides* a destination the user could see. Every rule fails toward
    /// "visible / not silently automated" so a transfer can neither lose
    /// state nor introduce a surprise the source's arm didn't have.
    pub fn absorb_user_state_from(&mut self, other: &Workspace) {
        let Workspace {
            // ── identity & structure: belong to the destination row ──
            schema: _,
            key: _,
            project_key: _,
            local: _,
            linked_checkout: _,
            name: _,
            branch: _,
            // Sessions are moved by the caller — a move needs the terminal
            // rebadge plan committed in the same transaction, which lives
            // in the server, not here.
            sessions: _,
            created_at: _,
            // ── provider-derived: re-synced from tasks, never hand-copied ──
            pr: _,
            gh_issues: _,
            linear_issues: _,
            // Carried by `absorb_activity_from` (remaps read/seen too).
            activity: _,
            seen_count: _,
            read_indices: _,
            // The cleanup-prompt answer is a per-workspace *lifecycle*
            // decision, not portable content: a "keep" the user made on
            // the source must not silently suppress the destination's own
            // merged/closed cleanup prompt. The destination keeps its own.
            cleanup_prompt: _,
            // ── user-owned state: one explicit merge rule each ──
            snoozed_until,
            auto_merge_on_green,
            track_main,
            base_branch,
            track_main_behind,
            policies,
            notes,
            sent_snippets,
            last_viewed_at,
        } = other;

        // Snooze: extend a hide the destination already has (take the
        // later deadline), but never newly hide a *visible* destination —
        // an issue→PR collapse must not make a PR the user needs vanish
        // just because the folded issue was snoozed.
        if self.snoozed_until.is_some() {
            self.snoozed_until = later_opt(self.snoozed_until, *snoozed_until);
        }
        // merge-on-green is a consequential daemon arm; carry it only
        // where there's actually a PR to merge. Mirrors the UI, which
        // refuses to arm it on a PR-less workspace, so a stray arm can't
        // ride a transfer onto something it could never fire on.
        if self.pr.is_some() {
            self.auto_merge_on_green |= *auto_merge_on_green;
        }
        // Track-main applies only to a non-PR, non-linked GitHub worktree
        // (`supports_track_main`). Carried onto anything else — e.g. the
        // PR after an issue collapse — it paints a permanent, misleading
        // "behind" badge the sweep (which skips ineligible rows) never
        // clears. Its `base_branch` rides the same eligibility.
        if self.supports_track_main() {
            self.track_main |= *track_main;
            self.track_main_behind |= *track_main_behind;
            if self.base_branch.is_none() {
                self.base_branch = base_branch.clone();
            }
        }
        // Policies: fold per arm, most-decisive choice wins.
        self.policies.absorb_from(policies);
        // Notes: concatenate so neither scratchpad is lost.
        self.notes = merge_notes(&self.notes, notes);
        // Snippet MRU: union, destination recency first, capped.
        self.sent_snippets = merge_sent_snippets(&self.sent_snippets, sent_snippets);
        // Last viewed: the more recent view of either row.
        self.last_viewed_at = later_opt(self.last_viewed_at, *last_viewed_at);
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
        // below `index` also unmarks every newer item that shared
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
    ///   words so it stays scannable). The number disambiguates
    ///   same-titled PRs.
    /// - No PR but an upstream task (issue / ticket) →
    ///   `issue-{number}-{slug-of-name}` (or `{slug-of-task-key}-…`
    ///   for numberless sources like Linear). Title alone used to be
    ///   the whole slug, so two issues titled "Bump dependencies"
    ///   silently shared one checkout — and one branch.
    /// - No task but a custom workspace name → slug of `name`; when
    ///   the workspace key was allocated from that same name
    ///   (`<slug>` / `<slug>-2` / …, see the daemon's
    ///   `allocate_workspace_key`) the key itself is used, so two
    ///   same-named scratch workspaces get distinct directories while
    ///   the first keeps its legacy `<slug>` path.
    /// - All empty → fall back to a stable `workspace_{key-suffix}`
    ///   placeholder so the path is always non-empty.
    ///
    /// Back-compat: a session's `worktree_path` is persisted at
    /// creation and reused in place when the directory is a live
    /// worktree (see the server's `migrate_session_paths_if_needed`),
    /// so a slug-scheme change only affects worktrees provisioned
    /// after it — existing checkouts keep resolving.
    pub fn worktree_slug(&self) -> String {
        if let Some(pr) = self.pr.as_ref()
            && let Some((_, num_str)) = pr.id.key.rsplit_once('#')
            && let Ok(num) = num_str.parse::<u64>()
        {
            return crate::slug::pr_slug(num, &pr.title);
        }
        let name_slug = crate::slug::slugify(&self.name);
        // Non-PR workspace anchored to an upstream task: prefix the
        // task's stable discriminator, mirroring branch naming
        // (`issue-42-bump-dependencies`).
        if self.pr.is_none()
            && let Some(task) = self.primary_task()
        {
            let number = task
                .id
                .key
                .rsplit_once('#')
                .map(|(_, n)| n)
                .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
            let stem = match number {
                Some(n) => Some(format!("issue-{n}")),
                // Numberless task keys (Linear `ENG-456`, …) slugify
                // to a stable discriminator of their own.
                None => {
                    let key_slug = crate::slug::slugify(&task.id.key);
                    (!key_slug.is_empty()).then_some(key_slug)
                }
            };
            if let Some(stem) = stem {
                return if name_slug.is_empty() {
                    stem
                } else {
                    format!("{stem}-{name_slug}")
                };
            }
        }
        if !name_slug.is_empty() {
            // A key allocated from this same name is the collision-free
            // form of the name slug (`bump-deps`, `bump-deps-2`, …):
            // use it directly. Identical to the name slug for the first
            // workspace of a name — the legacy path — and suffixed for
            // later same-named siblings, which used to collide. Keys
            // not derived from the name (task-keyed records that lost
            // their task, sandbox keys) keep the plain name slug.
            let key = self.key.as_str();
            let name_derived_key = key == name_slug
                || key
                    .strip_prefix(&format!("{name_slug}-"))
                    .is_some_and(|rest| {
                        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
                    });
            if name_derived_key {
                return key.to_string();
            }
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

    /// Repo/project qualifier prepended to the on-disk worktree path so
    /// two workspaces that slug to the same name in different repos
    /// can't land in one directory. Derived from the stable project key
    /// (recovered from the primary task's repo for back-compat records
    /// that predate projects), so it survives renames and branch
    /// changes — neither touches the project a workspace lives under.
    ///
    /// `None` only for a fully repo-less, project-less workspace, which
    /// keeps the legacy flat `<root>/<slug>` path.
    pub fn worktree_scope(&self) -> Option<String> {
        let key = self
            .project_key
            .clone()
            .or_else(|| self.primary_task().and_then(project_key_for_task))?;
        let slug = crate::slug::slugify(key.as_str());
        (!slug.is_empty()).then_some(slug)
    }

    /// Best-effort `owner/repo` this workspace belongs to. Prefers the
    /// primary task's repo (authoritative when a PR/issue is attached),
    /// and falls back to the project key's GitHub slug so a task-less
    /// workspace (a hand-created or freshly-provisioned one) still
    /// reports its repo. `None` for a repo-less/local/Linear workspace,
    /// or a GitHub key whose owner-or-repo boundary is ambiguous
    /// ([`crate::ProjectKey::unambiguous_github_slug`]).
    pub fn repo_slug(&self) -> Option<String> {
        if let Some(repo) = self.primary_task().and_then(|task| task.repo.clone()) {
            return Some(repo);
        }
        workspace_project_key(self).and_then(|key| key.unambiguous_github_slug())
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

fn upsert_by_id(list: &mut Vec<Task>, mut task: Task) {
    if let Some(slot) = list.iter_mut().find(|t| t.id == task.id) {
        // Same #512 guard as `preserve_lazy_pr_fields`: never let an
        // untyped re-poll clobber a stored typed `kind`.
        if task.kind.is_none() {
            task.kind = slot.kind;
        }
        *slot = task;
    } else {
        list.push(task);
    }
}

/// Preserve fields whose canonical value can be absent from a given
/// inbox-scan response — today just `checks` (from
/// `statusCheckRollup.contexts`, which the inbox query drops
/// entirely). It's also populated by the lazy `PR_DETAILS_QUERY`;
/// without this preservation, a poll carrying the empty value would
/// clobber the stored one and the per-check sidebar would flicker off.
///
/// `closes_issues` is deliberately NOT preserved: every PR-producing
/// query (inbox scan, single-PR, hot-tasks, lazy details) selects
/// `closingIssuesReferences`, so an incoming empty list means the PR
/// closes no issue — not "not fetched". Preserving the stored value on
/// empty kept a stale `closes_issues` alive after a PR dropped its
/// `Closes #N`, re-firing the issue→PR collapse on every poll (#581).
///
/// Rule: incoming wins for any field it has populated; existing
/// wins only for the listed lazy fields when incoming is empty.
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
    if incoming.checks.is_empty() && !existing.checks.is_empty() {
        incoming.checks = existing.checks.clone();
    }
    if incoming.mergeable == crate::Mergeable::Unknown
        && existing.mergeable != crate::Mergeable::Unknown
    {
        incoming.mergeable = existing.mergeable;
        incoming.is_behind_base = existing.is_behind_base;
    }
    // Once a provider has typed this task's kind, a later poll that
    // arrives untyped (`None`) must not erase it — that would revive
    // the URL-heuristic ambiguity #512 removed. Incoming still wins
    // whenever it carries its own typed kind.
    if incoming.kind.is_none() {
        incoming.kind = existing.kind;
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

/// The later of two optional timestamps (issue #554 user-state merge).
/// `None` is treated as "no value", so a present timestamp always wins
/// over an absent one.
fn later_opt(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Merge two workspace notes losslessly (issue #554): an empty side
/// contributes nothing, identical notes stay single, otherwise both are
/// kept, destination first, separated by a blank line. Concatenating
/// rather than picking a winner means a move never discards a scratchpad.
fn merge_notes(dst: &str, src: &str) -> String {
    match (dst.trim().is_empty(), src.trim().is_empty()) {
        (true, _) => src.to_string(),
        (_, true) => dst.to_string(),
        _ if dst == src => dst.to_string(),
        _ => format!("{dst}\n\n{src}"),
    }
}

/// Union two sent-snippet MRUs (issue #554), destination recency first,
/// de-duplicated and capped at [`SENT_SNIPPETS_MAX`]. The `]N` badge on
/// the surviving workspace then reflects everything told to either
/// session's agent rather than resetting on a move.
fn merge_sent_snippets(dst: &[String], src: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(dst.len() + src.len());
    for key in dst.iter().chain(src.iter()) {
        if !out.iter().any(|k| k == key) {
            out.push(key.clone());
        }
    }
    out.truncate(SENT_SNIPPETS_MAX);
    out
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
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
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
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
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
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
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
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
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

/// Direction for spatial navigation between tiles (the TUI's
/// `]]<arrow>` chords).
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
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
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
    /// Branch this managed worktree was provisioned on. Persisted with the
    /// session because workspace metadata can change independently during
    /// issue-to-PR transfer. `None` for legacy and externally supplied
    /// sessions whose branch was never recorded.
    #[serde(default)]
    pub worktree_branch: Option<String>,
    pub created_at: DateTime<Utc>,
    /// When the daemon last saw output from this session's PTY. None
    /// for compare/log sessions whose state model is different.
    #[serde(default)]
    pub last_output_at: Option<DateTime<Utc>>,
    /// Tile/tab arrangement for this session. Defaults to Tabs.
    /// Persisted so the user's layout survives restart.
    #[serde(default)]
    pub layout: SessionLayout,
    /// Exact upstream conversation identity per agent running in this
    /// worktree.
    #[serde(default)]
    pub provider_session_ids: BTreeMap<String, String>,
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
            worktree_branch: None,
            created_at: now,
            last_output_at: None,
            layout: SessionLayout::default(),
            provider_session_ids: BTreeMap::new(),
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
            created_at: None,
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
            kind: None,
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
    fn repo_slug_prefers_task_repo() {
        let ws = Workspace::from_task(pr("o/r#1"), now());
        assert_eq!(ws.repo_slug().as_deref(), Some("o/r"));
    }

    #[test]
    fn repo_slug_falls_back_to_project_key_when_task_less() {
        // A hand-created workspace with no PR/issue still knows its repo
        // through the project key.
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", now());
        ws.project_key = Some(crate::ProjectKey::github("owner", "repo"));
        assert!(ws.primary_task().is_none());
        assert_eq!(ws.repo_slug().as_deref(), Some("owner/repo"));
    }

    #[test]
    fn repo_slug_is_none_for_a_repo_less_workspace() {
        let ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", now());
        assert_eq!(ws.repo_slug(), None);
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
    fn classify_prefers_typed_kind_over_url() {
        // #512: a PR the provider tagged `TaskKind::Pr` routes to the PR
        // slot even with an empty/API URL the heuristic would misread as
        // an issue.
        let mut t = pr("o/r#1");
        t.url = String::new();
        t.kind = Some(crate::TaskKind::Pr);
        let ws = Workspace::from_task(t, now());
        assert!(ws.pr.is_some(), "typed Pr routes to the PR slot sans URL");

        // Conversely, an issue tagged `TaskKind::Issue` whose URL
        // happens to contain `/pull/` stays out of the PR slot.
        let mut t = issue("github", "o/r#7");
        t.url = "https://github.com/o/r/pull/7".into();
        t.kind = Some(crate::TaskKind::Issue);
        let ws = Workspace::from_task(t, now());
        assert!(ws.pr.is_none(), "typed Issue is not routed to the PR slot");
        assert_eq!(ws.gh_issues.len(), 1);
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

    /// Regression (#512): once a provider has typed a task's `kind`, an
    /// untyped re-poll (`kind: None`) must NOT clobber it back to `None`
    /// — that would revive the URL-heuristic ambiguity this PR removed.
    /// Covers both merge paths: PRs through `preserve_lazy_pr_fields`,
    /// issues through `upsert_by_id`. A re-poll carrying its own typed
    /// kind still wins.
    #[test]
    fn attach_preserves_typed_kind_against_untyped_repoll() {
        // PR slot: first poll typed, second poll untyped.
        let mut first = pr("o/r#1");
        first.kind = Some(crate::TaskKind::Pr);
        let mut ws = Workspace::from_task(first, now());
        let mut untyped = pr("o/r#1");
        untyped.url = String::new();
        untyped.kind = None;
        ws.attach_task(untyped);
        assert_eq!(
            ws.pr.as_ref().unwrap().kind,
            Some(crate::TaskKind::Pr),
            "untyped re-poll must not erase the stored PR kind",
        );
        assert!(ws.pr.is_some(), "PR stays in the PR slot across the merge");

        // A re-poll that IS typed still wins (here: same Pr, but proves
        // incoming typing is honored rather than blindly kept).
        let mut retyped = pr("o/r#1");
        retyped.kind = Some(crate::TaskKind::Pr);
        ws.attach_task(retyped);
        assert_eq!(ws.pr.as_ref().unwrap().kind, Some(crate::TaskKind::Pr));

        // Issue slot (upsert_by_id): first typed, second untyped.
        let mut first_issue = issue("github", "o/r#9");
        first_issue.kind = Some(crate::TaskKind::Issue);
        let mut ws = Workspace::from_task(first_issue, now());
        let mut untyped_issue = issue("github", "o/r#9");
        untyped_issue.kind = None;
        ws.attach_task(untyped_issue);
        assert_eq!(
            ws.gh_issues[0].kind,
            Some(crate::TaskKind::Issue),
            "untyped re-poll must not erase the stored issue kind",
        );
    }

    /// Regression for the GraphQL trim: the inbox-scan query does not
    /// fetch `statusCheckRollup.contexts`, so the incoming PR's `checks`
    /// can arrive empty. Without preservation, such a poll cycle would
    /// wipe out the stored value and the per-check sidebar would flicker
    /// off.
    #[test]
    fn attach_pr_preserves_checks_when_incoming_is_empty() {
        let mut first = pr("o/r#1");
        first.checks = vec![crate::CheckRun {
            name: "lint".into(),
            status: CiStatus::Success,
            url: None,
        }];
        let mut ws = Workspace::from_task(first, now());

        // Subsequent poll: same PR id, empty checks (inbox-scan shape).
        let next = pr("o/r#1");
        ws.attach_task(next);

        let pr_ref = ws.pr.as_ref().unwrap();
        assert_eq!(
            pr_ref.checks.len(),
            1,
            "checks must survive an inbox-scan-shaped re-poll",
        );
        assert_eq!(pr_ref.checks[0].name, "lint");
    }

    /// Regression for #581: every PR-producing query selects
    /// `closingIssuesReferences`, so an incoming empty `closes_issues`
    /// means the PR closes no issue — not "not fetched". A PR that drops
    /// its `Closes #N` must clear the stored link; preserving it kept the
    /// issue→PR collapse re-firing on every poll.
    #[test]
    fn attach_pr_clears_closes_issues_when_incoming_is_empty() {
        let mut first = pr("o/r#1");
        first.closes_issues = vec![TaskId {
            source: "github".into(),
            key: "o/r#42".into(),
        }];
        let mut ws = Workspace::from_task(first, now());

        // Subsequent poll: PR removed its closing reference.
        let next = pr("o/r#1");
        ws.attach_task(next);

        assert!(
            ws.pr.as_ref().unwrap().closes_issues.is_empty(),
            "a removed closing reference must clear the stored closes_issues",
        );
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

    /// Regression (#512): two genuinely distinct node-id-less events
    /// sharing the same (author, body, created_at) tuple — e.g. two
    /// identical bot posts in the same second — must BOTH survive when
    /// they arrive in one poll batch. The old content-as-identity dedup
    /// collapsed them into one and the second silently vanished.
    #[test]
    fn merge_activity_keeps_distinct_same_second_twins() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.merge_activity(&[activity_at(10, "beep boop"), activity_at(10, "beep boop")]);
        assert_eq!(
            ws.activity.len(),
            2,
            "two identical-tuple events in one batch stay distinct",
        );

        // Re-polling the same batch must NOT duplicate them — the k-th
        // incoming twin claims the k-th stored twin.
        ws.merge_activity(&[activity_at(10, "beep boop"), activity_at(10, "beep boop")]);
        assert_eq!(
            ws.activity.len(),
            2,
            "a re-poll of identical twins upserts in place, no growth",
        );
    }

    /// Regression (#512): read-state must track the correct twin. Mark
    /// exactly one of two identical-tuple events read; a re-poll must
    /// keep exactly one read and one unread — not remap the mark onto
    /// the wrong occurrence or collapse both.
    #[test]
    fn merge_activity_twins_keep_independent_read_state() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.merge_activity(&[activity_at(10, "dup"), activity_at(10, "dup")]);
        assert_eq!(ws.activity.len(), 2);

        // Mark one twin read, leave the other unread.
        ws.mark_activity_read(0);
        assert_eq!(ws.unread_count(), 1);

        // Re-poll the identical batch — the read mark must follow its
        // occurrence, leaving exactly one unread.
        ws.merge_activity(&[activity_at(10, "dup"), activity_at(10, "dup")]);
        assert_eq!(ws.activity.len(), 2, "no duplication on re-poll");
        assert_eq!(
            ws.unread_count(),
            1,
            "exactly one twin stays unread across the re-poll",
        );
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
    fn is_linked_reflects_linked_checkout_and_survives_serde() {
        let mut ws = Workspace::empty(WorkspaceKey::new("acme-widget"), "feature", now());
        assert!(!ws.is_linked(), "a plain workspace is not linked");
        ws.linked_checkout = Some(std::path::PathBuf::from("/home/dev/code/acme/widget"));
        assert!(ws.is_linked());

        // Round-trips through JSON (the store's persistence format).
        let json = serde_json::to_string(&ws).unwrap();
        let back: Workspace = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.linked_checkout,
            Some(std::path::PathBuf::from("/home/dev/code/acme/widget"))
        );

        // Back-compat: a pre-feature record with no `linked_checkout`
        // field deserializes to `None` (not an error).
        let legacy = r#"{"key":"old","name":"old","branch":"main","pr":null,
            "gh_issues":[],"linear_issues":[],"activity":[],"seen_count":0,
            "created_at":"2024-01-01T00:00:00Z","last_viewed_at":null}"#;
        let old: Workspace = serde_json::from_str(legacy).unwrap();
        assert!(!old.is_linked());
    }

    #[test]
    fn track_main_state_survives_serde_round_trip() {
        let mut ws = Workspace::empty(WorkspaceKey::new("acme-widget"), "scratch", now());
        assert!(!ws.track_main, "a fresh workspace does not track main");
        assert_eq!(ws.base_branch, None);
        assert!(!ws.track_main_behind);

        ws.track_main = true;
        ws.base_branch = Some("main".to_string());
        ws.track_main_behind = true;

        let json = serde_json::to_string(&ws).unwrap();
        let back: Workspace = serde_json::from_str(&json).unwrap();
        assert!(back.track_main);
        assert_eq!(back.base_branch.as_deref(), Some("main"));
        assert!(back.track_main_behind);
    }

    #[test]
    fn track_main_fields_default_on_legacy_records() {
        // A pre-#535 record with none of the tracking fields deserializes
        // to the disarmed defaults, not an error.
        let legacy = r#"{"key":"old","name":"old","branch":"main","pr":null,
            "gh_issues":[],"linear_issues":[],"activity":[],"seen_count":0,
            "created_at":"2024-01-01T00:00:00Z","last_viewed_at":null}"#;
        let old: Workspace = serde_json::from_str(legacy).unwrap();
        assert!(!old.track_main);
        assert_eq!(old.base_branch, None);
        assert!(!old.track_main_behind);
    }

    /// A submitted review that was EDITED keeps its GraphQL node id but
    /// changes its body. With a stable node id the merge must replace
    /// the stored copy in place — never append a duplicate row (the
    /// pending-review-duplicates bug was the node-id-less flavor of
    /// this: identity fell back to (author, body, created_at) with a
    /// fresh timestamp every poll).
    #[test]
    fn merge_activity_same_node_id_review_edit_replaces_in_place() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        let mut review = activity_at(10, "LGTM");
        review.kind = ActivityKind::Review;
        review.node_id = Some("PRR_1".into());
        ws.merge_activity(std::slice::from_ref(&review));
        assert_eq!(ws.activity.len(), 1);

        // Same node id, edited body, same timestamp (GitHub keeps
        // submittedAt on edit).
        let mut edited = review.clone();
        edited.body = "LGTM — one nit".into();
        ws.merge_activity(std::slice::from_ref(&edited));
        assert_eq!(
            ws.activity.len(),
            1,
            "an edit with the same node id must replace, not duplicate"
        );
        assert_eq!(ws.activity[0].body, "LGTM — one nit");

        // Re-polling the identical review stays idempotent.
        ws.merge_activity(std::slice::from_ref(&edited));
        assert_eq!(ws.activity.len(), 1);
    }

    /// Same-node-id replacement also holds when the re-poll carries a
    /// DIFFERENT created_at (a pending review that gets submitted gains
    /// a real submittedAt): the node id wins over the tuple.
    #[test]
    fn merge_activity_same_node_id_survives_timestamp_change() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        let mut review = activity_at(10, "thinking...");
        review.kind = ActivityKind::Review;
        review.node_id = Some("PRR_2".into());
        ws.merge_activity(std::slice::from_ref(&review));

        let mut submitted = review.clone();
        submitted.created_at = now() + chrono::Duration::seconds(500);
        ws.merge_activity(std::slice::from_ref(&submitted));
        assert_eq!(
            ws.activity.len(),
            1,
            "same node id with a new timestamp must still replace in place"
        );
        assert_eq!(ws.activity[0].created_at, submitted.created_at);
    }

    /// Issue→PR collapse (docs/resiliency-review.md): the absorbed
    /// issue workspace's activity must land on the PR workspace WITH
    /// its read/seen state — a comment the user already read on the
    /// issue must not resurface as unread after the fold, and the
    /// issue's genuinely-unread rows must stay unread.
    #[test]
    fn absorb_activity_from_carries_history_and_read_state() {
        let mut pr = Workspace::empty(WorkspaceKey::new("pr-ws"), "main", now());
        pr.merge_activity(&[activity_at(100, "pr-comment")]);
        pr.mark_activity_read(0);

        let mut issue = Workspace::empty(WorkspaceKey::new("issue-ws"), "main", now());
        issue.merge_activity(&[
            activity_at(30, "issue-newest"),
            activity_at(20, "issue-read"),
            activity_at(10, "issue-seen"),
        ]);
        // Oldest row is inside the positional seen tail; the middle
        // row carries an explicit read mark; the newest stays unread.
        issue.seen_count = 1;
        issue.mark_activity_read(1);
        assert!(issue.is_activity_unread(0));
        assert!(!issue.is_activity_unread(1));

        pr.absorb_activity_from(&issue);

        // Merged feed: all four rows, newest-first.
        let bodies: Vec<&str> = pr.activity.iter().map(|a| a.body.as_str()).collect();
        assert_eq!(
            bodies,
            ["pr-comment", "issue-newest", "issue-read", "issue-seen"],
            "the PR workspace must inherit the issue's full history"
        );
        assert!(!pr.is_activity_unread(0), "the PR's own read mark survives");
        assert!(
            pr.is_activity_unread(1),
            "an issue row the user never read stays unread"
        );
        assert!(
            !pr.is_activity_unread(2),
            "the issue's explicit read mark carries over"
        );
        assert!(
            !pr.is_activity_unread(3),
            "the issue's seen tail carries over as read"
        );
        assert_eq!(pr.unread_count(), 1);
    }

    /// The absorb must not manufacture read state: an all-unread
    /// issue folds in as all-unread, and the PR's pre-existing unread
    /// rows stay unread too.
    #[test]
    fn absorb_activity_from_keeps_unread_rows_unread() {
        let mut pr = Workspace::empty(WorkspaceKey::new("pr-ws"), "main", now());
        pr.merge_activity(&[activity_at(100, "pr-unread")]);
        let mut issue = Workspace::empty(WorkspaceKey::new("issue-ws"), "main", now());
        issue.merge_activity(&[activity_at(10, "issue-unread")]);

        pr.absorb_activity_from(&issue);
        assert_eq!(pr.activity.len(), 2);
        assert_eq!(pr.unread_count(), 2, "no row may be invented as read");
    }

    #[test]
    fn decode_persisted_accepts_legacy_rows_as_schema_zero() {
        // The same minimal pre-schema blob the other legacy tests use.
        let legacy = r#"{"key":"old","name":"old","branch":"main","pr":null,
            "gh_issues":[],"linear_issues":[],"activity":[],"seen_count":0,
            "created_at":"2024-01-01T00:00:00Z","last_viewed_at":null}"#;
        let ws = Workspace::decode_persisted(legacy).unwrap();
        assert_eq!(ws.schema, 0, "pre-schema rows read back as v0");
    }

    #[test]
    fn decode_persisted_refuses_rows_from_a_newer_build() {
        // Valid JSON, parses fine under lenient serde — but stamped by
        // a future build. Must be refused so a downgraded build never
        // rewrites (and thereby truncates) it.
        let future = r#"{"schema":999,"key":"old","name":"old","branch":"main","pr":null,
            "gh_issues":[],"linear_issues":[],"activity":[],"seen_count":0,
            "created_at":"2024-01-01T00:00:00Z","last_viewed_at":null,
            "field_from_the_future":{"important":"state"}}"#;
        let err = Workspace::decode_persisted(future).unwrap_err();
        assert!(
            matches!(
                err,
                WorkspaceDecodeError::NewerSchema {
                    found: 999,
                    supported: WORKSPACE_SCHEMA_VERSION
                }
            ),
            "expected NewerSchema, got: {err}"
        );
    }

    #[test]
    fn serialization_stamps_the_current_schema_even_on_legacy_loads() {
        let legacy = r#"{"key":"old","name":"old","branch":"main","pr":null,
            "gh_issues":[],"linear_issues":[],"activity":[],"seen_count":0,
            "created_at":"2024-01-01T00:00:00Z","last_viewed_at":null}"#;
        let ws = Workspace::decode_persisted(legacy).unwrap();
        let rewritten = serde_json::to_value(&ws).unwrap();
        assert_eq!(
            rewritten["schema"],
            serde_json::json!(WORKSPACE_SCHEMA_VERSION),
            "every save stamps the running build's schema version"
        );
    }

    #[test]
    fn supports_track_main_requires_a_github_worktree() {
        // GitHub project, lazybox worktree → eligible.
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "scratch", now());
        ws.project_key = Some(crate::ProjectKey::github("acme", "widget"));
        assert!(ws.supports_track_main());

        // A linked checkout sits on the user's own branch — not eligible.
        let mut linked = ws.clone();
        linked.linked_checkout = Some(std::path::PathBuf::from("/home/dev/acme/widget"));
        assert!(!linked.supports_track_main());

        // A local (non-GitHub) project has no origin to track.
        let mut local = Workspace::empty(WorkspaceKey::new("notes"), "scratch", now());
        local.project_key = Some(crate::ProjectKey::local("scratchpad"));
        assert!(!local.supports_track_main());

        // A PR workspace is excluded — its branch carries commits, so a
        // fast-forward onto main can never apply.
        let mut with_pr = ws.clone();
        with_pr.attach_task(pr("acme/widget#1"));
        assert!(with_pr.pr.is_some());
        assert!(!with_pr.supports_track_main());
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
    fn session_id_wraps_a_known_uuid() {
        let uuid = Uuid::from_u128(0x12345678_1234_5678_1234_567812345678);
        let session_id = SessionId(uuid);

        assert_eq!(session_id.0, uuid);
        assert_eq!(session_id.to_string(), uuid.to_string());
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
            worktree_branch: None,
            created_at: chrono::Utc::now(),
            last_output_at: None,
            layout: SessionLayout::default(),
            provider_session_ids: BTreeMap::new(),
        };
        w.add_session(make());
        w.add_session(make());
        assert_eq!(w.sessions.len(), 1, "second add with same id is a no-op");
    }

    #[test]
    fn provider_session_ids_round_trip_and_default_for_legacy_records() {
        let mut session = Session::new(
            WorkspaceKey::new("github:owner/repo#7"),
            SessionKind::Agent {
                agent_id: "codex".into(),
            },
            "/tmp/wt".into(),
            now(),
        );
        session
            .provider_session_ids
            .insert("codex".into(), "codex-conversation".into());
        session
            .provider_session_ids
            .insert("claude".into(), "claude-conversation".into());

        let value = serde_json::to_value(&session).expect("serialize session");
        let decoded: Session = serde_json::from_value(value.clone()).expect("deserialize session");
        assert_eq!(decoded.provider_session_ids, session.provider_session_ids);

        let mut legacy = value;
        legacy
            .as_object_mut()
            .expect("session object")
            .remove("provider_session_ids");
        let decoded: Session = serde_json::from_value(legacy).expect("deserialize legacy session");
        assert!(decoded.provider_session_ids.is_empty());
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

    #[test]
    fn same_titled_issue_workspaces_get_distinct_slugs() {
        // The collision that shared one checkout (and one branch)
        // between two agents: two issues in the same repo titled
        // identically used to slug to the bare title. The issue number
        // now disambiguates, mirroring branch naming.
        let mut a = issue("github", "o/r#42");
        a.title = "Bump dependencies".into();
        let mut b = issue("github", "o/r#43");
        b.title = "Bump dependencies".into();
        let a = Workspace::from_task(a, now());
        let b = Workspace::from_task(b, now());
        assert_ne!(a.worktree_slug(), b.worktree_slug());
        assert_eq!(a.worktree_slug(), "issue-42-bump-dependencies");
        assert_eq!(b.worktree_slug(), "issue-43-bump-dependencies");
    }

    #[test]
    fn same_titled_linear_workspaces_get_distinct_slugs() {
        // Numberless sources use their task key as the discriminator.
        let mut a = issue("linear", "ENG-456");
        a.title = "Ship it".into();
        let mut b = issue("linear", "ENG-457");
        b.title = "Ship it".into();
        let a = Workspace::from_task(a, now());
        let b = Workspace::from_task(b, now());
        assert_ne!(a.worktree_slug(), b.worktree_slug());
        assert_eq!(a.worktree_slug(), "eng-456-ship-it");
    }

    #[test]
    fn same_named_local_workspaces_get_distinct_slugs() {
        // Two scratch workspaces created with the same name get
        // collision-free keys (`bump-deps`, `bump-deps-2` — see the
        // daemon's `allocate_workspace_key`) but used to share a slug.
        // The key now carries the disambiguation; the FIRST workspace
        // of a name keeps its legacy `<name-slug>` path exactly, so
        // existing checkouts keep resolving.
        let mut a = Workspace::empty(crate::WorkspaceKey::new("bump-deps"), "main", now());
        a.name = "Bump Deps".into();
        let mut b = Workspace::empty(crate::WorkspaceKey::new("bump-deps-2"), "main", now());
        b.name = "Bump Deps".into();
        assert_eq!(a.worktree_slug(), "bump-deps", "legacy path preserved");
        assert_eq!(b.worktree_slug(), "bump-deps-2");
    }

    #[test]
    fn non_name_derived_key_keeps_plain_name_slug() {
        // A workspace whose key was NOT allocated from its name (a
        // task-keyed record, a sandbox key) keeps the legacy plain
        // name slug — the key would be unreadable as a directory name
        // and the legacy on-disk layout must not shift.
        let mut w = Workspace::empty(crate::WorkspaceKey::new("github:owner/repo"), "main", now());
        w.name = "Hotfix Auth".into();
        assert_eq!(w.worktree_slug(), "hotfix-auth");
    }

    // ── Worktree scope (#223) ─────────────────────────────────────
    //
    // The slug alone is not unique across repos: a workspace named
    // "Issues" in two different repos slugs identically. `worktree_scope`
    // is the repo/project qualifier that keeps their on-disk worktrees
    // in separate directories. It must stay stable across renames and
    // branch changes so the worktree doesn't orphan when either moves.

    fn named_ws_in_repo(name: &str, project: crate::ProjectKey) -> Workspace {
        let mut w = Workspace::empty(
            crate::WorkspaceKey::new(format!("local:{name}")),
            "main",
            now(),
        );
        w.name = name.into();
        w.project_key = Some(project);
        w
    }

    #[test]
    fn same_name_in_different_repos_gets_distinct_scopes() {
        let a = named_ws_in_repo("Issues", crate::ProjectKey::github("ownerA", "repoA"));
        let b = named_ws_in_repo("Issues", crate::ProjectKey::github("ownerB", "repoB"));
        // Same slug — the collision the bug rode in on.
        assert_eq!(a.worktree_slug(), b.worktree_slug());
        // Distinct scopes keep them off the same directory.
        assert_ne!(a.worktree_scope(), b.worktree_scope());
        assert_eq!(a.worktree_scope().as_deref(), Some("github-ownera-repoa"));
    }

    #[test]
    fn scope_is_stable_across_rename_and_branch_change() {
        let mut w = named_ws_in_repo("Issues", crate::ProjectKey::github("owner", "repo"));
        let before = w.worktree_scope();
        w.name = "Renamed".into();
        w.branch = "different-branch".into();
        assert_eq!(w.worktree_scope(), before);
    }

    #[test]
    fn scope_recovers_from_task_when_project_key_absent() {
        // Back-compat record predating projects: no `project_key`, but
        // the primary task still carries the repo to scope on.
        let mut w = Workspace::from_task(pr("o/r#1"), now());
        w.project_key = None;
        assert_eq!(w.worktree_scope().as_deref(), Some("github-o-r"));
    }

    #[test]
    fn scope_is_none_for_repoless_projectless_workspace() {
        let mut w = Workspace::empty(crate::WorkspaceKey::new("scratch"), "main", now());
        w.name = "Scratch".into();
        assert_eq!(w.worktree_scope(), None);
    }

    #[test]
    fn has_notes_ignores_blank_and_whitespace() {
        let mut w = Workspace::from_task(pr("o/r#1"), now());
        assert!(!w.has_notes());
        w.notes = "   \n\t".into();
        assert!(!w.has_notes());
        w.notes = "check the flaky test".into();
        assert!(w.has_notes());
    }

    #[test]
    fn notes_default_when_absent_from_json() {
        // Records written before #458 have no `notes` key; they must
        // deserialize to an empty scratchpad rather than fail.
        let mut w = Workspace::from_task(pr("o/r#1"), now());
        w.notes = "keep me".into();
        let json = serde_json::to_string(&w).unwrap();
        let back: Workspace = serde_json::from_str(&json).unwrap();
        assert_eq!(back.notes, "keep me");

        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value.as_object_mut().unwrap().remove("notes");
        let legacy: Workspace = serde_json::from_value(value).unwrap();
        assert_eq!(legacy.notes, "");
        assert!(!legacy.has_notes());
    }

    /// `sent_snippets` (#463) is an MRU: a re-send moves the key to the
    /// front, distinct keys stack newest-first, and the list is capped
    /// at [`SENT_SNIPPETS_MAX`], dropping the oldest.
    #[test]
    fn sent_snippets_mru_dedups_and_caps() {
        let mut w = Workspace::from_task(pr("o/r#1"), now());
        assert!(w.sent_snippets.is_empty(), "nothing sent yet");

        w.record_sent_snippet("rev".into());
        w.record_sent_snippet("plan".into());
        assert_eq!(w.sent_snippets, vec!["plan", "rev"], "most-recent first");

        w.record_sent_snippet("rev".into());
        assert_eq!(
            w.sent_snippets,
            vec!["rev", "plan"],
            "a re-send moves the key to the front, no duplicate",
        );

        for i in 0..SENT_SNIPPETS_MAX {
            w.record_sent_snippet(format!("k{i}"));
        }
        assert_eq!(w.sent_snippets.len(), SENT_SNIPPETS_MAX, "capped");
        assert_eq!(
            w.sent_snippets[0],
            format!("k{}", SENT_SNIPPETS_MAX - 1),
            "newest at the front",
        );
        assert!(
            !w.sent_snippets.iter().any(|k| k == "plan"),
            "the oldest keys evicted past the cap",
        );
    }

    /// `sent_snippets` round-trips through the workspace JSON blob, and a
    /// pre-#463 record (no key) reads back as an empty list.
    #[test]
    fn sent_snippets_default_when_absent_from_json() {
        let mut w = Workspace::from_task(pr("o/r#1"), now());
        w.record_sent_snippet("rev".into());
        let json = serde_json::to_string(&w).unwrap();
        let back: Workspace = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sent_snippets, vec!["rev"]);

        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value.as_object_mut().unwrap().remove("sent_snippets");
        let legacy: Workspace = serde_json::from_value(value).unwrap();
        assert!(legacy.sent_snippets.is_empty());
    }

    /// Populate every user-owned field with a distinctive, non-default
    /// value so a round-trip through `absorb_user_state_from` proves each
    /// one is actually carried. Deliberately does NOT set structural /
    /// provider-derived fields — those are the destination's own.
    fn source_with_all_user_state() -> Workspace {
        let mut w = Workspace::empty(WorkspaceKey::new("issue-src"), "scratch", now());
        w.snoozed_until = Some(now() + chrono::Duration::hours(5));
        w.auto_merge_on_green = true;
        w.track_main = true;
        w.base_branch = Some("main".into());
        w.track_main_behind = true;
        w.policies
            .set(crate::AutoFixKind::CiFailure, crate::PolicyArm::Arm);
        w.policies
            .set(crate::AutoFixKind::MergeConflict, crate::PolicyArm::Disarm);
        w.notes = "source note".into();
        w.record_sent_snippet("rev".into());
        w.record_sent_snippet("plan".into());
        w.cleanup_prompt = CleanupPrompt::Declined;
        w.last_viewed_at = Some(now() + chrono::Duration::hours(2));
        w
    }

    /// The core #554 guarantee for the always-portable fields: notes,
    /// snippet MRU, policies and last-viewed transfer onto any destination
    /// regardless of its kind. Uses a track-main-eligible (GitHub, no-PR)
    /// destination so the eligibility-gated arms carry here too.
    #[test]
    fn absorb_user_state_carries_portable_fields_onto_eligible_target() {
        let source = source_with_all_user_state();
        // Issue workspace: no PR, GitHub project → track-main eligible.
        let mut target = Workspace::from_task(issue("github", "o/r#99"), now());
        // Pre-snooze so the snooze rule (extend-only) applies.
        target.snoozed_until = Some(now() + chrono::Duration::hours(1));
        target.absorb_user_state_from(&source);

        assert_eq!(target.notes, "source note", "notes carried");
        assert_eq!(
            target.sent_snippets,
            vec!["plan", "rev"],
            "snippet MRU carried in order",
        );
        assert_eq!(
            target.policies.arm(crate::AutoFixKind::CiFailure),
            crate::PolicyArm::Arm,
            "auto-fix-ci policy carried",
        );
        assert_eq!(
            target.policies.arm(crate::AutoFixKind::MergeConflict),
            crate::PolicyArm::Disarm,
            "auto-fix-conflict policy carried",
        );
        assert_eq!(
            target.last_viewed_at, source.last_viewed_at,
            "later last-viewed carried",
        );
        // Eligible destination → track-main trio carries.
        assert!(
            target.track_main,
            "track-main arm carried onto eligible dest"
        );
        assert_eq!(target.base_branch.as_deref(), Some("main"), "base carried");
        assert!(target.track_main_behind, "behind flag carried");
        // Already-snoozed destination → extended to the later deadline.
        assert_eq!(
            target.snoozed_until,
            Some(now() + chrono::Duration::hours(5)),
            "snooze extended to the later deadline",
        );
        // No PR on this destination → merge-on-green arm not carried.
        assert!(
            !target.auto_merge_on_green,
            "merge-on-green must not arm a PR-less destination",
        );
        // Cleanup is the destination's own lifecycle decision, not carried.
        assert_eq!(
            target.cleanup_prompt,
            CleanupPrompt::Unresolved,
            "destination keeps its own cleanup answer",
        );
    }

    /// #554 regression: an issue with track-main armed, folded into its
    /// closing PR, must NOT paint a track-main badge on the PR — a PR is
    /// ineligible and the sweep never clears the flag, so the badge would
    /// be permanent and wrong.
    #[test]
    fn absorb_user_state_never_arms_track_main_on_a_pr() {
        let source = source_with_all_user_state();
        assert!(source.track_main, "precondition: source tracks main");
        let mut pr_target = Workspace::from_task(pr("o/r#1"), now());
        assert!(
            !pr_target.supports_track_main(),
            "a PR is track-main ineligible"
        );
        pr_target.absorb_user_state_from(&source);
        assert!(!pr_target.track_main, "track-main must not ride onto a PR");
        assert!(
            !pr_target.track_main_behind,
            "behind flag must not ride onto a PR"
        );
        assert_eq!(pr_target.base_branch, None, "no track-main base on a PR");
    }

    /// #554: a snooze on the source must never *hide* a destination the
    /// user can currently see (a visible PR the folded issue was snoozed).
    #[test]
    fn absorb_user_state_snooze_never_hides_a_visible_destination() {
        let source = source_with_all_user_state();
        assert!(
            source.snoozed_until.is_some(),
            "precondition: source snoozed"
        );
        let mut visible = Workspace::from_task(pr("o/r#1"), now());
        assert_eq!(visible.snoozed_until, None, "destination starts visible");
        visible.absorb_user_state_from(&source);
        assert_eq!(
            visible.snoozed_until, None,
            "a snoozed source must not hide a visible destination",
        );
    }

    /// #554: merge-on-green rides onto a destination that has a PR (there
    /// is something to merge), matching the UI's own arming gate.
    #[test]
    fn absorb_user_state_arms_merge_on_green_only_with_a_pr() {
        let mut source = Workspace::empty(WorkspaceKey::new("src"), "scratch", now());
        source.auto_merge_on_green = true;

        let mut pr_target = Workspace::from_task(pr("o/r#1"), now());
        pr_target.absorb_user_state_from(&source);
        assert!(pr_target.auto_merge_on_green, "arm carried onto a PR");

        let mut issue_target = Workspace::from_task(issue("github", "o/r#42"), now());
        issue_target.absorb_user_state_from(&source);
        assert!(
            !issue_target.auto_merge_on_green,
            "arm must not ride onto a PR-less destination",
        );
    }

    /// When both sides carry state, the merge rules combine rather than
    /// clobber: snooze/last-viewed take the later value, arms OR, notes
    /// concatenate, snippets union, policies keep the more decisive arm,
    /// cleanup keeps a recorded "keep".
    #[test]
    fn absorb_user_state_merges_when_both_sides_populated() {
        let source = source_with_all_user_state();

        let mut target = Workspace::empty(WorkspaceKey::new("pr-dst"), "feature", now());
        // Target has an EARLIER snooze and an EARLIER view than source.
        target.snoozed_until = Some(now() + chrono::Duration::hours(1));
        target.last_viewed_at = Some(now() + chrono::Duration::hours(1));
        target.notes = "target note".into();
        target.record_sent_snippet("test".into()); // distinct key
        target.record_sent_snippet("rev".into()); // shared with source
        // Target disarms auto-fix-ci; source armed it — Disarm is stronger.
        target
            .policies
            .set(crate::AutoFixKind::CiFailure, crate::PolicyArm::Disarm);

        target.absorb_user_state_from(&source);

        assert_eq!(
            target.snoozed_until,
            Some(now() + chrono::Duration::hours(5)),
            "the later snooze deadline wins",
        );
        assert_eq!(
            target.last_viewed_at,
            Some(now() + chrono::Duration::hours(2)),
            "the more recent view wins",
        );
        assert_eq!(
            target.notes, "target note\n\nsource note",
            "both notes kept, destination first",
        );
        // Union, destination MRU first (rev de-duped, not doubled).
        assert_eq!(target.sent_snippets, vec!["rev", "test", "plan"]);
        assert_eq!(
            target.policies.arm(crate::AutoFixKind::CiFailure),
            crate::PolicyArm::Disarm,
            "an explicit Disarm outranks the source's Arm",
        );
    }

    /// Merging notes must never drop a scratchpad: an empty side yields the
    /// other verbatim, and identical notes stay single (no doubling).
    #[test]
    fn merge_notes_is_lossless() {
        assert_eq!(merge_notes("", "src"), "src");
        assert_eq!(merge_notes("dst", ""), "dst");
        assert_eq!(merge_notes("same", "same"), "same");
        assert_eq!(merge_notes("a", "b"), "a\n\nb");
        assert_eq!(merge_notes("   ", "b"), "b", "whitespace-only counts empty");
    }

    /// The absorb is non-destructive on the source (the adopt flow keeps
    /// the source as a tracking row).
    #[test]
    fn absorb_user_state_leaves_source_untouched() {
        let source = source_with_all_user_state();
        let mut target = Workspace::empty(WorkspaceKey::new("pr-dst"), "feature", now());
        target.absorb_user_state_from(&source);
        // Source retains its own state — re-absorbing must be idempotent.
        assert_eq!(source.notes, "source note");
        assert_eq!(source.sent_snippets, vec!["plan", "rev"]);
        assert_eq!(source.cleanup_prompt, CleanupPrompt::Declined);
    }
}
