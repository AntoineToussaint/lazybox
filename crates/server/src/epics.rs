//! The epic resolver — the daemon side of #1517/#1522.
//!
//! An **epic** is a named, cross-repo set of workspaces with a dependency
//! graph. Its *status* is never stored: the resolver here recomputes a fresh
//! [`EpicSnapshot`] from the live workspaces + agent states on every relevant
//! event and broadcasts it on the bus. The only persisted thing is the thin
//! [`EpicRecord`] (identity + membership + opt-ins), kept as an `epic:<key>`
//! kv row exactly like a working claim — no schema change.
//!
//! Shape of the module:
//! - kv helpers ([`persist`], [`load`], [`list_all`], [`archive`]) over the
//!   `epic:` key prefix.
//! - Pure resolver functions ([`resolve`], [`diff`] and the graph helpers they
//!   call) that take plain data so they unit-test without a `ServerConfig`.
//! - A debounced bus subscriber ([`spawn`]) that coalesces bursts and calls
//!   [`recompute_all`], the one recompute path.
//!
//! ## Lock discipline
//!
//! [`recompute_all`] snapshots the agent states *async* first
//! (`agent_states_by_workspace().await`), then takes the `parking_lot`
//! [`EpicMemory`] lock and does the whole synchronous load→resolve→diff→store
//! under it with **no `.await`** — the latch mutex must never be held across a
//! suspend point. Bus sends are collected and emitted after the guard drops.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use lazybox_core::{
    Activity, ActivityKind, EpicRecord, Role, Task, TaskId, TaskState, Workspace, WorkspaceKey,
};
use lazybox_ipc::{
    AgentState, Blocker, BlockerKind, BlockerOwner, EdgeKind, EpicDelta, EpicEdge, EpicMember,
    EpicMemberStatus, EpicSnapshot, Event, MergeOrderEntry,
};
use tokio::sync::broadcast;

use crate::ServerConfig;

/// kv key prefix for persisted epic records. One row per epic.
const EPIC_KEY_PREFIX: &str = "epic:";

/// How long to wait after the last relevant event before recomputing. A burst
/// of `AgentState` / `WorkspaceUpserted` events (a poll tick updating a dozen
/// rows) coalesces into a single recompute instead of one per event.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// The resolver's cross-recompute memory. Neither field is persisted — both are
/// rebuilt lazily and only serve to keep derived output stable:
///
/// - `since`: per-epic latch of when each blocker *first* appeared, keyed by
///   `(member, kind, reason)`, so a blocker's age is trustworthy across
///   recomputes (a recompute must not reset "blocked 2h ago" to "just now").
/// - `last`: the last snapshot broadcast per epic, so a recompute can diff
///   against it and skip epics whose snapshot did not change.
#[derive(Default)]
pub struct EpicMemory {
    since: HashMap<String, HashMap<(WorkspaceKey, BlockerKind, String), i64>>,
    last: HashMap<String, EpicSnapshot>,
}

// ── kv persistence ──────────────────────────────────────────────────────

fn storage_key(key: &str) -> String {
    format!("{EPIC_KEY_PREFIX}{key}")
}

/// Persist (create or overwrite) an epic record.
pub fn persist(config: &ServerConfig, record: &EpicRecord) -> Result<(), String> {
    let json = serde_json::to_string(record).map_err(|e| e.to_string())?;
    config
        .store
        .set_kv(&storage_key(record.key.as_str()), &json)
        .map_err(|e| e.to_string())
}

/// Load one epic record by key, if present.
pub fn load(config: &ServerConfig, key: &str) -> Result<Option<EpicRecord>, String> {
    config
        .store
        .get_kv(&storage_key(key))
        .map_err(|e| e.to_string())?
        .map(|json| serde_json::from_str(&json).map_err(|e| e.to_string()))
        .transpose()
}

/// Load every epic record. A single row that fails to decode is skipped with a
/// warning rather than sinking the whole list — one malformed epic must not
/// blind the resolver to the others.
pub fn list_all(config: &ServerConfig) -> Result<Vec<EpicRecord>, String> {
    let rows = config
        .store
        .list_kv_prefix(EPIC_KEY_PREFIX)
        .map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(rows.len());
    for (key, json) in rows {
        match serde_json::from_str::<EpicRecord>(&json) {
            Ok(record) => out.push(record),
            Err(e) => tracing::warn!("epics: skipping unreadable record {key}: {e}"),
        }
    }
    Ok(out)
}

// ── declared blockers (report_blocker / clear_blocker) ───────────────────

/// kv key prefix for operator/agent-declared blockers. One row per workspace.
const DECLARED_BLOCKER_PREFIX: &str = "declared-blocker:";

/// A blocker a worker declared on its *own* workspace via the `report_blocker`
/// MCP tool, when it cannot proceed without a human or an outside party. Unlike
/// the graph-derived blockers it is *persisted*, so its age survives a daemon
/// restart, and [`resolve`] reads it as a first-class blocker source.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeclaredBlocker {
    pub workspace: WorkspaceKey,
    /// One-line human-readable reason ("waiting on legal sign-off").
    pub reason: String,
    /// Refined kind; [`BlockerKind::Decision`] when the reporter did not say.
    pub kind: BlockerKind,
    /// Who must clear it. [`BlockerOwner::Operator`] (the default) is what
    /// raises the `!` and the stale-blocker alert.
    pub owner: BlockerOwner,
    /// Unix ms first declared — the trustworthy age, since it is persisted.
    pub since: i64,
}

fn declared_storage_key(workspace: &str) -> String {
    format!("{DECLARED_BLOCKER_PREFIX}{workspace}")
}

/// Persist (create or overwrite) a declared blocker for one workspace.
pub fn persist_declared(config: &ServerConfig, blocker: &DeclaredBlocker) -> Result<(), String> {
    let json = serde_json::to_string(blocker).map_err(|e| e.to_string())?;
    config
        .store
        .set_kv(&declared_storage_key(blocker.workspace.as_str()), &json)
        .map_err(|e| e.to_string())
}

/// Load one workspace's declared blocker, if present.
pub fn load_declared(
    config: &ServerConfig,
    workspace: &str,
) -> Result<Option<DeclaredBlocker>, String> {
    config
        .store
        .get_kv(&declared_storage_key(workspace))
        .map_err(|e| e.to_string())?
        .map(|json| serde_json::from_str(&json).map_err(|e| e.to_string()))
        .transpose()
}

/// Remove a workspace's declared blocker, if any (idempotent).
pub fn clear_declared(config: &ServerConfig, workspace: &str) -> Result<(), String> {
    config
        .store
        .delete_kv(&declared_storage_key(workspace))
        .map_err(|e| e.to_string())
}

/// Load every declared blocker, keyed by workspace. A row that fails to decode
/// is skipped with a warning rather than sinking the whole map.
pub fn list_declared(
    config: &ServerConfig,
) -> Result<HashMap<WorkspaceKey, DeclaredBlocker>, String> {
    let rows = config
        .store
        .list_kv_prefix(DECLARED_BLOCKER_PREFIX)
        .map_err(|e| e.to_string())?;
    let mut out = HashMap::with_capacity(rows.len());
    for (key, json) in rows {
        match serde_json::from_str::<DeclaredBlocker>(&json) {
            Ok(blocker) => {
                out.insert(blocker.workspace.clone(), blocker);
            }
            Err(e) => tracing::warn!("epics: skipping unreadable declared blocker {key}: {e}"),
        }
    }
    Ok(out)
}

// ── pure resolver ───────────────────────────────────────────────────────

/// Everything the resolver derives for a single epic in one place, so the
/// membership/graph work is done once and shared by `waves`, `blockers_for`,
/// and `member_status`.
struct Resolved<'a> {
    /// Members that resolved to a loaded workspace, in insertion order.
    members: Vec<WorkspaceKey>,
    by_key: HashMap<WorkspaceKey, &'a Workspace>,
    /// `depends_on[M]` = the members M directly depends on (edges).
    depends_on: HashMap<WorkspaceKey, BTreeSet<WorkspaceKey>>,
    /// `merge_after[M]` = the members whose PRs must land before M's may merge.
    /// The union of M's explicit `Merge after:` markers and — unless the epic
    /// opts out — every `depends_on[M]` edge (a work dependency implies a
    /// landing-order one).
    merge_after: HashMap<WorkspaceKey, BTreeSet<WorkspaceKey>>,
    /// `external[M]` = blocking tasks that are not themselves members.
    external: HashMap<WorkspaceKey, BTreeSet<TaskId>>,
    /// Whether each member has *landed* — counts toward epic completion. A PR
    /// is done only when **merged** (a closed-unmerged PR is abandoned, not
    /// done); an issue is done when merged or closed. See [`member_done`].
    done: HashMap<WorkspaceKey, bool>,
    /// Whether each member has stopped gating a successor's *landing order* —
    /// its merge-after edge is no longer live. True once the deliverable reaches
    /// a terminal state: a PR **merged** (order honored) *or* **closed** without
    /// merging (abandoned, so the "land after me" premise is void). Distinct
    /// from `done`, which excludes the closed case. See [`landing_settled`].
    landing_settled: HashMap<WorkspaceKey, bool>,
    /// Whether each member currently has a *live* PR (open, not merged/closed).
    /// Merge order lists only PR members.
    has_pr: HashMap<WorkspaceKey, bool>,
}

/// Resolve an epic's membership + dependency graph from the loaded workspaces.
fn resolved_graph<'a>(record: &EpicRecord, workspaces: &'a [Workspace]) -> Resolved<'a> {
    let by_key: HashMap<WorkspaceKey, &Workspace> =
        workspaces.iter().map(|w| (w.key.clone(), w)).collect();

    // task id → owning workspace, and task id → its parent, across every
    // loaded workspace (membership by anchor walks the parent chain).
    let mut task_ws: HashMap<TaskId, WorkspaceKey> = HashMap::new();
    let mut task_parent: HashMap<TaskId, TaskId> = HashMap::new();
    for ws in workspaces {
        for task in tasks_of(ws) {
            task_ws.insert(task.id.clone(), ws.key.clone());
            if let Some(parent) = &task.parent {
                task_parent.insert(task.id.clone(), parent.clone());
            }
        }
    }

    // Membership = explicit members (that resolve to a loaded workspace) ∪
    // every workspace whose task chain reaches the anchor. Insertion order is
    // preserved and deduped so wave leveling is deterministic.
    let mut members: Vec<WorkspaceKey> = Vec::new();
    let mut seen: HashSet<WorkspaceKey> = HashSet::new();
    let push = |key: WorkspaceKey, members: &mut Vec<WorkspaceKey>, seen: &mut HashSet<_>| {
        if seen.insert(key.clone()) {
            members.push(key);
        }
    };
    for key in &record.members {
        if by_key.contains_key(key) {
            push(key.clone(), &mut members, &mut seen);
        }
    }
    if let Some(anchor) = &record.anchor {
        // Deterministic order over the anchor sweep: sort candidate keys.
        let mut anchored: Vec<WorkspaceKey> = workspaces
            .iter()
            .filter(|ws| tasks_of(ws).any(|t| reaches_anchor(&t.id, anchor, &task_parent)))
            .map(|ws| ws.key.clone())
            .collect();
        anchored.sort();
        for key in anchored {
            push(key, &mut members, &mut seen);
        }
    }
    let member_set: HashSet<WorkspaceKey> = members.iter().cloned().collect();

    // Edges: each member's tasks' `blocked_by`. A blocker owned by another
    // member is an internal dependency edge; a blocker owned by this member's
    // own workspace is internal to its own work (not an epic blocker) and is
    // skipped; anything else (a non-member workspace, or an unknown task) is
    // external.
    let mut depends_on: HashMap<WorkspaceKey, BTreeSet<WorkspaceKey>> = HashMap::new();
    let mut merge_after: HashMap<WorkspaceKey, BTreeSet<WorkspaceKey>> = HashMap::new();
    let mut external: HashMap<WorkspaceKey, BTreeSet<TaskId>> = HashMap::new();
    let mut done: HashMap<WorkspaceKey, bool> = HashMap::new();
    let mut landing_settled: HashMap<WorkspaceKey, bool> = HashMap::new();
    let mut has_pr: HashMap<WorkspaceKey, bool> = HashMap::new();
    // Same classify-a-blocker logic reused for `blocked_by` and `merge_after`:
    // a member's own task is internal, another member is an internal edge, and
    // anything else is external. Merge-after only tracks the internal-edge case
    // (cross-member landing order); external merge-after tasks aren't part of
    // the epic's own merge sequence.
    for key in &members {
        let ws = by_key[key];
        done.insert(key.clone(), member_done(ws));
        landing_settled.insert(key.clone(), landing_settled_for(ws));
        has_pr.insert(
            key.clone(),
            ws.pr
                .as_ref()
                .is_some_and(|p| p.state != TaskState::Merged && p.state != TaskState::Closed),
        );
        let deps = depends_on.entry(key.clone()).or_default();
        let ext = external.entry(key.clone()).or_default();
        let mut ma: BTreeSet<WorkspaceKey> = BTreeSet::new();
        for task in tasks_of(ws) {
            for blocker in &task.blocked_by {
                match task_ws.get(blocker) {
                    Some(other) if other == key => {
                        // The member's own task blocking another of its tasks
                        // is internal to its work, not an epic-level wait.
                    }
                    Some(other) if member_set.contains(other) => {
                        deps.insert(other.clone());
                    }
                    _ => {
                        ext.insert(blocker.clone());
                    }
                }
            }
            for pred in &task.merge_after {
                match task_ws.get(pred) {
                    Some(other) if other == key => {}
                    Some(other) if member_set.contains(other) => {
                        ma.insert(other.clone());
                    }
                    _ => {
                        // A merge-after predecessor lazybox doesn't track as a
                        // member is out of scope for the epic's merge sequence.
                    }
                }
            }
        }
        merge_after.insert(key.clone(), ma);
    }

    // A `Blocks` edge implies a `MergeAfter` edge unless the epic opts out: the
    // dependent's PR must not land before the PR it depends on.
    if record.implied_merge_after {
        for (key, deps) in &depends_on {
            let ma = merge_after.entry(key.clone()).or_default();
            for dep in deps {
                ma.insert(dep.clone());
            }
        }
    }

    Resolved {
        members,
        by_key,
        depends_on,
        merge_after,
        external,
        done,
        landing_settled,
        has_pr,
    }
}

/// Every task linked to a workspace: the PR and each linked issue.
fn tasks_of(ws: &Workspace) -> impl Iterator<Item = &Task> {
    ws.pr
        .iter()
        .chain(ws.gh_issues.iter())
        .chain(ws.linear_issues.iter())
}

/// Whether a task's parent chain reaches `anchor` (walking strictly upward
/// from the task's parent), with a visited guard against a malformed cycle.
fn reaches_anchor(start: &TaskId, anchor: &TaskId, parents: &HashMap<TaskId, TaskId>) -> bool {
    let mut visited: HashSet<&TaskId> = HashSet::new();
    let mut cur = parents.get(start);
    while let Some(id) = cur {
        if id == anchor {
            return true;
        }
        if !visited.insert(id) {
            break;
        }
        cur = parents.get(id);
    }
    false
}

/// A workspace's own PR/issue is "done" only when the work actually landed. A
/// PR is done when **merged** — a PR *closed without merging* is an abandoned
/// deliverable, not a completion, so it must not count toward `Done`, must not
/// satisfy a downstream dependency, and must not fire the epic-wide `Completed`.
/// An issue has no merge state, so it is done when closed. The PR wins when
/// present; otherwise the first linked issue stands for the member.
fn member_done(ws: &Workspace) -> bool {
    if let Some(pr) = &ws.pr {
        return pr.state == TaskState::Merged;
    }
    tasks_of(ws)
        .next()
        .is_some_and(|t| matches!(t.state, TaskState::Merged | TaskState::Closed))
}

/// Whether a member has stopped gating a successor's landing order.
///
/// A `merge_after` edge encodes "B must land after A." That constraint is live
/// only while A is still expected to land, and it resolves in *two* ways, both
/// of which must free B:
///   * A's PR **merged** — the order was honored; or
///   * A's PR **closed without merging** — A is abandoned and will never land,
///     so the "land after me" premise is void. Holding B behind a dead PR
///     forever is a stall, not a safeguard (auto-merge silently never fires and
///     manual `g m` keeps being refused). An issue-only member settles when its
///     issue reaches a terminal Merged/Closed state.
///
/// Deliberately distinct from [`member_done`]: an abandoned predecessor is *not*
/// Done (it must not count toward the epic's `Completed`, and it must not
/// satisfy a work dependency), but it must not gate a merge either. Completion
/// keys off `member_done`; the merge hold keys off this.
fn landing_settled_for(ws: &Workspace) -> bool {
    if let Some(pr) = &ws.pr {
        return matches!(pr.state, TaskState::Merged | TaskState::Closed);
    }
    tasks_of(ws)
        .next()
        .is_some_and(|t| matches!(t.state, TaskState::Merged | TaskState::Closed))
}

/// Kahn topological leveling with longest-path waves and cycle detection.
///
/// `wave[root] = 0`; every other member sits one past the deepest member it
/// depends on. Returns the wave map, a topological order (empty tail on a
/// cycle), and the set of members caught in a cycle (never drained).
fn waves(
    members: &[WorkspaceKey],
    depends_on: &HashMap<WorkspaceKey, BTreeSet<WorkspaceKey>>,
) -> (
    HashMap<WorkspaceKey, u16>,
    Vec<WorkspaceKey>,
    HashSet<WorkspaceKey>,
) {
    // dependents[X] = members that depend on X (reverse edges), for draining.
    let mut dependents: HashMap<WorkspaceKey, Vec<WorkspaceKey>> = HashMap::new();
    let mut indegree: HashMap<WorkspaceKey, usize> = HashMap::new();
    for m in members {
        indegree.entry(m.clone()).or_insert(0);
        for dep in depends_on.get(m).into_iter().flatten() {
            dependents.entry(dep.clone()).or_default().push(m.clone());
            *indegree.entry(m.clone()).or_insert(0) += 1;
        }
    }

    let mut wave: HashMap<WorkspaceKey, u16> = members.iter().map(|m| (m.clone(), 0)).collect();
    // Seed the queue in members order so ties resolve deterministically.
    let mut queue: VecDeque<WorkspaceKey> = members
        .iter()
        .filter(|m| indegree.get(*m).copied().unwrap_or(0) == 0)
        .cloned()
        .collect();
    let mut order: Vec<WorkspaceKey> = Vec::with_capacity(members.len());
    while let Some(x) = queue.pop_front() {
        order.push(x.clone());
        let wx = wave[&x];
        for m in dependents.get(&x).into_iter().flatten() {
            let entry = wave.get_mut(m).expect("member has a wave");
            *entry = (*entry).max(wx.saturating_add(1));
            let deg = indegree.get_mut(m).expect("member has indegree");
            *deg -= 1;
            if *deg == 0 {
                queue.push_back(m.clone());
            }
        }
    }

    // Anything never drained sits on a cycle.
    let cycle: HashSet<WorkspaceKey> = members
        .iter()
        .filter(|m| indegree.get(*m).copied().unwrap_or(0) > 0)
        .cloned()
        .collect();
    (wave, order, cycle)
}

/// For each member, how many *other* members transitively depend on it. Drives
/// a blocker's `holds` (members waiting behind this one).
fn downstream_counts(
    members: &[WorkspaceKey],
    depends_on: &HashMap<WorkspaceKey, BTreeSet<WorkspaceKey>>,
) -> HashMap<WorkspaceKey, u32> {
    let mut dependents: HashMap<WorkspaceKey, Vec<WorkspaceKey>> = HashMap::new();
    for m in members {
        for dep in depends_on.get(m).into_iter().flatten() {
            dependents.entry(dep.clone()).or_default().push(m.clone());
        }
    }
    let mut out: HashMap<WorkspaceKey, u32> = HashMap::new();
    for start in members {
        let mut seen: HashSet<WorkspaceKey> = HashSet::new();
        let mut stack: Vec<WorkspaceKey> = dependents
            .get(start)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        while let Some(node) = stack.pop() {
            if node == *start || !seen.insert(node.clone()) {
                continue;
            }
            for next in dependents.get(&node).into_iter().flatten() {
                stack.push(next.clone());
            }
        }
        out.insert(start.clone(), seen.len() as u32);
    }
    out
}

/// The declared blocker kind refined from a `blocked:<kind>` label on the task,
/// if any; `None` when the task carries no such label.
fn declared_kind(task: &Task) -> Option<BlockerKind> {
    task.labels
        .iter()
        .find_map(|l| l.name.strip_prefix("blocked:").map(BlockerKind::parse))
}

/// Compute the blockers for one member: cycle membership, unfinished
/// dependencies, external tasks, and operator-declared blockers. `since` is
/// latched (via `since_at`) so ages survive recomputes; `holds` is recomputed.
#[allow(clippy::too_many_arguments)]
fn blockers_for(
    key: &WorkspaceKey,
    resolved: &Resolved,
    in_cycle: bool,
    holds: u32,
    declared: Option<&DeclaredBlocker>,
    since_at: &mut dyn FnMut(&WorkspaceKey, BlockerKind, &str) -> i64,
) -> Vec<Blocker> {
    let mut out: Vec<Blocker> = Vec::new();

    if in_cycle {
        let reason = "dependency cycle".to_string();
        let since = since_at(key, BlockerKind::Cycle, &reason);
        out.push(Blocker {
            kind: BlockerKind::Cycle,
            reason,
            owner: BlockerOwner::Operator,
            since,
            holds,
        });
    }

    for dep in resolved.depends_on.get(key).into_iter().flatten() {
        if resolved.done.get(dep).copied().unwrap_or(false) {
            continue; // a merged/closed dependency no longer blocks.
        }
        let dep_name = resolved
            .by_key
            .get(dep)
            .map(|w| w.name.clone())
            .unwrap_or_else(|| dep.as_str().to_string());
        let reason = format!("waiting on {dep_name}");
        let since = since_at(key, BlockerKind::Dependency, &reason);
        out.push(Blocker {
            kind: BlockerKind::Dependency,
            reason,
            owner: BlockerOwner::Agent(dep.clone()),
            since,
            holds,
        });
    }

    for ext in resolved.external.get(key).into_iter().flatten() {
        let reason = format!("waiting on {ext}");
        let since = since_at(key, BlockerKind::External, &reason);
        out.push(Blocker {
            kind: BlockerKind::External,
            reason,
            owner: BlockerOwner::External(ext.to_string()),
            since,
            holds,
        });
    }

    if let Some(ws) = resolved.by_key.get(key) {
        for task in tasks_of(ws) {
            let Some(reason) = task.blocked_on.as_ref().filter(|r| !r.trim().is_empty()) else {
                continue;
            };
            let kind = declared_kind(task).unwrap_or(BlockerKind::Decision);
            let since = since_at(key, kind, reason);
            out.push(Blocker {
                kind,
                reason: reason.clone(),
                owner: BlockerOwner::Operator,
                since,
                holds,
            });
        }
    }

    // A blocker the worker declared on itself via `report_blocker` — persisted,
    // so it uses its own recorded `since` (trustworthy across restarts) rather
    // than the in-memory latch.
    if let Some(declared) = declared {
        let reason = declared.reason.trim();
        if !reason.is_empty() {
            out.push(Blocker {
                kind: declared.kind,
                reason: reason.to_string(),
                owner: declared.owner.clone(),
                since: declared.since,
                holds,
            });
        }
    }

    // Deterministic order: by kind, then reason.
    out.sort_by(|a, b| (a.kind.as_str(), &a.reason).cmp(&(b.kind.as_str(), &b.reason)));
    out.dedup_by(|a, b| a.kind == b.kind && a.reason == b.reason);
    out
}

/// Derive a member's status. Precedence (first match wins): Done → Failed →
/// Asking → InProgress → Mergeable → PrOpen → Claimed → Blocked → Ready.
fn member_status(
    ws: &Workspace,
    agent: Option<AgentState>,
    blockers: &[Blocker],
    held_by: &[WorkspaceKey],
) -> EpicMemberStatus {
    if member_done(ws) {
        return EpicMemberStatus::Done;
    }
    // A PR closed without merging is a dead deliverable, not a live PR, so it
    // must not read as `Mergeable`/`PrOpen`. `member_done` already returned for
    // a *merged* PR above, so filtering `Closed` here leaves only live PR states
    // and lets an abandoned PR fall through to Claimed/Blocked/Ready — the honest
    // "this member still needs a completed deliverable."
    let pr = ws.pr.as_ref().filter(|p| p.state != TaskState::Closed);
    if pr.is_none() && matches!(agent, Some(AgentState::Exited { code: Some(c) }) if c != 0) {
        return EpicMemberStatus::Failed;
    }
    if matches!(
        agent,
        Some(AgentState::InputNeeded | AgentState::LimitReached | AgentState::CreditExhausted)
    ) {
        return EpicMemberStatus::Asking;
    }
    if matches!(agent, Some(AgentState::Working)) {
        return EpicMemberStatus::InProgress;
    }
    if let Some(pr) = pr {
        let ci_failing = matches!(
            pr.ci,
            lazybox_core::CiStatus::Failure | lazybox_core::CiStatus::Mixed
        );
        let changes_requested = matches!(pr.review, lazybox_core::ReviewStatus::ChangesRequested);
        let mergeable = pr.ci == lazybox_core::CiStatus::Success
            && pr.mergeable == lazybox_core::Mergeable::Mergeable
            && !changes_requested
            && !pr.merge_blocked;
        if mergeable {
            return EpicMemberStatus::Mergeable {
                held_by: held_by.to_vec(),
            };
        }
        return EpicMemberStatus::PrOpen {
            ci_failing,
            changes_requested,
        };
    }
    if agent.is_none() && has_working_claim(ws) {
        return EpicMemberStatus::Claimed;
    }
    if !blockers.is_empty() {
        return EpicMemberStatus::Blocked;
    }
    EpicMemberStatus::Ready
}

/// A member is "claimed" when a fleet working label sits on one of its tasks
/// but no agent runs locally — another box owns it.
fn has_working_claim(ws: &Workspace) -> bool {
    tasks_of(ws).any(|t| {
        t.labels.iter().any(|l| {
            l.name == lazybox_core::WORKING_LABEL_NAME
                || l.name.starts_with(lazybox_core::WORKING_CLAIM_LABEL_PREFIX)
        })
    })
}

/// Longest dependency path through the DAG, as member keys root→leaf. Empty on
/// a cycle (`order` is short) — a critical path is undefined there.
fn critical_path(
    order: &[WorkspaceKey],
    depends_on: &HashMap<WorkspaceKey, BTreeSet<WorkspaceKey>>,
) -> Vec<WorkspaceKey> {
    let mut best_len: HashMap<WorkspaceKey, u32> = HashMap::new();
    let mut prev: HashMap<WorkspaceKey, Option<WorkspaceKey>> = HashMap::new();
    for node in order {
        let mut len = 1;
        let mut from = None;
        for dep in depends_on.get(node).into_iter().flatten() {
            let cand = best_len.get(dep).copied().unwrap_or(0) + 1;
            if cand > len {
                len = cand;
                from = Some(dep.clone());
            }
        }
        best_len.insert(node.clone(), len);
        prev.insert(node.clone(), from);
    }
    let Some(end) = best_len
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(k, _)| k.clone())
    else {
        return Vec::new();
    };
    let mut path = vec![end.clone()];
    let mut cur = end;
    while let Some(Some(p)) = prev.get(&cur) {
        path.push(p.clone());
        cur = p.clone();
    }
    path.reverse();
    path
}

/// The full typed edge set of an epic's dependency graph — both `Blocks` and
/// `MergeAfter` edges — resolved fresh from the loaded workspaces. A thin
/// public entry point over the internal graph builder, for callers that want
/// the edges without a full status snapshot.
pub fn epic_graph(record: &EpicRecord, workspaces: &[Workspace]) -> Vec<EpicEdge> {
    graph_edges(&resolved_graph(record, workspaces))
}

/// Resolve a full [`EpicSnapshot`] for one epic. `since_latch` is this epic's
/// blocker-age memory (mutated in place: new blockers get `now`, disappeared
/// ones are pruned). `now` is unix-ms.
pub fn resolve(
    record: &EpicRecord,
    workspaces: &[Workspace],
    agent_states: &HashMap<WorkspaceKey, AgentState>,
    declared: &HashMap<WorkspaceKey, DeclaredBlocker>,
    since_latch: &mut HashMap<(WorkspaceKey, BlockerKind, String), i64>,
    now: i64,
) -> EpicSnapshot {
    let resolved = resolved_graph(record, workspaces);
    let (wave, order, cycle) = waves(&resolved.members, &resolved.depends_on);
    let downstream = downstream_counts(&resolved.members, &resolved.depends_on);

    // Blocker-age latch: hand `blockers_for` a closure that reads or seeds the
    // since map, and remember every key we touched so unseen ones get pruned.
    let mut seen_latch: HashSet<(WorkspaceKey, BlockerKind, String)> = HashSet::new();

    let mut members: Vec<EpicMember> = Vec::with_capacity(resolved.members.len());
    for key in &resolved.members {
        let holds = downstream.get(key).copied().unwrap_or(0);
        let in_cycle = cycle.contains(key);
        let blockers = {
            let seen = &mut seen_latch;
            let mut since_at = |k: &WorkspaceKey, kind: BlockerKind, reason: &str| -> i64 {
                let latch_key = (k.clone(), kind, reason.to_string());
                seen.insert(latch_key.clone());
                *since_latch.entry(latch_key).or_insert(now)
            };
            blockers_for(
                key,
                &resolved,
                in_cycle,
                holds,
                declared.get(key),
                &mut since_at,
            )
        };
        let ws = resolved.by_key[key];
        // Merge-after predecessors that haven't settled hold this member's
        // merge. A predecessor settles when it merges (order honored) or its PR
        // is closed without merging (abandoned — the ordering premise is void);
        // either way it stops gating. Keyed off `landing_settled`, not `done`,
        // so a dead predecessor can't hold a successor's merge forever.
        let held_by: Vec<WorkspaceKey> = resolved
            .merge_after
            .get(key)
            .into_iter()
            .flatten()
            .filter(|pred| {
                !resolved
                    .landing_settled
                    .get(*pred)
                    .copied()
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        let status = member_status(ws, agent_states.get(key).copied(), &blockers, &held_by);
        members.push(EpicMember {
            key: key.clone(),
            wave: wave.get(key).copied().unwrap_or(0),
            status,
            blocked_by: resolved
                .depends_on
                .get(key)
                .into_iter()
                .flatten()
                .cloned()
                .collect(),
            external_blockers: resolved
                .external
                .get(key)
                .into_iter()
                .flatten()
                .cloned()
                .collect(),
            blockers,
        });
    }

    // Prune latch entries for blockers that no longer exist, so the map can't
    // grow without bound as reasons churn.
    since_latch.retain(|k, _| seen_latch.contains(k));

    // Wave order for display (then key), independent of resolution order.
    members.sort_by(|a, b| a.wave.cmp(&b.wave).then_with(|| a.key.cmp(&b.key)));

    let total = members.len() as u32;
    let mut done = 0;
    let mut ready = 0;
    let mut blocked = 0;
    let mut asking = 0;
    let mut failing = 0;
    for m in &members {
        match &m.status {
            EpicMemberStatus::Done => done += 1,
            EpicMemberStatus::Ready => ready += 1,
            EpicMemberStatus::Blocked => blocked += 1,
            EpicMemberStatus::Asking => asking += 1,
            EpicMemberStatus::Failed => failing += 1,
            EpicMemberStatus::PrOpen { ci_failing, .. } if *ci_failing => failing += 1,
            _ => {}
        }
    }
    let blockers_needing_operator = members
        .iter()
        .flat_map(|m| &m.blockers)
        .filter(|b| matches!(b.owner, BlockerOwner::Operator))
        .count() as u32;

    EpicSnapshot {
        key: record.key.as_str().to_string(),
        name: record.name.clone(),
        members,
        done,
        total,
        ready,
        blocked,
        asking,
        failing,
        blockers_needing_operator,
        cycle: !cycle.is_empty(),
        critical_path: critical_path(&order, &resolved.depends_on),
        edges: graph_edges(&resolved),
        merge_order: merge_order(&resolved),
        computed_at: now,
    }
}

/// Every typed edge in the resolved graph, deterministically ordered
/// (`from`, `to`, `kind`). A pair joined by both a `Blocks` and a `MergeAfter`
/// edge yields two edges — the DAG view draws them distinctly.
fn graph_edges(resolved: &Resolved) -> Vec<EpicEdge> {
    let mut edges: Vec<EpicEdge> = Vec::new();
    for (from, tos) in &resolved.depends_on {
        for to in tos {
            edges.push(EpicEdge {
                from: from.clone(),
                to: to.clone(),
                kind: EdgeKind::Blocks,
            });
        }
    }
    for (from, tos) in &resolved.merge_after {
        for to in tos {
            edges.push(EpicEdge {
                from: from.clone(),
                to: to.clone(),
                kind: EdgeKind::MergeAfter,
            });
        }
    }
    edges.sort();
    edges
}

/// The epic's PRs in the order they may land: a topological sort of the
/// merge-after graph restricted to members with a live PR, each annotated with
/// the not-yet-landed predecessors holding it. Cycle members (never drained)
/// are appended in key order so the readout still lists them.
fn merge_order(resolved: &Resolved) -> Vec<MergeOrderEntry> {
    let pr_members: Vec<WorkspaceKey> = resolved
        .members
        .iter()
        .filter(|m| resolved.has_pr.get(*m).copied().unwrap_or(false))
        .cloned()
        .collect();
    let pr_set: HashSet<&WorkspaceKey> = pr_members.iter().collect();

    // Edges restricted to PR members (a predecessor with no live PR — already
    // merged, or issue-only — does not sequence the landing).
    let restricted: HashMap<WorkspaceKey, BTreeSet<WorkspaceKey>> = pr_members
        .iter()
        .map(|m| {
            let preds = resolved
                .merge_after
                .get(m)
                .into_iter()
                .flatten()
                .filter(|p| pr_set.contains(p))
                .cloned()
                .collect();
            (m.clone(), preds)
        })
        .collect();

    let (_wave, order, cycle) = waves(&pr_members, &restricted);

    let mut ordered = order.clone();
    // Members caught in a merge-after cycle never drain; append them (key order)
    // so the readout is exhaustive rather than silently dropping them.
    let mut leftover: Vec<WorkspaceKey> = cycle.into_iter().collect();
    leftover.sort();
    for k in leftover {
        if !ordered.contains(&k) {
            ordered.push(k);
        }
    }

    ordered
        .into_iter()
        .map(|key| {
            let held_by: Vec<WorkspaceKey> = resolved
                .merge_after
                .get(&key)
                .into_iter()
                .flatten()
                .filter(|pred| {
                    !resolved
                        .landing_settled
                        .get(*pred)
                        .copied()
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            MergeOrderEntry { key, held_by }
        })
        .collect()
}

/// Compute the deltas from `old` to `new`. `None` old (first sight) yields no
/// deltas — the snapshot itself carries the full state.
pub fn diff(old: Option<&EpicSnapshot>, new: &EpicSnapshot) -> Vec<EpicDelta> {
    let Some(old) = old else {
        return Vec::new();
    };
    let mut deltas: Vec<EpicDelta> = Vec::new();

    let old_members: HashMap<&WorkspaceKey, &EpicMember> =
        old.members.iter().map(|m| (&m.key, m)).collect();

    for m in &new.members {
        let Some(prev) = old_members.get(&m.key) else {
            continue; // a brand-new member shows in the snapshot itself.
        };

        if prev.status != m.status {
            let prev_held = held_preds(&prev.status);
            let new_held = held_preds(&m.status);
            // A member leaving Blocked is reported as Unblocked (with the
            // dependencies that cleared it), which is more useful in the feed
            // than a bare status transition.
            if prev.status == EpicMemberStatus::Blocked && m.status != EpicMemberStatus::Blocked {
                let now_done: Vec<WorkspaceKey> = prev
                    .blocked_by
                    .iter()
                    .filter(|dep| {
                        new.members
                            .iter()
                            .any(|x| &x.key == *dep && x.status == EpicMemberStatus::Done)
                    })
                    .cloned()
                    .collect();
                deltas.push(EpicDelta::Unblocked {
                    key: m.key.clone(),
                    because: now_done,
                });
            } else if new_held.is_some_and(|h| !h.is_empty())
                && !prev_held.is_some_and(|h| !h.is_empty())
            {
                // Newly held: became merge-ready-but-held, or the predecessor
                // set went from empty to non-empty. A held→held change (a
                // predecessor landed but others remain) is not re-announced.
                deltas.push(EpicDelta::Held {
                    key: m.key.clone(),
                    by: new_held.unwrap_or(&[]).to_vec(),
                });
            } else if prev_held.is_some_and(|h| !h.is_empty())
                && new_held.is_some_and(|h| h.is_empty())
            {
                // The last predecessor landed while still merge-ready: released.
                deltas.push(EpicDelta::Released { key: m.key.clone() });
            } else if !(prev_held.is_some() && new_held.is_some()) {
                // Genuine status-class change. Two `Mergeable`s differing only
                // in `held_by` are handled by the Held/Released arms above; any
                // remaining both-`Mergeable` case (e.g. held-set churn) is not a
                // status change worth a delta.
                deltas.push(EpicDelta::StatusChanged {
                    key: m.key.clone(),
                    from: prev.status.clone(),
                    to: m.status.clone(),
                });
            }
        }

        let prev_blockers: HashSet<(BlockerKind, &str)> = prev
            .blockers
            .iter()
            .map(|b| (b.kind, b.reason.as_str()))
            .collect();
        let new_blockers: HashSet<(BlockerKind, &str)> = m
            .blockers
            .iter()
            .map(|b| (b.kind, b.reason.as_str()))
            .collect();
        for b in &m.blockers {
            if !prev_blockers.contains(&(b.kind, b.reason.as_str())) {
                deltas.push(EpicDelta::BlockerAdded {
                    key: m.key.clone(),
                    blocker: b.clone(),
                });
            }
        }
        for b in &prev.blockers {
            if !new_blockers.contains(&(b.kind, b.reason.as_str())) {
                deltas.push(EpicDelta::BlockerCleared {
                    key: m.key.clone(),
                    kind: b.kind,
                    reason: b.reason.clone(),
                });
            }
        }
    }

    // Completed: everything done now, wasn't before.
    let complete = |s: &EpicSnapshot| s.total > 0 && s.done == s.total;
    if complete(new) && !complete(old) {
        deltas.push(EpicDelta::Completed);
    }

    // Stalled: work remains but nothing is actionable (no ready / in-progress /
    // asking / mergeable member), and this is newly true.
    if stalled(new) && !stalled(old) {
        deltas.push(EpicDelta::Stalled {
            reason: if new.cycle {
                "dependency cycle".to_string()
            } else {
                "every remaining member is blocked".to_string()
            },
        });
    }

    deltas
}

/// Work remains but no member can move: nothing ready, in progress, asking, or
/// mergeable.
fn stalled(s: &EpicSnapshot) -> bool {
    if s.total == 0 || s.done == s.total {
        return false;
    }
    !s.members.iter().any(|m| {
        matches!(
            m.status,
            EpicMemberStatus::Ready
                | EpicMemberStatus::InProgress
                | EpicMemberStatus::Asking
                | EpicMemberStatus::Mergeable { .. }
        )
    })
}

/// The `Ready` members of a snapshot, each with how many members it would
/// transitively unblock, ranked unblocks-desc then key. Derived purely from the
/// snapshot's edges — the ranking behind the `epic_ready` MCP tool and the
/// overview's ready queue. Working the highest-unblocks row first frees the most
/// downstream work.
pub fn ready_queue(snapshot: &EpicSnapshot) -> Vec<(WorkspaceKey, u32)> {
    // dependents[X] = members that list X in their `blocked_by` (reverse edges).
    let mut dependents: HashMap<WorkspaceKey, Vec<WorkspaceKey>> = HashMap::new();
    for m in &snapshot.members {
        for dep in &m.blocked_by {
            dependents
                .entry(dep.clone())
                .or_default()
                .push(m.key.clone());
        }
    }
    let mut out: Vec<(WorkspaceKey, u32)> = snapshot
        .members
        .iter()
        .filter(|m| m.status == EpicMemberStatus::Ready)
        .map(|m| {
            let mut seen: HashSet<WorkspaceKey> = HashSet::new();
            let mut stack: Vec<WorkspaceKey> = dependents
                .get(&m.key)
                .into_iter()
                .flatten()
                .cloned()
                .collect();
            while let Some(node) = stack.pop() {
                if node == m.key || !seen.insert(node.clone()) {
                    continue;
                }
                for next in dependents.get(&node).into_iter().flatten() {
                    stack.push(next.clone());
                }
            }
            (m.key.clone(), seen.len() as u32)
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Whether two snapshots carry the same derived status — every field except
/// `computed_at`, which bumps on every recompute and would otherwise make an
/// unchanged epic look changed. Kept field-wise (rather than a clone-and-`==`)
/// so it does no allocation on the hot recompute path; extend it when a new
/// snapshot field is added.
fn same_status(a: &EpicSnapshot, b: &EpicSnapshot) -> bool {
    a.key == b.key
        && a.name == b.name
        && a.done == b.done
        && a.total == b.total
        && a.ready == b.ready
        && a.blocked == b.blocked
        && a.asking == b.asking
        && a.failing == b.failing
        && a.blockers_needing_operator == b.blockers_needing_operator
        && a.cycle == b.cycle
        && a.critical_path == b.critical_path
        && a.edges == b.edges
        && a.merge_order == b.merge_order
        && a.members == b.members
}

// ── epic events → activity feed (step 8) ────────────────────────────────

/// A short human label for a member status, for activity-feed bodies.
fn status_label(s: &EpicMemberStatus) -> &'static str {
    match s {
        EpicMemberStatus::Blocked => "blocked",
        EpicMemberStatus::Ready => "ready",
        EpicMemberStatus::Claimed => "claimed",
        EpicMemberStatus::InProgress => "in progress",
        EpicMemberStatus::Asking => "asking",
        EpicMemberStatus::PrOpen { .. } => "PR open",
        EpicMemberStatus::Mergeable { held_by } if !held_by.is_empty() => "held",
        EpicMemberStatus::Mergeable { .. } => "mergeable",
        EpicMemberStatus::Done => "done",
        EpicMemberStatus::Failed => "failed",
    }
}

/// Map one epic delta to the workspace it concerns and a one-line activity
/// body, or `None` for an epic-wide delta (`Completed` / `Stalled`) that names
/// no single member. Pure so the wording is unit-testable. The daemon turns the
/// result into an [`ActivityKind::StatusChange`] row on that workspace's feed so
/// a derived transition (unblocked, a new blocker, a status move) shows up as
/// unread activity, exactly like a comment or a CI update (#1517 step 8).
fn delta_activity(delta: &EpicDelta, epic_name: &str) -> Option<(WorkspaceKey, String)> {
    match delta {
        EpicDelta::Unblocked { key, because } => {
            let body = if because.is_empty() {
                format!("Unblocked in epic {epic_name}")
            } else {
                let names: Vec<&str> = because.iter().map(WorkspaceKey::as_str).collect();
                format!(
                    "Unblocked in epic {epic_name} — {} cleared",
                    names.join(", ")
                )
            };
            Some((key.clone(), body))
        }
        EpicDelta::StatusChanged { key, from, to } => Some((
            key.clone(),
            format!(
                "Epic {epic_name}: {} → {}",
                status_label(from),
                status_label(to)
            ),
        )),
        EpicDelta::Held { key, by } => {
            let names: Vec<&str> = by.iter().map(WorkspaceKey::as_str).collect();
            Some((
                key.clone(),
                format!(
                    "Merge held in epic {epic_name} — waiting on {}",
                    names.join(", ")
                ),
            ))
        }
        EpicDelta::Released { key } => Some((
            key.clone(),
            format!("Merge released in epic {epic_name} — free to land"),
        )),
        EpicDelta::BlockerAdded { key, blocker } => Some((
            key.clone(),
            format!("Blocked in epic {epic_name} — {}", blocker.reason),
        )),
        EpicDelta::BlockerCleared { key, reason, .. } => Some((
            key.clone(),
            format!("Blocker cleared in epic {epic_name} — {reason}"),
        )),
        // Epic-wide: no single member to attach to. Still rides the
        // `EpicStatus` event; just not an activity row.
        EpicDelta::Completed | EpicDelta::Stalled { .. } => None,
    }
}

/// The merge-after predecessors a `Mergeable` status is held on, if any.
/// `None` for every non-`Mergeable` status; `Some(&[])` for a free merge.
fn held_preds(s: &EpicMemberStatus) -> Option<&[WorkspaceKey]> {
    match s {
        EpicMemberStatus::Mergeable { held_by } => Some(held_by),
        _ => None,
    }
}

/// Build the per-workspace activity rows a batch of freshly-emitted
/// `EpicStatus` events implies: one [`ActivityKind::StatusChange`] row per
/// workspace-keyed delta, grouped by workspace. `now` is the shared recompute
/// timestamp so a row's identity (author + created_at + body) is stable and
/// [`Workspace::merge_activity`]'s content dedupe never double-inserts.
fn activity_rows_for(events: &[Event], now: i64) -> HashMap<WorkspaceKey, Vec<Activity>> {
    let created_at = chrono::DateTime::from_timestamp_millis(now).unwrap_or_else(chrono::Utc::now);
    let mut rows: HashMap<WorkspaceKey, Vec<Activity>> = HashMap::new();
    for event in events {
        let Event::EpicStatus { snapshot, delta } = event else {
            continue;
        };
        for d in delta {
            if let Some((key, body)) = delta_activity(d, &snapshot.name) {
                rows.entry(key).or_default().push(Activity {
                    author: "lazybox".to_string(),
                    body,
                    created_at,
                    kind: ActivityKind::StatusChange,
                    node_id: None,
                    path: None,
                    line: None,
                    diff_hunk: None,
                    thread_id: None,
                });
            }
        }
    }
    rows
}

// ── debounced bus subscriber ────────────────────────────────────────────

/// Whether an event can change any epic snapshot. Agent-state and
/// workspace/task churn matter; `EpicStatus` itself must NOT (that would loop).
fn is_relevant(event: &Event) -> bool {
    matches!(
        event,
        Event::AgentState { .. } | Event::WorkspaceUpserted(_) | Event::WorkspaceRemoved(_)
    )
}

/// Subscribe the resolver to the event bus. Like the other bus subscribers
/// (`stats_accumulator`, `error_inbox`), subscribe here — before the task
/// spawns — so events between this call and the first `recv` queue rather than
/// vanish.
pub fn spawn(config: &ServerConfig) -> tokio::task::JoinHandle<()> {
    let rx = config.bus.subscribe();
    let config = config.clone();
    tokio::spawn(async move { run(rx, config).await })
}

async fn run(mut rx: broadcast::Receiver<Event>, config: ServerConfig) {
    // Compute once at startup so a client that connects before any event still
    // gets current epic status.
    recompute_all(&config).await;

    let mut dirty = false;
    loop {
        let debounce = async {
            if dirty {
                tokio::time::sleep(DEBOUNCE).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            recv = rx.recv() => match recv {
                Ok(event) => {
                    if is_relevant(&event) {
                        dirty = true;
                    }
                }
                // A lagged receiver may have missed a relevant event — recompute
                // to be safe rather than risk a stale snapshot.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("epics: bus lagged, {n} event(s) dropped — recomputing");
                    dirty = true;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = debounce => {
                recompute_all(&config).await;
                dirty = false;
            }
        }
    }
}

/// Recompute every non-archived epic and broadcast the ones whose snapshot
/// changed. The only recompute path.
///
/// Lock discipline: the agent-state snapshot is taken async first; the
/// `EpicMemory` mutex is then held for the whole synchronous
/// load→resolve→diff→store with no `.await`, and bus sends happen after the
/// guard drops.
pub async fn recompute_all(config: &ServerConfig) {
    let records = match list_all(config) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("epics: list failed: {e}");
            return;
        }
    };
    if records.is_empty() {
        return;
    }

    let agent_states = config.terminal.agent_states_by_workspace().await;
    let declared = list_declared(config).unwrap_or_else(|e| {
        tracing::warn!("epics: list declared blockers failed: {e}");
        HashMap::new()
    });
    let workspaces = crate::load_workspaces(&*config.store).values;
    let now = chrono::Utc::now().timestamp_millis();

    let mut to_emit: Vec<Event> = Vec::new();
    {
        let mut memory = config.poll.epics.lock();
        let EpicMemory { since, last } = &mut *memory;
        let live: HashSet<String> = records.iter().map(|r| r.key.as_str().to_string()).collect();

        for record in &records {
            if record.archived {
                continue;
            }
            let latch = since.entry(record.key.as_str().to_string()).or_default();
            let snapshot = resolve(record, &workspaces, &agent_states, &declared, latch, now);
            let prev = last.get(record.key.as_str());
            // `computed_at` bumps every recompute, so equality must ignore it —
            // otherwise every tick looks "changed" and re-broadcasts. Blocker
            // `since` is latched-stable, so the rest of the snapshot only moves
            // on a real change.
            if prev.is_some_and(|p| same_status(p, &snapshot)) {
                continue; // nothing meaningful changed for this epic.
            }
            let delta = diff(prev, &snapshot);
            last.insert(record.key.as_str().to_string(), snapshot.clone());
            to_emit.push(Event::EpicStatus { snapshot, delta });
        }

        // Drop memory for epics that no longer exist (archived rows keep their
        // record but stop recomputing; deleted rows leave `live`).
        last.retain(|k, _| live.contains(k));
        since.retain(|k, _| live.contains(k));
    }

    // Land each workspace-keyed delta as an unread `StatusChange` row on its
    // workspace's activity feed *before* the `EpicStatus` events fire, so a
    // client that reacts to `EpicStatus` by re-reading the workspace already
    // sees the row (#1517 step 8). The write goes through `merge_activity`, so
    // read/seen marks and the content dedupe are honored, and a persisted
    // workspace re-broadcasts as `WorkspaceUpserted`.
    let rows = activity_rows_for(&to_emit, now);
    for (key, acts) in rows {
        // Route each activity write through the race-safe mutation primitive.
        // It locks the workspace, re-loads the *fresh* row, merges, then commits
        // (persist + broadcast as `WorkspaceUpserted`). The old path cloned the
        // stale top-of-function snapshot (`workspaces`, loaded before the async
        // agent-state read and the whole resolve loop) and raw-`save_workspace`d
        // it — so any concurrent poll that wrote fresher CI / review / mergeable
        // state into the row between our load and this write was silently
        // clobbered, dropping real activity. That is exactly the lost-update
        // race `apply_and_commit` exists to close (see `polling::mutate`).
        // A member whose workspace has vanished returns `Missing` and is skipped.
        crate::polling::apply_and_commit(config, &key, |ws| ws.merge_activity(&acts)).await;
    }

    for event in to_emit {
        let _ = config.bus.send(event);
    }
}

// ── on-demand snapshots (MCP read tools) ─────────────────────────────────

/// Resolve a fresh snapshot for every non-archived epic, on demand — the read
/// path behind `epic_status` / `epic_ready`. Uses the shared blocker-age latch
/// so ages match what the bus broadcasts, but never diffs, stores `last`, or
/// emits, so a status query cannot suppress a real change event.
///
/// Same lock discipline as [`recompute_all`]: agent states + declared blockers
/// are read async first, then the `parking_lot` latch is held for the
/// synchronous resolve loop with no `.await` inside.
pub async fn all_snapshots(config: &ServerConfig) -> Vec<EpicSnapshot> {
    let records = match list_all(config) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("epics: list failed: {e}");
            return Vec::new();
        }
    };
    if records.is_empty() {
        return Vec::new();
    }

    let agent_states = config.terminal.agent_states_by_workspace().await;
    let declared = list_declared(config).unwrap_or_else(|e| {
        tracing::warn!("epics: list declared blockers failed: {e}");
        HashMap::new()
    });
    let workspaces = crate::load_workspaces(&*config.store).values;
    let now = chrono::Utc::now().timestamp_millis();

    let mut out = Vec::new();
    {
        let mut memory = config.poll.epics.lock();
        let EpicMemory { since, .. } = &mut *memory;
        for record in &records {
            if record.archived {
                continue;
            }
            let latch = since.entry(record.key.as_str().to_string()).or_default();
            out.push(resolve(
                record,
                &workspaces,
                &agent_states,
                &declared,
                latch,
                now,
            ));
        }
    }
    out
}

// ── merge-after hold (auto-merge integration) ────────────────────────────

/// The unsettled merge-after predecessors of `key` across every active epic —
/// the workspaces whose PRs must merge before `key`'s may. Empty when nothing
/// holds `key`: no epic constrains it, or every predecessor has *settled* — each
/// either merged (order honored) or had its PR closed without merging
/// (abandoned, so it can no longer gate; see `landing_settled_for`). Backs the
/// merge-on-green hold
/// (`auto_merge::on_workspace_committed`) and the manual-merge
/// `force` gate. Deduped and sorted so a workspace held by more than one epic
/// lists each predecessor once.
///
/// Cheap in the common case: with no epic records the prefix scan returns empty
/// before any workspace load.
pub fn held_by(config: &ServerConfig, key: &WorkspaceKey) -> Vec<WorkspaceKey> {
    let records = match list_all(config) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("epics: held_by list failed: {e}");
            return Vec::new();
        }
    };
    if records.is_empty() {
        return Vec::new();
    }
    let workspaces = crate::load_workspaces(&*config.store).values;
    held_by_in(&records, &workspaces, key)
}

/// Pure core of [`held_by`]: the unmerged merge-after predecessors of `key`
/// across `records`, from already-loaded workspaces. Split out so the hold
/// logic unit-tests without a `ServerConfig`.
fn held_by_in(
    records: &[EpicRecord],
    workspaces: &[Workspace],
    key: &WorkspaceKey,
) -> Vec<WorkspaceKey> {
    let mut held: BTreeSet<WorkspaceKey> = BTreeSet::new();
    for record in records {
        if record.archived {
            continue;
        }
        let resolved = resolved_graph(record, workspaces);
        let Some(preds) = resolved.merge_after.get(key) else {
            continue;
        };
        for pred in preds {
            if !resolved.landing_settled.get(pred).copied().unwrap_or(false) {
                held.insert(pred.clone());
            }
        }
    }
    held.into_iter().collect()
}

/// A PR just landed merged (manual, auto, or external) and the store now holds
/// that ground truth. Re-probe every workspace that named `merged` as a
/// merge-after predecessor, so a successor whose *last* predecessor just landed
/// fires its own merge-on-green immediately rather than waiting for the next
/// poll of that workspace. Successors still holding on other predecessors stay
/// held — `auto_merge::on_workspace_committed` recomputes [`held_by`] and finds
/// the remaining ones.
pub fn on_pr_merged(config: &ServerConfig, merged: &WorkspaceKey) {
    let records = match list_all(config) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("epics: on_pr_merged list failed: {e}");
            return;
        }
    };
    if records.is_empty() {
        return;
    }
    let workspaces = crate::load_workspaces(&*config.store).values;
    let by_key: HashMap<WorkspaceKey, &Workspace> =
        workspaces.iter().map(|w| (w.key.clone(), w)).collect();

    for succ in merged_successors(&records, &workspaces, merged) {
        let Some(ws) = by_key.get(&succ) else {
            continue;
        };
        // Re-run the auto-merge projection for the successor. `merged` is now
        // done in the store, so `on_workspace_committed`'s hold check no longer
        // counts it; if that was the last predecessor and the PR is armed +
        // mergeable, the hold lifts and the attempt fires.
        let signal = crate::polling::auto_merge::signal_for(ws);
        crate::polling::auto_merge::on_workspace_committed(config, &succ, signal, true);
    }
}

/// Pure core of [`on_pr_merged`]: every member that named `merged` as a
/// merge-after predecessor, across `records`, deduped and sorted. Split out so
/// the release fan-out unit-tests without a `ServerConfig`.
fn merged_successors(
    records: &[EpicRecord],
    workspaces: &[Workspace],
    merged: &WorkspaceKey,
) -> Vec<WorkspaceKey> {
    let mut successors: BTreeSet<WorkspaceKey> = BTreeSet::new();
    for record in records {
        if record.archived {
            continue;
        }
        let resolved = resolved_graph(record, workspaces);
        for (member, preds) in &resolved.merge_after {
            if member != merged && preds.contains(merged) {
                successors.insert(member.clone());
            }
        }
    }
    successors.into_iter().collect()
}

/// Assemble the spawn-time [`RolePromptCtx`](lazybox_core::prompts::RolePromptCtx)
/// for a role-bearing workspace (#1523 Step 4): the epic it belongs to, the
/// blockers already satisfied (Worker), the wave/merge order (Integrator), and
/// the anchor + planning briefs (Planner). Returns `None` when the workspace
/// carries no role; the resolved [`Role`] rides alongside the ctx so the caller
/// can pick the matching preamble. A role set before the workspace joins an
/// epic still yields `Some` with an epic-less ctx — the preamble degrades to a
/// generic framing rather than being dropped.
pub async fn role_prompt_ctx(
    config: &ServerConfig,
    workspace: &Workspace,
) -> Option<(Role, lazybox_core::prompts::RolePromptCtx)> {
    let role = workspace.effective_role()?;
    let mut ctx = lazybox_core::prompts::RolePromptCtx::default();

    // The Planner authors the graph; its briefs are the `carve` + `designissues`
    // snippet bodies. Injected here because `core` (which shapes the preamble)
    // cannot depend on `config` (which owns the snippet bodies).
    if role == Role::Planner {
        ctx.planner_briefs = ["carve", "designissues"]
            .into_iter()
            .filter_map(lazybox_config::Snippets::builtin_body)
            .collect();
    }

    // Locate the workspace's epic. The snapshot's member list covers explicit
    // assignments (`spawn_worker` / `E c`) *and* anchor-descendant workers; the
    // record fallback catches an assigned member whose workspace did not surface
    // in the snapshot (e.g. a task-less coordinator not yet loaded).
    let records = list_all(config).unwrap_or_default();
    let snapshots = all_snapshots(config).await;
    let snapshot = snapshots
        .iter()
        .find(|s| s.members.iter().any(|m| m.key == workspace.key))
        .or_else(|| {
            records
                .iter()
                .find(|r| r.members.iter().any(|k| k == &workspace.key))
                .and_then(|r| snapshots.iter().find(|s| s.key == r.key.as_str()))
        });

    let Some(snapshot) = snapshot else {
        return Some((role, ctx));
    };
    ctx.epic_key = snapshot.key.clone();
    ctx.epic_name = snapshot.name.clone();
    ctx.anchor_ref = records
        .iter()
        .find(|r| r.key.as_str() == snapshot.key)
        .and_then(|r| r.anchor.as_ref())
        .map(|anchor| anchor.key.clone());

    // Worker: the blockers already satisfied — this member's `blocked_by` edges
    // whose target member is Done (`blocked_by` is every direct dependency,
    // `blockers` only the *unsatisfied* ones, so "resolved" is the difference).
    if role == Role::Worker
        && let Some(me) = snapshot.members.iter().find(|m| m.key == workspace.key)
    {
        let done: HashSet<&WorkspaceKey> = snapshot
            .members
            .iter()
            .filter(|m| m.status == EpicMemberStatus::Done)
            .map(|m| &m.key)
            .collect();
        ctx.resolved_blockers = me
            .blocked_by
            .iter()
            .filter(|b| done.contains(*b))
            .map(|b| b.as_str().to_string())
            .collect();
    }

    // Integrator: land members in wave order (the snapshot is pre-sorted by wave
    // then key), skipping those already merged.
    if role == Role::Integrator {
        ctx.merge_order = snapshot
            .members
            .iter()
            .filter(|m| m.status != EpicMemberStatus::Done)
            .map(|m| m.key.as_str().to_string())
            .collect();
    }

    Some((role, ctx))
}

// ── command handlers ────────────────────────────────────────────────────

/// Record a declared blocker on `workspace` (the caller's own), then recompute
/// so the epic status reflects it immediately. Backs the `report_blocker` MCP
/// tool.
pub async fn report_blocker(
    config: &ServerConfig,
    workspace: WorkspaceKey,
    reason: String,
    kind: BlockerKind,
    owner: BlockerOwner,
) {
    let blocker = DeclaredBlocker {
        workspace,
        reason,
        kind,
        owner,
        since: chrono::Utc::now().timestamp_millis(),
    };
    if let Err(e) = persist_declared(config, &blocker) {
        tracing::warn!(
            "epics: report_blocker for {} failed: {e}",
            blocker.workspace
        );
        return;
    }
    recompute_all(config).await;
}

/// Clear `workspace`'s declared blocker (if any), then recompute. Backs the
/// `clear_blocker` MCP tool.
pub async fn clear_blocker(config: &ServerConfig, workspace: &str) {
    if let Err(e) = clear_declared(config, workspace) {
        tracing::warn!("epics: clear_blocker for {workspace} failed: {e}");
        return;
    }
    recompute_all(config).await;
}

/// Create or overwrite an epic record, then recompute so the new/changed epic
/// broadcasts its first snapshot without waiting for the next bus event.
pub async fn upsert(config: &ServerConfig, record: EpicRecord) {
    if let Err(e) = persist(config, &record) {
        tracing::warn!("epics: upsert {} failed: {e}", record.key);
        return;
    }
    recompute_all(config).await;
}

/// Add or remove a workspace from an epic's explicit membership.
pub async fn assign(config: &ServerConfig, epic: &str, workspace: WorkspaceKey, member: bool) {
    let Some(mut record) = load(config, epic).unwrap_or_else(|e| {
        tracing::warn!("epics: load {epic} failed: {e}");
        None
    }) else {
        tracing::warn!("epics: assign to unknown epic {epic}");
        return;
    };
    let present = record.members.iter().any(|k| k == &workspace);
    match (member, present) {
        (true, false) => record.members.push(workspace),
        (false, true) => record.members.retain(|k| k != &workspace),
        _ => return, // already in the desired state.
    }
    upsert(config, record).await;
}

/// Mark an epic archived. Archived epics keep their record (so an un-archive is
/// possible) but stop broadcasting status.
pub async fn archive(config: &ServerConfig, epic: &str) {
    let Some(mut record) = load(config, epic).unwrap_or_else(|e| {
        tracing::warn!("epics: load {epic} failed: {e}");
        None
    }) else {
        return;
    };
    if record.archived {
        return;
    }
    record.archived = true;
    if let Err(e) = persist(config, &record) {
        tracing::warn!("epics: archive {epic} failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use lazybox_core::{CiStatus, EpicKey, Mergeable, ReviewStatus};

    fn ws(key: &str) -> Workspace {
        // `Workspace::empty` names the workspace after its key, which is what
        // the resolver's blocker reasons render — good enough for tests.
        Workspace::empty(WorkspaceKey::new(key), "branch", Utc::now())
    }

    /// A minimal open task. `Task` has no `Default`, so the fields are spelled
    /// out once here and tests mutate the few they care about.
    fn task(source: &str, key: &str) -> Task {
        Task {
            id: TaskId {
                source: source.into(),
                key: key.into(),
            },
            title: "t".into(),
            body: None,
            state: TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://example.test/1".into(),
            repo: None,
            branch: None,
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            approval_policy: Default::default(),
            assignees: vec![],
            author: String::new(),
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            closes_issues: vec![],
            linked_tasks: vec![],
            blocked_by: vec![],
            merge_after: vec![],
            blocked_on: None,
            parent: None,
            kind: None,
            priority: None,
            state_label: None,
        }
    }

    fn pr(ws: &mut Workspace, state: TaskState, ci: CiStatus) {
        let mut t = task("github", &format!("{}#pr", ws.key.as_str()));
        t.state = state;
        t.ci = ci;
        t.mergeable = Mergeable::Mergeable;
        ws.pr = Some(t);
    }

    fn record_with(members: &[&str]) -> EpicRecord {
        let mut r = EpicRecord::new(EpicKey::new("e"), "Epic", Utc::now());
        r.members = members.iter().map(|k| WorkspaceKey::new(*k)).collect();
        r
    }

    fn resolve_fresh(record: &EpicRecord, workspaces: &[Workspace]) -> EpicSnapshot {
        let mut latch = HashMap::new();
        resolve(
            record,
            workspaces,
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            1_000,
        )
    }

    #[test]
    fn empty_epic_resolves_to_zero_members() {
        let snap = resolve_fresh(&record_with(&[]), &[]);
        assert_eq!(snap.total, 0);
        assert!(snap.members.is_empty());
        assert!(!snap.cycle);
    }

    #[test]
    fn explicit_members_resolve_only_when_loaded() {
        let workspaces = vec![ws("a")];
        // "b" is a member on paper but has no loaded workspace → excluded.
        let snap = resolve_fresh(&record_with(&["a", "b"]), &workspaces);
        assert_eq!(snap.total, 1);
        assert_eq!(snap.members[0].key.as_str(), "a");
    }

    #[test]
    fn anchor_membership_walks_the_parent_chain() {
        let anchor = TaskId {
            source: "github".into(),
            key: "o/r#100".into(),
        };
        // child issue whose parent is the anchor.
        let mut child = ws("child");
        let mut child_task = task("github", "o/r#101");
        child_task.parent = Some(anchor.clone());
        child.gh_issues = vec![child_task];
        // grandchild whose parent is the child issue (transitive).
        let mut grand = ws("grand");
        let mut grand_task = task("github", "o/r#102");
        grand_task.parent = Some(TaskId {
            source: "github".into(),
            key: "o/r#101".into(),
        });
        grand.gh_issues = vec![grand_task];
        // unrelated workspace.
        let other = ws("other");

        let mut r = EpicRecord::new(EpicKey::new("e"), "Epic", Utc::now());
        r.anchor = Some(anchor);
        let snap = resolve_fresh(&r, &[child, grand, other]);
        let keys: Vec<&str> = snap.members.iter().map(|m| m.key.as_str()).collect();
        assert!(keys.contains(&"child"));
        assert!(keys.contains(&"grand"));
        assert!(!keys.contains(&"other"));
    }

    #[test]
    fn dependency_edges_and_waves_level_correctly() {
        let mut a = ws("a");
        let mut b = ws("b");
        let mut c = ws("c");
        // b depends on a, c depends on b: a(0) → b(1) → c(2).
        let mut b_issue = task("github", "b#1");
        b_issue.blocked_by = vec![task("github", "a#pr").id];
        b.gh_issues = vec![b_issue];
        let mut c_issue = task("github", "c#1");
        c_issue.blocked_by = vec![task("github", "b#pr").id];
        c.gh_issues = vec![c_issue];
        // Give a and b the PRs their downstream edges point at.
        pr(&mut a, TaskState::Open, CiStatus::Pending);
        pr(&mut b, TaskState::Open, CiStatus::Pending);

        let snap = resolve_fresh(&record_with(&["a", "b", "c"]), &[a, b, c]);
        let wave = |k: &str| {
            snap.members
                .iter()
                .find(|m| m.key.as_str() == k)
                .unwrap()
                .wave
        };
        assert_eq!(wave("a"), 0);
        assert_eq!(wave("b"), 1);
        assert_eq!(wave("c"), 2);
        assert_eq!(snap.critical_path.len(), 3);
        assert!(!snap.cycle);
    }

    #[test]
    fn a_cycle_is_detected_and_flagged() {
        let mut a = ws("a");
        let mut b = ws("b");
        let mut a_issue = task("github", "a#1");
        a_issue.blocked_by = vec![task("github", "b#pr").id];
        a.gh_issues = vec![a_issue];
        let mut b_issue = task("github", "b#1");
        b_issue.blocked_by = vec![task("github", "a#pr").id];
        b.gh_issues = vec![b_issue];
        pr(&mut a, TaskState::Open, CiStatus::Pending);
        pr(&mut b, TaskState::Open, CiStatus::Pending);

        let snap = resolve_fresh(&record_with(&["a", "b"]), &[a, b]);
        assert!(snap.cycle);
        assert!(snap.critical_path.is_empty());
        assert!(
            snap.members
                .iter()
                .all(|m| m.blockers.iter().any(|b| b.kind == BlockerKind::Cycle))
        );
    }

    #[test]
    fn done_dependency_no_longer_blocks() {
        let mut a = ws("a");
        pr(&mut a, TaskState::Merged, CiStatus::Success); // a is done.
        let mut b = ws("b");
        let mut b_issue = task("github", "b#1");
        b_issue.blocked_by = vec![task("github", "a#pr").id];
        b.gh_issues = vec![b_issue];

        let snap = resolve_fresh(&record_with(&["a", "b"]), &[a, b]);
        let bm = snap.members.iter().find(|m| m.key.as_str() == "b").unwrap();
        // Edge still recorded, but no active dependency blocker → Ready.
        assert!(bm.blocked_by.iter().any(|k| k.as_str() == "a"));
        assert!(
            bm.blockers
                .iter()
                .all(|bk| bk.kind != BlockerKind::Dependency)
        );
        assert_eq!(bm.status, EpicMemberStatus::Ready);
    }

    #[test]
    fn status_precedence_holds() {
        // Merged PR → Done.
        let mut done = ws("done");
        pr(&mut done, TaskState::Merged, CiStatus::Success);
        // Open PR, green, mergeable → Mergeable.
        let mut green = ws("green");
        pr(&mut green, TaskState::Open, CiStatus::Success);
        // Open PR, failing CI → PrOpen{ci_failing}.
        let mut failing = ws("failing");
        pr(&mut failing, TaskState::Open, CiStatus::Failure);

        let workspaces = vec![done, green, failing];
        let mut latch = HashMap::new();
        let mut states = HashMap::new();
        states.insert(WorkspaceKey::new("green"), AgentState::Idle); // idle ≠ working/asking.
        let snap = resolve(
            &record_with(&["done", "green", "failing"]),
            &workspaces,
            &states,
            &HashMap::new(),
            &mut latch,
            1,
        );
        let st = |k: &str| {
            snap.members
                .iter()
                .find(|m| m.key.as_str() == k)
                .unwrap()
                .status
                .clone()
        };
        assert_eq!(st("done"), EpicMemberStatus::Done);
        assert_eq!(st("green"), EpicMemberStatus::Mergeable { held_by: vec![] });
        assert_eq!(
            st("failing"),
            EpicMemberStatus::PrOpen {
                ci_failing: true,
                changes_requested: false
            }
        );
        assert_eq!(snap.failing, 1);
        assert_eq!(snap.done, 1);
    }

    #[test]
    fn agent_state_beats_pr_status() {
        let mut w = ws("w");
        pr(&mut w, TaskState::Open, CiStatus::Success); // would be Mergeable…
        let mut states = HashMap::new();
        states.insert(WorkspaceKey::new("w"), AgentState::Working);
        let mut latch = HashMap::new();
        let snap = resolve(
            &record_with(&["w"]),
            &[w],
            &states,
            &HashMap::new(),
            &mut latch,
            1,
        );
        // …but a working agent wins.
        assert_eq!(snap.members[0].status, EpicMemberStatus::InProgress);
    }

    #[test]
    fn declared_blocker_from_blocked_on_and_label() {
        let mut w = ws("w");
        let mut t = task("github", "w#1");
        t.blocked_on = Some("waiting on legal sign-off".into());
        t.labels = vec![lazybox_core::Label::new("blocked:decision")];
        w.gh_issues = vec![t];

        let snap = resolve_fresh(&record_with(&["w"]), &[w]);
        let m = &snap.members[0];
        assert_eq!(m.status, EpicMemberStatus::Blocked);
        let b = m
            .blockers
            .iter()
            .find(|b| b.kind == BlockerKind::Decision)
            .expect("decision blocker");
        assert_eq!(b.reason, "waiting on legal sign-off");
        assert!(matches!(b.owner, BlockerOwner::Operator));
        assert_eq!(snap.blockers_needing_operator, 1);
    }

    #[test]
    fn external_blocker_when_dependency_is_not_a_member() {
        let mut w = ws("w");
        let mut t = task("github", "w#1");
        t.blocked_by = vec![task("github", "other/repo#9").id];
        w.gh_issues = vec![t];

        let snap = resolve_fresh(&record_with(&["w"]), &[w]);
        let m = &snap.members[0];
        assert_eq!(m.external_blockers.len(), 1);
        assert!(m.blockers.iter().any(|b| b.kind == BlockerKind::External));
        assert_eq!(m.status, EpicMemberStatus::Blocked);
    }

    #[test]
    fn self_owned_blocker_is_not_external() {
        // A member whose task is `blocked_by` another task in its *own*
        // workspace must not surface that as an external blocker: it's internal
        // to the member's own work, not an epic-level wait. Two issues in one
        // workspace, one blocked by the other, no PR, no agent → Ready with no
        // blockers (before the fix it resolved as a spurious External → Blocked).
        let mut w = ws("w");
        let mut first = task("github", "w#1");
        let second = task("github", "w#2");
        first.blocked_by = vec![second.id.clone()];
        w.gh_issues = vec![first, second];

        let snap = resolve_fresh(&record_with(&["w"]), &[w]);
        let m = &snap.members[0];
        assert!(
            m.external_blockers.is_empty(),
            "own task must not be an external blocker: {:?}",
            m.external_blockers
        );
        assert!(m.blockers.is_empty(), "no blockers: {:?}", m.blockers);
        assert_eq!(m.status, EpicMemberStatus::Ready);
    }

    #[test]
    fn since_is_latched_across_recomputes() {
        let mut w = ws("w");
        let mut t = task("github", "w#1");
        t.blocked_on = Some("decide".into());
        w.gh_issues = vec![t];
        let workspaces = vec![w];
        let record = record_with(&["w"]);

        let mut latch = HashMap::new();
        let first = resolve(
            &record,
            &workspaces,
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            500,
        );
        assert_eq!(first.members[0].blockers[0].since, 500);
        // A later recompute must keep the original since, not stamp `now`.
        let second = resolve(
            &record,
            &workspaces,
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            9_999,
        );
        assert_eq!(second.members[0].blockers[0].since, 500);
    }

    #[test]
    fn latch_prunes_disappeared_blockers() {
        let record = record_with(&["w"]);
        let mut blocked = ws("w");
        let mut t = task("github", "w#1");
        t.blocked_on = Some("decide".into());
        blocked.gh_issues = vec![t];

        let mut latch = HashMap::new();
        let _ = resolve(
            &record,
            &[blocked],
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            1,
        );
        assert_eq!(latch.len(), 1);
        // Same workspace, blocker gone → latch pruned.
        let clear = ws("w");
        let _ = resolve(
            &record,
            &[clear],
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            2,
        );
        assert!(latch.is_empty());
    }

    #[test]
    fn diff_reports_unblock_and_completion() {
        let record = record_with(&["a", "b"]);
        // a done; b blocked on a (open) → b Blocked.
        let mut a = ws("a");
        pr(&mut a, TaskState::Open, CiStatus::Pending);
        let mut b = ws("b");
        let mut b_issue = task("github", "b#1");
        b_issue.blocked_by = vec![task("github", "a#pr").id];
        b.gh_issues = vec![b_issue];
        let mut latch = HashMap::new();
        let before = resolve(
            &record,
            &[a.clone(), b.clone()],
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            1,
        );
        assert_eq!(
            before
                .members
                .iter()
                .find(|m| m.key.as_str() == "b")
                .unwrap()
                .status,
            EpicMemberStatus::Blocked
        );

        // a merges → b unblocks; both done-ish (a done, b ready).
        pr(&mut a, TaskState::Merged, CiStatus::Success);
        let after = resolve(
            &record,
            &[a, b],
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            2,
        );
        let deltas = diff(Some(&before), &after);
        assert!(deltas.iter().any(|d| matches!(
            d,
            EpicDelta::Unblocked { key, because } if key.as_str() == "b" && because.iter().any(|k| k.as_str() == "a")
        )));
    }

    #[test]
    fn diff_reports_completion() {
        let record = record_with(&["a"]);
        let mut open = ws("a");
        pr(&mut open, TaskState::Open, CiStatus::Pending);
        let mut latch = HashMap::new();
        let before = resolve(
            &record,
            &[open],
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            1,
        );

        let mut merged = ws("a");
        pr(&mut merged, TaskState::Merged, CiStatus::Success);
        let after = resolve(
            &record,
            &[merged],
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            2,
        );
        let deltas = diff(Some(&before), &after);
        assert!(deltas.iter().any(|d| matches!(d, EpicDelta::Completed)));
    }

    /// A PR whose body carries an explicit `Merge after: …` marker gets a
    /// `MergeAfter` edge to that member — a landing-order edge with no work
    /// dependency, so no `Blocks` edge accompanies it.
    #[test]
    fn explicit_merge_after_marker_creates_edge() {
        let mut a = ws("a");
        pr(&mut a, TaskState::Open, CiStatus::Pending);
        let mut b = ws("b");
        let mut b_pr = task("github", "b#pr");
        b_pr.merge_after = vec![task("github", "a#pr").id];
        b.pr = Some(b_pr);

        let mut record = record_with(&["a", "b"]);
        // Isolate the explicit marker from the implied-edge path.
        record.implied_merge_after = false;
        let edges = epic_graph(&record, &[a, b]);
        assert!(edges.iter().any(|e| e.from.as_str() == "b"
            && e.to.as_str() == "a"
            && e.kind == EdgeKind::MergeAfter));
        assert!(!edges.iter().any(|e| e.kind == EdgeKind::Blocks));
    }

    /// A `Blocks` edge implies a `MergeAfter` edge under the default opt-in:
    /// the dependent's PR must not land before the PR it depends on.
    #[test]
    fn blocks_edge_implies_merge_after() {
        let mut a = ws("a");
        pr(&mut a, TaskState::Open, CiStatus::Pending);
        let mut b = ws("b");
        let mut b_issue = task("github", "b#1");
        b_issue.blocked_by = vec![task("github", "a#pr").id];
        b.gh_issues = vec![b_issue];

        let edges = epic_graph(&record_with(&["a", "b"]), &[a, b]);
        assert!(
            edges.iter().any(|e| e.from.as_str() == "b"
                && e.to.as_str() == "a"
                && e.kind == EdgeKind::Blocks)
        );
        assert!(edges.iter().any(|e| e.from.as_str() == "b"
            && e.to.as_str() == "a"
            && e.kind == EdgeKind::MergeAfter));
    }

    /// Opting out (`implied_merge_after = false`) keeps the work `Blocks` edge
    /// but drops the implied landing-order edge — the members may merge in any
    /// order despite the work ordering.
    #[test]
    fn implied_merge_after_opt_out_drops_implied_edge() {
        let mut a = ws("a");
        pr(&mut a, TaskState::Open, CiStatus::Pending);
        let mut b = ws("b");
        let mut b_issue = task("github", "b#1");
        b_issue.blocked_by = vec![task("github", "a#pr").id];
        b.gh_issues = vec![b_issue];

        let mut record = record_with(&["a", "b"]);
        record.implied_merge_after = false;
        let edges = epic_graph(&record, &[a, b]);
        assert!(edges.iter().any(|e| e.kind == EdgeKind::Blocks));
        assert!(!edges.iter().any(|e| e.kind == EdgeKind::MergeAfter));
    }

    /// `merge_order` topologically sorts the PRs by their merge-after edges and
    /// annotates each held entry with the not-yet-landed predecessors gating it.
    #[test]
    fn merge_order_lists_prs_topologically_and_marks_held() {
        // a → b → c landing chain, all three with open PRs.
        let mut a = ws("a");
        pr(&mut a, TaskState::Open, CiStatus::Success);
        let mut b = ws("b");
        let mut b_pr = task("github", "b#pr");
        b_pr.merge_after = vec![task("github", "a#pr").id];
        b.pr = Some(b_pr);
        let mut c = ws("c");
        let mut c_pr = task("github", "c#pr");
        c_pr.merge_after = vec![task("github", "b#pr").id];
        c.pr = Some(c_pr);

        let mut record = record_with(&["a", "b", "c"]);
        record.implied_merge_after = false;
        let snap = resolve_fresh(&record, &[a, b, c]);
        let order: Vec<&str> = snap.merge_order.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(order, vec!["a", "b", "c"]);
        let held = |k: &str| -> Vec<String> {
            snap.merge_order
                .iter()
                .find(|e| e.key.as_str() == k)
                .unwrap()
                .held_by
                .iter()
                .map(|w| w.as_str().to_string())
                .collect()
        };
        assert!(held("a").is_empty());
        assert_eq!(held("b"), vec!["a".to_string()]);
        assert_eq!(held("c"), vec!["b".to_string()]);
    }

    /// The merge readout sequences only *live* PRs: a merged predecessor no
    /// longer holds its dependent (and drops out of the order), and an
    /// issue-only member with no PR never appears.
    #[test]
    fn merge_order_drops_landed_predecessor_and_issue_only_members() {
        let mut a = ws("a");
        pr(&mut a, TaskState::Merged, CiStatus::Success); // landed → no live PR.
        let mut b = ws("b");
        let mut b_pr = task("github", "b#pr");
        b_pr.merge_after = vec![task("github", "a#pr").id];
        b.pr = Some(b_pr);
        let mut c = ws("c");
        c.gh_issues = vec![task("github", "c#1")]; // issue only, no PR.

        let mut record = record_with(&["a", "b", "c"]);
        record.implied_merge_after = false;
        let snap = resolve_fresh(&record, &[a, b, c]);
        let order: Vec<&str> = snap.merge_order.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(order, vec!["b"]);
        assert!(snap.merge_order[0].held_by.is_empty());
    }

    fn keys(v: Vec<WorkspaceKey>) -> Vec<String> {
        v.iter().map(|k| k.as_str().to_string()).collect()
    }

    /// `held_by_in` (the pure core of the auto-merge hold and the manual
    /// `force` gate) reports a dependent's not-yet-landed merge-after
    /// predecessors, and nothing once every predecessor has landed.
    #[test]
    fn held_by_in_reports_unmerged_predecessors() {
        let mut a = ws("a");
        pr(&mut a, TaskState::Open, CiStatus::Success);
        let mut b = ws("b");
        let mut b_pr = task("github", "b#pr");
        b_pr.merge_after = vec![task("github", "a#pr").id];
        b.pr = Some(b_pr);

        let mut record = record_with(&["a", "b"]);
        record.implied_merge_after = false;
        let records = [record];

        // a's PR is open → it holds b; a has no predecessors → unheld.
        assert_eq!(
            keys(held_by_in(
                &records,
                &[a.clone(), b.clone()],
                &WorkspaceKey::new("b")
            )),
            vec!["a".to_string()]
        );
        assert!(held_by_in(&records, &[a.clone(), b.clone()], &WorkspaceKey::new("a")).is_empty());

        // a lands → b is released.
        pr(&mut a, TaskState::Merged, CiStatus::Success);
        assert!(held_by_in(&records, &[a, b], &WorkspaceKey::new("b")).is_empty());
    }

    /// A predecessor whose PR is **closed without merging** is abandoned: it
    /// will never land, so the "b merges after a" ordering premise is void and b
    /// must be released. Regression for the indefinite-hold bug — the hold used
    /// to key off `member_done` (merged-only for PRs), so a closed predecessor
    /// stayed `!done` forever and held b's merge (auto-merge silently never
    /// fired, manual `g m` kept being refused) until a manual force-override.
    #[test]
    fn held_by_in_releases_when_predecessor_pr_closed_unmerged() {
        let mut a = ws("a");
        pr(&mut a, TaskState::Closed, CiStatus::Failure); // abandoned, never merged.
        let mut b = ws("b");
        let mut b_pr = task("github", "b#pr");
        b_pr.merge_after = vec![task("github", "a#pr").id];
        b.pr = Some(b_pr);

        let mut record = record_with(&["a", "b"]);
        record.implied_merge_after = false;

        // a can never land, so it no longer gates b — b is free to merge.
        assert!(
            held_by_in(&[record], &[a, b], &WorkspaceKey::new("b")).is_empty(),
            "a closed-unmerged predecessor must not hold its successor's merge",
        );
    }

    /// A dependent held by two predecessors stays held until BOTH land — one
    /// merging leaves the other in `held_by`.
    #[test]
    fn held_by_in_needs_all_predecessors_landed() {
        let mut a = ws("a");
        pr(&mut a, TaskState::Merged, CiStatus::Success); // one landed…
        let mut x = ws("x");
        pr(&mut x, TaskState::Open, CiStatus::Success); // …one still open.
        let mut b = ws("b");
        let mut b_pr = task("github", "b#pr");
        b_pr.merge_after = vec![task("github", "a#pr").id, task("github", "x#pr").id];
        b.pr = Some(b_pr);

        let mut record = record_with(&["a", "x", "b"]);
        record.implied_merge_after = false;
        assert_eq!(
            keys(held_by_in(&[record], &[a, x, b], &WorkspaceKey::new("b"))),
            vec!["x".to_string()]
        );
    }

    /// An archived epic never holds a merge — its edges are inert.
    #[test]
    fn held_by_in_ignores_archived_epic() {
        let mut a = ws("a");
        pr(&mut a, TaskState::Open, CiStatus::Success);
        let mut b = ws("b");
        let mut b_pr = task("github", "b#pr");
        b_pr.merge_after = vec![task("github", "a#pr").id];
        b.pr = Some(b_pr);

        let mut record = record_with(&["a", "b"]);
        record.implied_merge_after = false;
        record.archived = true;
        assert!(held_by_in(&[record], &[a, b], &WorkspaceKey::new("b")).is_empty());
    }

    /// `merged_successors` (the pure core of the release fan-out) finds every
    /// member that named the just-landed workspace as a merge-after predecessor
    /// — the keys `on_pr_merged` re-probes.
    #[test]
    fn merged_successors_finds_direct_dependents() {
        // a → b → c chain.
        let mut a = ws("a");
        pr(&mut a, TaskState::Merged, CiStatus::Success);
        let mut b = ws("b");
        let mut b_pr = task("github", "b#pr");
        b_pr.merge_after = vec![task("github", "a#pr").id];
        b.pr = Some(b_pr);
        let mut c = ws("c");
        let mut c_pr = task("github", "c#pr");
        c_pr.merge_after = vec![task("github", "b#pr").id];
        c.pr = Some(c_pr);

        let mut record = record_with(&["a", "b", "c"]);
        record.implied_merge_after = false;
        let records = [record];
        let all = [a, b, c];

        // Only b named a; c named b, not a — the release is one hop, not
        // transitive (c re-probes when b later lands).
        assert_eq!(
            keys(merged_successors(&records, &all, &WorkspaceKey::new("a"))),
            vec!["b".to_string()]
        );
        assert_eq!(
            keys(merged_successors(&records, &all, &WorkspaceKey::new("b"))),
            vec!["c".to_string()]
        );
        assert!(merged_successors(&records, &all, &WorkspaceKey::new("c")).is_empty());
    }

    /// A merge-ready PR whose merge-after predecessor hasn't landed reads as
    /// `Mergeable { held_by }`; when the predecessor merges it becomes an
    /// unheld `Mergeable` and `diff` reports a `Released`.
    #[test]
    fn held_then_released_diff_transitions() {
        let mut record = record_with(&["a", "b"]);
        record.implied_merge_after = false;
        let mut a = ws("a");
        pr(&mut a, TaskState::Open, CiStatus::Success);
        let mut b = ws("b");
        let mut b_pr = task("github", "b#pr");
        b_pr.merge_after = vec![task("github", "a#pr").id];
        b_pr.ci = CiStatus::Success;
        b.pr = Some(b_pr);

        let mut latch = HashMap::new();
        let before = resolve(
            &record,
            &[a.clone(), b.clone()],
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            1,
        );
        let bm = before
            .members
            .iter()
            .find(|m| m.key.as_str() == "b")
            .unwrap();
        assert_eq!(
            bm.status,
            EpicMemberStatus::Mergeable {
                held_by: vec![WorkspaceKey::new("a")]
            }
        );

        // a merges → b released (unheld Mergeable).
        pr(&mut a, TaskState::Merged, CiStatus::Success);
        let after = resolve(
            &record,
            &[a, b],
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            2,
        );
        let bm2 = after
            .members
            .iter()
            .find(|m| m.key.as_str() == "b")
            .unwrap();
        assert_eq!(bm2.status, EpicMemberStatus::Mergeable { held_by: vec![] });
        let deltas = diff(Some(&before), &after);
        assert!(
            deltas
                .iter()
                .any(|d| matches!(d, EpicDelta::Released { key } if key.as_str() == "b"))
        );
    }

    /// A PR that first becomes merge-ready while its predecessor is still open
    /// emits a `Held` delta naming the predecessor.
    #[test]
    fn newly_held_emits_held_delta() {
        let mut record = record_with(&["a", "b"]);
        record.implied_merge_after = false;
        let mut a = ws("a");
        pr(&mut a, TaskState::Open, CiStatus::Success);
        let make_b = |ci: CiStatus| {
            let mut b = ws("b");
            let mut b_pr = task("github", "b#pr");
            b_pr.merge_after = vec![task("github", "a#pr").id];
            b_pr.ci = ci;
            b.pr = Some(b_pr);
            b
        };

        let mut latch = HashMap::new();
        // b failing CI first → PrOpen, not yet held.
        let before = resolve(
            &record,
            &[a.clone(), make_b(CiStatus::Failure)],
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            1,
        );
        // b green now → merge-ready but held by a's still-open PR.
        let after = resolve(
            &record,
            &[a, make_b(CiStatus::Success)],
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            2,
        );
        let deltas = diff(Some(&before), &after);
        assert!(deltas.iter().any(|d| matches!(
            d,
            EpicDelta::Held { key, by } if key.as_str() == "b" && by.iter().any(|w| w.as_str() == "a")
        )));
    }

    #[test]
    fn closed_unmerged_pr_is_not_done_and_does_not_complete_the_epic() {
        // A PR *closed without merging* is abandoned work, not a completion. It
        // must not read as `Done`, must not fire the epic-wide `Completed`, and
        // must not masquerade as a live `PrOpen` — it falls through to `Ready`,
        // the honest "this member still needs a completed deliverable."
        let record = record_with(&["a"]);
        let mut open = ws("a");
        pr(&mut open, TaskState::Open, CiStatus::Pending);
        let mut latch = HashMap::new();
        let before = resolve(
            &record,
            &[open],
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            1,
        );

        let mut closed = ws("a");
        pr(&mut closed, TaskState::Closed, CiStatus::Failure); // abandoned, unmerged.
        let after = resolve(
            &record,
            &[closed],
            &HashMap::new(),
            &HashMap::new(),
            &mut latch,
            2,
        );

        let m = &after.members[0];
        assert_eq!(
            m.status,
            EpicMemberStatus::Ready,
            "a closed-unmerged PR is neither Done nor PrOpen"
        );
        assert_eq!(after.done, 0, "an abandoned PR does not count as done");
        let deltas = diff(Some(&before), &after);
        assert!(
            !deltas.iter().any(|d| matches!(d, EpicDelta::Completed)),
            "closing a PR without merging must not complete the epic: {deltas:?}"
        );
    }

    #[test]
    fn first_snapshot_has_no_deltas() {
        let record = record_with(&["a"]);
        let a = ws("a");
        let snap = resolve_fresh(&record, &[a]);
        assert!(diff(None, &snap).is_empty());
    }

    #[test]
    fn holds_counts_downstream_members() {
        // a ← b ← c and a ← d: a holds b, c, d (3).
        let mut a = ws("a");
        pr(&mut a, TaskState::Open, CiStatus::Pending);
        let edge_to = |from: &str| {
            let mut issue = task("github", &format!("{from}#1"));
            issue.blocked_by = vec![task("github", &format!("{from}#pr")).id];
            issue
        };
        let _ = edge_to; // (helper illustrative; explicit below)

        let mut b = ws("b");
        let mut b_issue = task("github", "b#1");
        b_issue.blocked_by = vec![task("github", "a#pr").id];
        b.gh_issues = vec![b_issue];
        pr(&mut b, TaskState::Open, CiStatus::Pending);

        let mut c = ws("c");
        let mut c_issue = task("github", "c#1");
        c_issue.blocked_by = vec![task("github", "b#pr").id];
        c.gh_issues = vec![c_issue];

        let mut d = ws("d");
        let mut d_issue = task("github", "d#1");
        d_issue.blocked_by = vec![task("github", "a#pr").id];
        d.gh_issues = vec![d_issue];

        let snap = resolve_fresh(&record_with(&["a", "b", "c", "d"]), &[a, b, c, d]);
        // b's dependency blocker on a holds b's downstream (c) = 1.
        let bm = snap.members.iter().find(|m| m.key.as_str() == "b").unwrap();
        let dep = bm
            .blockers
            .iter()
            .find(|bk| bk.kind == BlockerKind::Dependency)
            .unwrap();
        assert_eq!(dep.holds, 1);
    }

    #[test]
    fn declared_blocker_is_a_first_class_source() {
        // A workspace with no PR, no agent, no graph edge is Ready — until its
        // worker declares a blocker via `report_blocker`. Then it's Blocked with
        // that blocker surfaced, carrying the declared kind, owner, and its own
        // persisted `since` (not the latch's `now`).
        let record = record_with(&["w"]);
        let mut declared = HashMap::new();
        declared.insert(
            WorkspaceKey::new("w"),
            DeclaredBlocker {
                workspace: WorkspaceKey::new("w"),
                reason: "need the API contract nailed down".into(),
                kind: BlockerKind::Contract,
                owner: BlockerOwner::Agent(WorkspaceKey::new("w")),
                since: 4_242,
            },
        );
        let mut latch = HashMap::new();
        let snap = resolve(
            &record,
            &[ws("w")],
            &HashMap::new(),
            &declared,
            &mut latch,
            9_999,
        );
        let m = &snap.members[0];
        assert_eq!(m.status, EpicMemberStatus::Blocked);
        let b = m
            .blockers
            .iter()
            .find(|b| b.kind == BlockerKind::Contract)
            .expect("declared contract blocker");
        assert_eq!(b.reason, "need the API contract nailed down");
        assert!(matches!(b.owner, BlockerOwner::Agent(ref k) if k.as_str() == "w"));
        // Uses the persisted `since`, not `now` — trustworthy across restarts.
        assert_eq!(b.since, 4_242);
    }

    #[test]
    fn declared_blocker_with_blank_reason_is_ignored() {
        let record = record_with(&["w"]);
        let mut declared = HashMap::new();
        declared.insert(
            WorkspaceKey::new("w"),
            DeclaredBlocker {
                workspace: WorkspaceKey::new("w"),
                reason: "   ".into(),
                kind: BlockerKind::Decision,
                owner: BlockerOwner::Operator,
                since: 1,
            },
        );
        let mut latch = HashMap::new();
        let snap = resolve(
            &record,
            &[ws("w")],
            &HashMap::new(),
            &declared,
            &mut latch,
            1,
        );
        assert!(snap.members[0].blockers.is_empty());
        assert_eq!(snap.members[0].status, EpicMemberStatus::Ready);
    }

    #[test]
    fn ready_queue_ranks_by_transitive_unblocks() {
        // Chain a ← b ← c and a standalone-blocked d ← a; only `a` is Ready
        // (b, c, d are Blocked behind it). Working `a` transitively unblocks
        // b, c, d = 3. The upstream `a` carries only an issue (no PR, no agent),
        // so it resolves as an edge target yet stays Ready.
        let mut a = ws("a");
        a.gh_issues = vec![task("github", "a#1")];
        let mut b = ws("b");
        let mut b_issue = task("github", "b#1");
        b_issue.blocked_by = vec![task("github", "a#1").id];
        b.gh_issues = vec![b_issue];
        let mut c = ws("c");
        let mut c_issue = task("github", "c#1");
        c_issue.blocked_by = vec![task("github", "b#1").id];
        c.gh_issues = vec![c_issue];
        let mut d = ws("d");
        let mut d_issue = task("github", "d#1");
        d_issue.blocked_by = vec![task("github", "a#1").id];
        d.gh_issues = vec![d_issue];

        let snap = resolve_fresh(&record_with(&["a", "b", "c", "d"]), &[a, b, c, d]);
        let queue = ready_queue(&snap);
        assert_eq!(queue.len(), 1, "only a is ready: {queue:?}");
        assert_eq!(queue[0].0.as_str(), "a");
        assert_eq!(queue[0].1, 3, "a transitively unblocks b, c, d");
    }

    #[test]
    fn ready_queue_orders_higher_unblocks_first_then_key() {
        // Two independent ready roots: `a` unblocks one dependent, `m` unblocks
        // none. `a` (1) ranks before `m` (0); a tie would break on key.
        let mut a = ws("a");
        a.gh_issues = vec![task("github", "a#1")];
        let mut b = ws("b");
        let mut b_issue = task("github", "b#1");
        b_issue.blocked_by = vec![task("github", "a#1").id];
        b.gh_issues = vec![b_issue];
        let m = ws("m"); // ready, nothing depends on it.

        let snap = resolve_fresh(&record_with(&["a", "b", "m"]), &[a, b, m]);
        let queue = ready_queue(&snap);
        let keys: Vec<&str> = queue.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["a", "m"], "higher unblocks first: {queue:?}");
    }

    #[tokio::test]
    async fn declared_persist_load_list_clear_round_trip() {
        let config = ServerConfig::in_memory();
        let blocker = DeclaredBlocker {
            workspace: WorkspaceKey::new("w"),
            reason: "waiting on decision".into(),
            kind: BlockerKind::Decision,
            owner: BlockerOwner::Operator,
            since: 7,
        };
        persist_declared(&config, &blocker).expect("persist");

        let loaded = load_declared(&config, "w").expect("load").expect("present");
        assert_eq!(loaded.reason, "waiting on decision");
        assert_eq!(loaded.kind, BlockerKind::Decision);
        assert_eq!(loaded.since, 7);

        let all = list_declared(&config).expect("list");
        assert_eq!(all.len(), 1);
        assert!(all.contains_key(&WorkspaceKey::new("w")));

        clear_declared(&config, "w").expect("clear");
        assert!(load_declared(&config, "w").expect("load").is_none());
        assert!(list_declared(&config).expect("list").is_empty());
    }

    #[tokio::test]
    async fn report_then_clear_blocker_moves_status() {
        // End-to-end through the command handlers: an epic member with nothing
        // else going on is Ready; report_blocker makes it Blocked; clear_blocker
        // returns it to Ready.
        let config = ServerConfig::in_memory();
        // The member must be a LOADED workspace for the resolver to include it.
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: "w".to_string(),
                created_at: Utc::now(),
                workspace_json: Some(serde_json::to_string(&ws("w")).unwrap()),
            })
            .unwrap();
        let record = record_with(&["w"]);
        upsert(&config, record).await;

        report_blocker(
            &config,
            WorkspaceKey::new("w"),
            "need sign-off".into(),
            BlockerKind::Review,
            BlockerOwner::Agent(WorkspaceKey::new("w")),
        )
        .await;
        let snaps = all_snapshots(&config).await;
        let m = &snaps.iter().find(|s| s.key == "e").expect("epic e").members[0];
        assert_eq!(m.status, EpicMemberStatus::Blocked);
        assert!(m.blockers.iter().any(|b| b.kind == BlockerKind::Review));

        clear_blocker(&config, "w").await;
        let snaps = all_snapshots(&config).await;
        let m = &snaps.iter().find(|s| s.key == "e").expect("epic e").members[0];
        assert_eq!(m.status, EpicMemberStatus::Ready);
        assert!(m.blockers.is_empty());
    }

    #[test]
    fn delta_activity_maps_keyed_deltas_and_skips_epic_wide() {
        // Workspace-keyed deltas each yield a body naming the epic; the
        // epic-wide ones name no member, so they produce no activity row.
        let unblocked = EpicDelta::Unblocked {
            key: WorkspaceKey::new("b"),
            because: vec![WorkspaceKey::new("a")],
        };
        let (k, body) = delta_activity(&unblocked, "auth").expect("keyed");
        assert_eq!(k.as_str(), "b");
        assert!(body.contains("auth") && body.contains("a"), "{body}");

        let changed = EpicDelta::StatusChanged {
            key: WorkspaceKey::new("b"),
            from: EpicMemberStatus::Ready,
            to: EpicMemberStatus::InProgress,
        };
        let (_, body) = delta_activity(&changed, "auth").expect("keyed");
        assert!(
            body.contains("ready") && body.contains("in progress"),
            "{body}"
        );

        let added = EpicDelta::BlockerAdded {
            key: WorkspaceKey::new("b"),
            blocker: Blocker {
                kind: BlockerKind::Review,
                reason: "needs sign-off".into(),
                owner: BlockerOwner::Operator,
                since: 0,
                holds: 0,
            },
        };
        let (_, body) = delta_activity(&added, "auth").expect("keyed");
        assert!(body.contains("needs sign-off"), "{body}");

        assert!(delta_activity(&EpicDelta::Completed, "auth").is_none());
        assert!(
            delta_activity(
                &EpicDelta::Stalled {
                    reason: "cycle".into()
                },
                "auth"
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn epic_delta_lands_as_unread_status_change_activity() {
        // A derived transition must show up on the affected workspace's activity
        // feed as an unread `StatusChange` row (#1517 step 8). The first
        // recompute (at upsert) seeds the latch with no delta; report_blocker
        // drives the second recompute, whose Ready→Blocked deltas land as
        // activity that survives a round trip through the store.
        let config = ServerConfig::in_memory();
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: "w".to_string(),
                created_at: Utc::now(),
                workspace_json: Some(serde_json::to_string(&ws("w")).unwrap()),
            })
            .unwrap();
        upsert(&config, record_with(&["w"])).await;

        // No activity yet: the first emission carries the snapshot, not a delta.
        let before: Workspace = serde_json::from_str(
            config
                .store
                .get_workspace(&WorkspaceKey::new("w"))
                .unwrap()
                .unwrap()
                .workspace_json
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert!(before.activity.is_empty(), "no delta on first sight");

        report_blocker(
            &config,
            WorkspaceKey::new("w"),
            "need sign-off".into(),
            BlockerKind::Review,
            BlockerOwner::Operator,
        )
        .await;

        let after: Workspace = serde_json::from_str(
            config
                .store
                .get_workspace(&WorkspaceKey::new("w"))
                .unwrap()
                .unwrap()
                .workspace_json
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert!(
            after
                .activity
                .iter()
                .any(|a| a.kind == ActivityKind::StatusChange
                    && a.author == "lazybox"
                    && a.body.contains("need sign-off")),
            "expected a StatusChange blocker row: {:?}",
            after.activity
        );
        assert!(
            after.unread_count() > 0,
            "the epic-event row must be unread: {:?}",
            after.activity
        );
    }

    #[tokio::test]
    async fn concurrent_store_write_survives_epic_activity_persist() {
        // `recompute_all` snapshots every workspace once at the top, then writes
        // each epic-status activity row back. The original path cloned that stale
        // top-of-function snapshot and raw-`save_workspace`d it, so a concurrent
        // poll that wrote fresher activity into the row *after* the snapshot was
        // clobbered — a lost-update race (the same one `polling::mutate` was
        // built to close). Routing the write through `apply_and_commit` re-loads
        // the fresh row under the workspace lock and merges the status row on
        // top, so both the concurrent write and the epic row survive.
        let config = ServerConfig::in_memory();
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: "w".to_string(),
                created_at: Utc::now(),
                workspace_json: Some(serde_json::to_string(&ws("w")).unwrap()),
            })
            .unwrap();
        upsert(&config, record_with(&["w"])).await; // seeds latch, no delta yet.

        // Hold "w"'s workspace lock so the activity persist inside the pending
        // recompute parks on `apply_and_commit`'s lock acquisition — *after* the
        // recompute has already taken its (now-about-to-go-stale) snapshot.
        let guard = config.lock_workspace("w").await;

        let task_config = config.clone();
        let task = tokio::spawn(async move {
            // Declaring a blocker drives a Ready→Blocked delta whose StatusChange
            // row is written through `apply_and_commit` for "w".
            report_blocker(
                &task_config,
                WorkspaceKey::new("w"),
                "need sign-off".into(),
                BlockerKind::Review,
                BlockerOwner::Operator,
            )
            .await;
        });

        // Give the recompute time to snapshot "w" and park on the held lock. Its
        // snapshot load + resolve are synchronous, so once polled it reaches the
        // lock and blocks; the fix is timing-independent regardless (a snapshot
        // taken after our write already contains it), but this pins the race.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // A concurrent poll commits a fresh comment into "w" *after* the
        // recompute's snapshot was taken.
        let mut fresh = ws("w");
        fresh.merge_activity(&[Activity {
            author: "octocat".to_string(),
            body: "CONCURRENT poll comment".to_string(),
            created_at: Utc::now(),
            kind: ActivityKind::Comment,
            node_id: Some("concurrent-node".to_string()),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        }]);
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: "w".to_string(),
                created_at: fresh.created_at,
                workspace_json: Some(serde_json::to_string(&fresh).unwrap()),
            })
            .unwrap();

        // Release the lock; the parked activity persist now re-loads fresh.
        drop(guard);
        task.await.unwrap();

        let after: Workspace = serde_json::from_str(
            config
                .store
                .get_workspace(&WorkspaceKey::new("w"))
                .unwrap()
                .unwrap()
                .workspace_json
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert!(
            after
                .activity
                .iter()
                .any(|a| a.body.contains("CONCURRENT poll comment")),
            "the concurrent poll's comment must not be clobbered: {:?}",
            after.activity
        );
        assert!(
            after
                .activity
                .iter()
                .any(|a| a.kind == ActivityKind::StatusChange && a.body.contains("need sign-off")),
            "the epic StatusChange row must be merged on top: {:?}",
            after.activity
        );
    }

    /// Persist a workspace so `all_snapshots`/`load_workspaces` can read it.
    fn save_ws(config: &ServerConfig, w: &Workspace) {
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: w.key.as_str().to_string(),
                created_at: w.created_at,
                workspace_json: Some(serde_json::to_string(w).unwrap()),
            })
            .unwrap();
    }

    /// An unroled workspace short-circuits before the expensive snapshot pass:
    /// no role, no ctx (#1523).
    #[tokio::test]
    async fn role_prompt_ctx_none_without_a_role() {
        let config = ServerConfig::in_memory();
        assert!(role_prompt_ctx(&config, &ws("w")).await.is_none());
    }

    /// A Worker's ctx lists the blockers already satisfied — its direct deps
    /// whose member is Done — and the rendered preamble names them (#1523).
    #[tokio::test]
    async fn worker_ctx_lists_resolved_blockers() {
        let config = ServerConfig::in_memory();
        // Dependency `a` is a merged PR → Done.
        let mut a = ws("a");
        pr(&mut a, TaskState::Merged, CiStatus::Success);
        // Worker `w` is blocked by a#pr and carries the Worker role.
        let mut w = ws("w");
        w.role = Some(Role::Worker);
        let mut w_issue = task("github", "w#1");
        w_issue.blocked_by = vec![task("github", "a#pr").id];
        w.gh_issues = vec![w_issue];
        save_ws(&config, &a);
        save_ws(&config, &w);
        let mut record = record_with(&["a", "w"]);
        record.anchor = Some(task("github", "o/r#100").id);
        persist(&config, &record).unwrap();

        let (role, ctx) = role_prompt_ctx(&config, &w).await.unwrap();
        assert_eq!(role, Role::Worker);
        assert_eq!(ctx.epic_key, "e");
        assert_eq!(ctx.epic_name, "Epic");
        assert_eq!(ctx.anchor_ref.as_deref(), Some("o/r#100"));
        assert_eq!(ctx.resolved_blockers, vec!["a".to_string()]);
        // The satisfied blocker surfaces in the rendered preamble, tagged for
        // the blackboard read.
        let preamble = lazybox_core::prompts::role_preamble(role, &ctx);
        assert!(preamble.contains('a'));
        assert!(preamble.contains("epic:e"));
    }

    /// An Integrator's ctx is the wave-ordered merge plan of members not yet
    /// landed — Done members drop out (#1523).
    #[tokio::test]
    async fn integrator_ctx_lists_unlanded_members_in_wave_order() {
        let config = ServerConfig::in_memory();
        // `a` merged (Done → excluded); `b` open, blocked by a (wave 1).
        let mut a = ws("a");
        pr(&mut a, TaskState::Merged, CiStatus::Success);
        let mut b = ws("b");
        let mut b_issue = task("github", "b#1");
        b_issue.blocked_by = vec![task("github", "a#pr").id];
        b.gh_issues = vec![b_issue];
        pr(&mut b, TaskState::Open, CiStatus::Pending);
        // Integrator `i` — task-less explicit member, so it resolves Ready.
        let mut integ = ws("i");
        integ.role = Some(Role::Integrator);
        save_ws(&config, &a);
        save_ws(&config, &b);
        save_ws(&config, &integ);
        persist(&config, &record_with(&["a", "b", "i"])).unwrap();

        let (role, ctx) = role_prompt_ctx(&config, &integ).await.unwrap();
        assert_eq!(role, Role::Integrator);
        assert!(
            ctx.merge_order.contains(&"b".to_string()),
            "an unlanded member is in the merge plan: {:?}",
            ctx.merge_order
        );
        assert!(
            !ctx.merge_order.contains(&"a".to_string()),
            "a landed (Done) member drops out: {:?}",
            ctx.merge_order
        );
        let preamble = lazybox_core::prompts::role_preamble(role, &ctx);
        assert!(preamble.contains('b'));
    }

    /// A Planner's ctx carries the built-in planning briefs regardless of epic
    /// membership, and the preamble teaches the machine-readable graph (#1523).
    #[tokio::test]
    async fn planner_ctx_carries_builtin_briefs() {
        let config = ServerConfig::in_memory();
        let mut p = ws("p");
        p.role = Some(Role::Planner);

        let (role, ctx) = role_prompt_ctx(&config, &p).await.unwrap();
        assert_eq!(role, Role::Planner);
        assert!(
            !ctx.planner_briefs.is_empty(),
            "carve/designissues bodies are injected server-side",
        );
        let preamble = lazybox_core::prompts::role_preamble(role, &ctx);
        assert!(preamble.contains("--parent"));
        assert!(preamble.contains("--blocked-by"));
        assert!(preamble.contains("Blocked by:"));
    }
}
