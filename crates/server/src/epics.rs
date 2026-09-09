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
    /// `contracts[M]` = the members that must publish an interface contract
    /// before M can build against it (`Contract:` markers, #1525). Like
    /// `merge_after`, only *member* producers are tracked — a contract on a
    /// task the epic does not contain is outside the epic's own graph.
    contracts: HashMap<WorkspaceKey, BTreeSet<WorkspaceKey>>,
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
    let mut contracts: HashMap<WorkspaceKey, BTreeSet<WorkspaceKey>> = HashMap::new();
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
        let mut ct: BTreeSet<WorkspaceKey> = BTreeSet::new();
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
            for producer in &task.contracts {
                match task_ws.get(producer) {
                    Some(other) if other == key => {}
                    Some(other) if member_set.contains(other) => {
                        ct.insert(other.clone());
                    }
                    _ => {
                        // A contract producer outside the epic cannot publish
                        // an `epic:<key>` note, so it is not a gate we can ever
                        // observe satisfied — out of scope, like merge-after.
                    }
                }
            }
        }
        merge_after.insert(key.clone(), ma);
        contracts.insert(key.clone(), ct);
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
        contracts,
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
    published_contracts: Option<&HashSet<WorkspaceKey>>,
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

    // A contract edge is satisfied by the producer *publishing the interface*
    // on the blackboard, not by its task closing — so a producer that is still
    // mid-flight but has posted its `contract` note no longer gates the
    // consumer, and one that merged without ever posting still does (#1525).
    for producer in resolved.contracts.get(key).into_iter().flatten() {
        if published_contracts.is_some_and(|published| published.contains(producer)) {
            continue;
        }
        let producer_name = resolved
            .by_key
            .get(producer)
            .map(|w| w.name.clone())
            .unwrap_or_else(|| producer.as_str().to_string());
        let reason = format!("waiting on contract from {producer_name}");
        let since = since_at(key, BlockerKind::Contract, &reason);
        out.push(Blocker {
            kind: BlockerKind::Contract,
            reason,
            owner: BlockerOwner::Agent(producer.clone()),
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
/// Asking → InProgress → ReviewBlocked → Mergeable → PrOpen → Claimed →
/// Blocked → Ready.
fn member_status(
    ws: &Workspace,
    agent: Option<AgentState>,
    blockers: &[Blocker],
    held_by: &[WorkspaceKey],
    review_blocking: bool,
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
    // A blocking review outranks every PR-shape status: the PR is live and may
    // even be green, but the Reviewer found something and the merge is held
    // until a clean review lands (#1525). Below the agent states, so a worker
    // actively fixing the findings still reads as InProgress.
    if review_blocking && pr.is_some() {
        return EpicMemberStatus::ReviewBlocked;
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
    latches: &LatchInputs,
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
                latches.contracts_for(record.key.as_str()),
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
        let status = member_status(
            ws,
            agent_states.get(key).copied(),
            &blockers,
            &held_by,
            latches.review_blocking.contains(key),
        );
        // The one non-graph block worth naming machine-readably: a consumer
        // waiting on an unpublished contract looks identical to an ordinary
        // dependency wait in `blocked_by` (it is not one) and carries no
        // `external_blockers` at all (#1525).
        let blocked_reason = blockers
            .iter()
            .any(|b| b.kind == BlockerKind::Contract)
            .then(|| BlockerKind::Contract.as_str().to_string());
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
            blocked_reason,
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
            EpicMemberStatus::ReviewBlocked => blocked += 1,
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
        policies: record.policies,
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
    for (from, tos) in &resolved.contracts {
        for to in tos {
            edges.push(EpicEdge {
                from: from.clone(),
                to: to.clone(),
                kind: EdgeKind::Contract,
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
            // A review verdict reads as its own delta rather than a bare
            // status transition — it is the one status change the *user* has
            // to act on, and it names the direction (#1525).
            if prev.status == EpicMemberStatus::ReviewBlocked
                || m.status == EpicMemberStatus::ReviewBlocked
            {
                deltas.push(EpicDelta::Reviewed {
                    key: m.key.clone(),
                    blocking: m.status == EpicMemberStatus::ReviewBlocked,
                });
            // A member leaving Blocked is reported as Unblocked (with the
            // dependencies that cleared it), which is more useful in the feed
            // than a bare status transition.
            } else if prev.status == EpicMemberStatus::Blocked
                && m.status != EpicMemberStatus::Blocked
            {
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
                // A review-blocked member is waiting on findings someone can
                // act on right now, not on an external task or a cycle.
                // Counting it as stalled would report "every remaining member
                // is blocked" for an epic whose only outstanding item is a
                // code review (#1525).
                | EpicMemberStatus::ReviewBlocked
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
        && a.policies == b.policies
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
        EpicMemberStatus::ReviewBlocked => "review blocked",
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
        EpicDelta::Reviewed { key, blocking } => Some((
            key.clone(),
            if *blocking {
                format!("Review found blocking findings in epic {epic_name} — merge held")
            } else {
                format!("Review clean in epic {epic_name} — merge released")
            },
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
        // No epic owns anything, so no review row can be legitimate. Falling
        // through to the prune (rather than returning) is what stops a hold
        // outliving the epic that created it when every record is deleted.
        prune_review_rows(config, &HashSet::new());
        return;
    }

    let agent_states = config.terminal.agent_states_by_workspace().await;
    let declared = list_declared(config).unwrap_or_else(|e| {
        tracing::warn!("epics: list declared blockers failed: {e}");
        HashMap::new()
    });
    let workspaces = crate::load_workspaces(&*config.store).values;
    let latches = LatchInputs::load(config, &records);
    let now = chrono::Utc::now().timestamp_millis();

    let mut to_emit: Vec<Event> = Vec::new();
    // Every member of every LIVE epic, accumulated regardless of whether that
    // epic's snapshot changed — the review-row prune below needs the full live
    // set, not just the epics that moved this pass.
    let mut live_members: HashSet<WorkspaceKey> = HashSet::new();
    {
        let mut memory = config.poll.epics.lock();
        let EpicMemory { since, last } = &mut *memory;
        let live: HashSet<String> = records.iter().map(|r| r.key.as_str().to_string()).collect();

        for record in &records {
            if record.archived {
                continue;
            }
            let latch = since.entry(record.key.as_str().to_string()).or_default();
            let snapshot = resolve(
                record,
                &workspaces,
                &agent_states,
                &declared,
                &latches,
                latch,
                now,
            );
            live_members.extend(snapshot.members.iter().map(|m| m.key.clone()));
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

    // A review row is only ever cleared through its epic's member list, so a
    // member that leaves — unassigned, workspace removed, epic archived or
    // deleted — would strand a `blocking: true` row. `review_blocks_merge` is
    // keyed on the workspace alone and consults no epic, so that stranded row
    // holds the PR's merge forever: auto-merge silently downgrades to Hold on
    // every tick and a manual `g m` keeps refusing, both citing an epic that no
    // longer exists. Prune to the live member set so a hold cannot outlive the
    // epic that raised it.
    prune_review_rows(config, &live_members);

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

    // Broadcast first, then act. A client sees the new status before an
    // autonomy latch starts anything on the back of it, so an `AUTO` spawn
    // never appears to precede the transition that caused it.
    let mut latched: Vec<EpicSnapshot> = Vec::new();
    for event in to_emit {
        if let Event::EpicStatus { snapshot, .. } = &event {
            latched.push(snapshot.clone());
        }
        let _ = config.bus.send(event);
    }
    for snapshot in &latched {
        run_latches(config, snapshot).await;
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
    let latches = LatchInputs::load(config, &records);
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
                &latches,
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
        // Quote the epic's published contracts, newest revision per producer,
        // so the Worker starts with the interface in hand rather than having to
        // go and read the blackboard for it (#1525). Read from the latch rows,
        // not the blackboard: the note that carried a contract is evicted once
        // its author has posted fifty more, and a Worker dispatched after that
        // would otherwise be briefed with no interface at all (#1577).
        let mut rows: Vec<PublishedContract> = list_published_contracts(config)
            .into_iter()
            .filter(|row| row.epic == snapshot.key)
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.published_at));
        ctx.contract_notes = rows.into_iter().map(|row| row.text).collect();
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

/// Set an epic's autonomy-dial latches, then recompute so an armed latch acts
/// on the current status immediately rather than waiting for the next
/// transition. Backs `Command::SetEpicPolicies` (#1525).
pub async fn set_policies(config: &ServerConfig, epic: &str, policies: lazybox_core::EpicPolicies) {
    let Some(mut record) = load(config, epic).unwrap_or_else(|e| {
        tracing::warn!("epics: load {epic} failed: {e}");
        None
    }) else {
        tracing::warn!("epics: set_policies on unknown epic {epic}");
        return;
    };
    if record.policies == policies {
        return;
    }
    tracing::info!(
        epic,
        auto_dispatch = policies.auto_dispatch.as_str(),
        auto_review = policies.auto_review.as_str(),
        merge_in_order = policies.merge_in_order.as_str(),
        "epics: autonomy latches set"
    );
    record.policies = policies;
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
        return;
    }
    // Archiving emits no bus event, so nothing would otherwise mark the
    // resolver dirty — recompute here so the review-row prune runs and any
    // merge this epic was holding is released now rather than whenever an
    // unrelated event happens to wake the debounced loop.
    recompute_all(config).await;
}

// ── autonomy dial (#1525) ───────────────────────────────────────────────
//
// Three latches on the epic record, each a `PolicyArm` in the shape of the
// existing `ARM` / `FIX` policies, and all off until armed:
//
//   * `AUTO`   — dispatch a Worker onto a member the moment it becomes Ready;
//   * `REVIEW` — dispatch a Reviewer onto a member whose PR turns green, and
//                hold its merge while the review reports `blocking` findings;
//   * `ORDER`  — arm merge-on-green on every member as its PR opens, so P3's
//                merge-after hold lands the whole epic in order.
//
// Every decision is a pure function over already-loaded data (`plan_dispatch`,
// `plan_reviews`, `plan_merge_arming`); `on_epic_status` is the thin async
// shell that gathers the inputs, calls them, and performs the effects.

/// kv key prefix for the reviewer stage's per-member memory.
const REVIEW_STATE_PREFIX: &str = "epic-review:";

/// What the automatic Reviewer stage knows about one member's current *green
/// run*. Persisted (so a restart neither re-reviews a PR nor forgets a hold)
/// and deliberately short-lived: the row is dropped the moment the member
/// stops being green, which is exactly what makes a re-green after fixes
/// re-run the review once.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReviewState {
    pub workspace: WorkspaceKey,
    /// The epic whose `REVIEW` latch opened this run. A verdict note must
    /// carry this epic's tag to be accepted, which is what makes the
    /// `epic:<key>` tag load-bearing rather than decorative.
    #[serde(default)]
    pub epic: String,
    /// A Reviewer has been dispatched for this green run. Gates the "fires
    /// once" property — the row exists from dispatch until the PR leaves
    /// green, so a poll storm cannot fan out reviewers.
    pub dispatched: bool,
    /// The Reviewer posted findings tagged `blocking`. Holds the merge and
    /// shows the member as `ReviewBlocked` until a `clean` note lands.
    pub blocking: bool,
    /// Unix ms the row was created.
    pub since: i64,
}

fn review_storage_key(workspace: &str) -> String {
    format!("{REVIEW_STATE_PREFIX}{workspace}")
}

fn persist_review(config: &ServerConfig, state: &ReviewState) -> Result<(), String> {
    let json = serde_json::to_string(state).map_err(|e| e.to_string())?;
    config
        .store
        .set_kv(&review_storage_key(state.workspace.as_str()), &json)
        .map_err(|e| e.to_string())
}

fn clear_review(config: &ServerConfig, workspace: &str) {
    if let Err(error) = config.store.delete_kv(&review_storage_key(workspace)) {
        tracing::warn!(%error, workspace, "epics: clearing review state failed");
    }
}

/// Drop every review row whose workspace is no longer a member of any live
/// epic. Called once per [`recompute_all`] with the full live member set.
fn prune_review_rows(config: &ServerConfig, live_members: &HashSet<WorkspaceKey>) {
    for (workspace, _) in list_reviews(config) {
        if live_members.contains(&workspace) {
            continue;
        }
        tracing::info!(
            %workspace,
            "epics: dropping a review row whose epic no longer owns the member"
        );
        clear_review(config, workspace.as_str());
    }
}

/// Every member's review state, keyed by workspace. A row that fails to decode
/// is skipped rather than sinking the whole read.
pub fn list_reviews(config: &ServerConfig) -> HashMap<WorkspaceKey, ReviewState> {
    let rows = match config.store.list_kv_prefix(REVIEW_STATE_PREFIX) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "epics: listing review state failed");
            return HashMap::new();
        }
    };
    rows.into_iter()
        .filter_map(|(_, json)| serde_json::from_str::<ReviewState>(&json).ok())
        .map(|state| (state.workspace.clone(), state))
        .collect()
}

// ── published contracts (#1577) ──────────────────────────────────────────

/// kv key prefix for a latched contract, one row per (epic, producer).
const CONTRACT_LATCH_PREFIX: &str = "epic-contract:";

/// A contract a producer has published for one epic, recorded the moment it
/// was first seen on the blackboard.
///
/// The blackboard is a rolling buffer — `post_note` prunes each scope to its
/// newest `NOTES_PER_SCOPE` entries — so the note that satisfied a `Contract`
/// edge is evicted once its author has posted fifty more, and `global` fills
/// faster still. Re-deriving satisfaction from the notes alone therefore
/// un-satisfies an interface that was genuinely agreed: the consumer flips
/// back to `Blocked` with reason `contract` long after the fact, and with
/// `AUTO` armed a Worker already dispatched on it now sits behind a blocker
/// nobody raised. So the row, not the note, is the record of the publication;
/// the note is only how it arrives.
///
/// **Rows are never deleted**, which is what separates them from the review
/// rows next door. A review row is a cache — drop it and the next green run
/// rebuilds it. Once the note is evicted this row holds the *only* copy of an
/// interface another agent wrote, so an epic-scoped prune would destroy it
/// irrecoverably: delete an epic and recreate it from the same name (the key
/// is `slugify(name)`, so it comes back identical) and every consumer
/// re-blocks on a contract nobody can republish. The bound is the real one —
/// one row per (epic, producer) that ever published, each capped by
/// `MAX_NOTE_BYTES`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublishedContract {
    pub epic: String,
    pub producer: WorkspaceKey,
    /// Bumped each time the producer publishes a *different* interface.
    /// Satisfaction stays keyed on the row existing rather than on the
    /// revision — §7.6 keeps the loose form until it has been dogfooded — so
    /// this is the version a later content-aware gate reads, and today it is
    /// what makes a changed interface visible in the log instead of moving
    /// under its consumers in silence.
    pub revision: u32,
    /// `ts` of the note this revision reflects. Orders the contracts quoted
    /// into a Worker preamble; never used to decide whether a note is newer
    /// (see [`latch_published_contracts`]).
    pub published_at: i64,
    /// The published interface itself, so a consumer's Worker preamble can
    /// still quote it once the note is gone.
    pub text: String,
}

fn contract_storage_key(epic: &str, producer: &str) -> String {
    format!("{CONTRACT_LATCH_PREFIX}{epic}:{producer}")
}

/// Every latched contract. A row that fails to decode is skipped rather than
/// sinking the whole read.
pub fn list_published_contracts(config: &ServerConfig) -> Vec<PublishedContract> {
    let rows = match config.store.list_kv_prefix(CONTRACT_LATCH_PREFIX) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "epics: listing published contracts failed");
            return Vec::new();
        }
    };
    rows.into_iter()
        .filter_map(|(_, json)| serde_json::from_str::<PublishedContract>(&json).ok())
        .collect()
}

/// Record every contract note currently on the blackboard, one row per (epic,
/// producer), taking the latch the first time and bumping the revision when a
/// producer publishes a different interface. `tags` maps each live epic's
/// `epic:<key>` tag to its key, so a note naming an epic that is archived or
/// gone records nothing.
///
/// Returns the whole latched set — including rows for epics that are not live
/// — so the caller filters what it already has in hand instead of re-reading
/// the rows this just wrote.
fn latch_published_contracts(
    config: &ServerConfig,
    tags: &HashMap<String, String>,
) -> HashMap<(String, WorkspaceKey), PublishedContract> {
    let mut latched: HashMap<(String, WorkspaceKey), PublishedContract> =
        list_published_contracts(config)
            .into_iter()
            .map(|row| ((row.epic.clone(), row.producer.clone()), row))
            .collect();
    // `notes_with_tags` orders by `(ts, seq)` descending, so the first note
    // seen for a pair is the current interface and the rest are its history.
    let mut seen: HashSet<(String, WorkspaceKey)> = HashSet::new();
    for note in crate::mcp::notes_with_tags(config, &[CONTRACT_TAG]) {
        let producer = WorkspaceKey::new(note.author.clone());
        for tag in &note.tags {
            let Some(epic) = tags.get(tag) else { continue };
            let id = (epic.clone(), producer.clone());
            if !seen.insert(id.clone()) {
                continue;
            }
            // Compare the interface, never the clock. `ts` is a millisecond
            // stamp and `seq` — which this does not have — is what orders two
            // notes inside one millisecond, so a `ts` test drops a correction
            // posted straight after its first draft and the row then quotes
            // the superseded text for the life of the epic.
            let row = match latched.get(&id) {
                Some(prev) if prev.text == note.text => continue,
                Some(prev) => {
                    tracing::info!(
                        epic = %epic,
                        producer = %producer,
                        revision = prev.revision + 1,
                        "epics: producer published a changed contract"
                    );
                    PublishedContract {
                        revision: prev.revision + 1,
                        published_at: note.ts,
                        text: note.text.clone(),
                        ..prev.clone()
                    }
                }
                None => {
                    tracing::info!(
                        epic = %epic,
                        producer = %producer,
                        "epics: latched a published contract"
                    );
                    PublishedContract {
                        epic: epic.clone(),
                        producer: producer.clone(),
                        revision: 1,
                        published_at: note.ts,
                        text: note.text.clone(),
                    }
                }
            };
            // Only a row that reached the store counts. Treating a failed
            // write as latched would satisfy the edge for this recompute and
            // un-satisfy it on the next — the flapping the latch exists to
            // stop.
            if persist_published_contract(config, &row) {
                latched.insert(id, row);
            }
        }
    }
    latched
}

fn persist_published_contract(config: &ServerConfig, row: &PublishedContract) -> bool {
    let key = contract_storage_key(&row.epic, row.producer.as_str());
    let write = serde_json::to_string(row)
        .map_err(|e| e.to_string())
        .and_then(|json| config.store.set_kv(&key, &json).map_err(|e| e.to_string()));
    match write {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                %error,
                epic = %row.epic,
                producer = %row.producer,
                "epics: persisting a published contract failed"
            );
            false
        }
    }
}

/// Cross-cutting latch state the resolver *reads* but does not derive: which
/// producers have published their interface contract, and which members a
/// blocking review is holding. Both live in the kv (the contract rows and the
/// review rows), so they are loaded once per recompute and handed to
/// [`resolve`] rather than being read from inside it — [`resolve`] stays a pure
/// function of plain data.
///
/// [`LatchInputs::load`] itself is *not* pure: taking a contract latch is a
/// write, and it happens wherever a contract is first observed — including
/// `all_snapshots`, which a client subscribe runs. That is deliberate.
/// Latching only where a recompute happens to run would leave the window
/// between a note arriving and the next recompute unlatched, which is the
/// window retention can close.
#[derive(Debug, Default)]
pub struct LatchInputs {
    /// Per epic key, the members that have published an interface contract, so
    /// their `Contract` edges in *that* epic are satisfied. Keyed by epic
    /// rather than flattened: a contract published for one epic says nothing
    /// about a different epic's interface, so a flat set would let a producer
    /// satisfy an edge it never published for.
    pub published_contracts: HashMap<String, HashSet<WorkspaceKey>>,
    /// Members whose latest review reported `blocking` findings.
    pub review_blocking: HashSet<WorkspaceKey>,
}

impl LatchInputs {
    /// Gather the latch state for `records` from the store.
    ///
    /// Reads the blackboard **once** and buckets by epic tag. Scanning per
    /// epic instead would re-read and re-parse every note in the store for
    /// each record — notes are capped per scope but scopes are not (one per
    /// session), so a fleet-sized blackboard turns an N-epic recompute into N
    /// full table scans on the 300 ms debounce path.
    ///
    /// Satisfaction itself comes from the latch rows, not that scan: the scan
    /// only *takes* the latch (#1577). See [`PublishedContract`].
    pub fn load(config: &ServerConfig, records: &[EpicRecord]) -> Self {
        let tags: HashMap<String, String> = records
            .iter()
            .filter(|r| !r.archived)
            .map(|r| {
                (
                    crate::mcp::epic_tag(r.key.as_str()),
                    r.key.as_str().to_string(),
                )
            })
            .collect();
        let live: HashSet<&String> = tags.values().collect();
        let mut published_contracts: HashMap<String, HashSet<WorkspaceKey>> = HashMap::new();
        for (epic, producer) in latch_published_contracts(config, &tags).into_keys() {
            if live.contains(&epic) {
                published_contracts
                    .entry(epic)
                    .or_default()
                    .insert(producer);
            }
        }
        let review_blocking = list_reviews(config)
            .into_iter()
            .filter(|(_, state)| state.blocking)
            .map(|(key, _)| key)
            .collect();
        Self {
            published_contracts,
            review_blocking,
        }
    }

    /// The producers that have published a contract for `epic`.
    fn contracts_for(&self, epic: &str) -> Option<&HashSet<WorkspaceKey>> {
        self.published_contracts.get(epic)
    }
}

/// Note tag marking a published interface contract.
const CONTRACT_TAG: &str = "contract";
/// Note tag marking a Reviewer's findings.
const REVIEW_TAG: &str = "review";
/// Verdict tags a Reviewer's note carries alongside [`REVIEW_TAG`].
const BLOCKING_TAG: &str = "blocking";
const CLEAN_TAG: &str = "clean";

/// One member the `AUTO` latch wants a Worker started on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchTicket {
    pub member: WorkspaceKey,
    /// Store-backed idempotency key, so a double-fire (two recomputes racing,
    /// or a daemon restart mid-dispatch) collapses to one spawn.
    pub dedup_key: String,
}

/// Everything `plan_dispatch` needs beyond the snapshot, gathered by the
/// caller so the planner itself touches no store and no clock.
#[derive(Debug, Default)]
pub struct DispatchContext {
    /// Members of this epic already carrying the Worker role.
    pub live_workers: usize,
    /// `agent.max_epic_workers`. Zero disables dispatch entirely, exactly as
    /// it disables the `spawn_worker` MCP tool.
    pub max_workers: usize,
    /// Members lazybox must not start: a `working` claim held on another box,
    /// or an agent already running locally. Never start two agents on one
    /// member.
    pub excluded: HashSet<WorkspaceKey>,
    /// Members carrying a `no-auto-fix` / `do-not-lazybox` label. Those
    /// labels mean "do not act on this row unattended", which covers
    /// auto-dispatch as much as auto-fix.
    pub opted_out: HashSet<WorkspaceKey>,
}

/// The Workers the `AUTO` latch should start right now, ranked so the member
/// that unblocks the most others goes first, and capped at the epic's
/// remaining worker headroom. Pure: every gate is decided from the arguments.
///
/// Returns empty — the "stands down" cases — when the latch is not armed, the
/// cap is reached or disabled, or every ready member is claimed / opted out.
pub fn plan_dispatch(
    snapshot: &EpicSnapshot,
    policies: &lazybox_core::EpicPolicies,
    ctx: &DispatchContext,
) -> Vec<DispatchTicket> {
    if !policies.armed(lazybox_core::EpicLatch::AutoDispatch) {
        return Vec::new();
    }
    let headroom = ctx.max_workers.saturating_sub(ctx.live_workers);
    if headroom == 0 {
        return Vec::new();
    }
    ready_queue(snapshot)
        .into_iter()
        .map(|(key, _)| key)
        .filter(|key| !ctx.excluded.contains(key) && !ctx.opted_out.contains(key))
        .take(headroom)
        .map(|member| DispatchTicket {
            dedup_key: dispatch_dedup_key(&snapshot.key, &member),
            member,
        })
        .collect()
}

/// The store marker that makes one epic→member dispatch fire at most once.
fn dispatch_dedup_key(epic: &str, member: &WorkspaceKey) -> String {
    format!("autospawn-epic:{epic}:{member}")
}

/// Whether a member's PR is currently *green* — the state the Reviewer stage
/// keys off. A `Mergeable` PR is green by construction; a `PrOpen` one is green
/// only with CI passing and no requested changes. Every other status (an agent
/// working, a red PR, a held review) is not.
fn member_is_green(status: &EpicMemberStatus) -> bool {
    match status {
        EpicMemberStatus::Mergeable { .. } => true,
        EpicMemberStatus::PrOpen {
            ci_failing,
            changes_requested,
        } => !ci_failing && !changes_requested,
        _ => false,
    }
}

/// What the `REVIEW` latch should do this tick: which members to dispatch a
/// Reviewer onto, and which review rows to drop.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReviewPlan {
    /// Members whose PR just turned green with no reviewer yet.
    pub dispatch: Vec<WorkspaceKey>,
    /// Members that are no longer green — their row is dropped so the next
    /// green run reviews once more (and a stale hold cannot outlive the PR).
    pub clear: Vec<WorkspaceKey>,
}

/// Plan the automatic Reviewer stage. Pure over the snapshot, the latch, the
/// persisted review rows, and the label opt-out set.
///
/// The `clear` half runs regardless of the latch: a row left behind by a
/// disarmed latch must still be dropped when its PR goes red, or a stale
/// `blocking` hold would outlive the review that produced it.
pub fn plan_reviews(
    snapshot: &EpicSnapshot,
    policies: &lazybox_core::EpicPolicies,
    reviews: &HashMap<WorkspaceKey, ReviewState>,
    opted_out: &HashSet<WorkspaceKey>,
) -> ReviewPlan {
    let armed = policies.armed(lazybox_core::EpicLatch::AutoReview);
    let mut plan = ReviewPlan::default();
    for member in &snapshot.members {
        let green = member_is_green(&member.status);
        // `ReviewBlocked` is the status a held member *has*; it is not a
        // reason to drop the hold that produced it.
        let held = member.status == EpicMemberStatus::ReviewBlocked;
        match reviews.get(&member.key) {
            Some(_) if !green && !held => plan.clear.push(member.key.clone()),
            Some(_) => {}
            None if armed && green && !opted_out.contains(&member.key) => {
                plan.dispatch.push(member.key.clone())
            }
            None => {}
        }
    }
    plan
}

/// The members the `ORDER` latch should arm merge-on-green on: every member
/// with a live PR that is not armed yet. P3's merge-after hold supplies the
/// ordering, so arming everything is safe — a PR behind an unlanded
/// predecessor is held, not merged early.
///
/// A `ReviewBlocked` member is skipped, not held: the review hold would stop
/// the merge anyway, so arming it only paints an `ARM` pill promising a merge
/// that a second policy is silently refusing. It arms on the next pass once
/// the review clears and the member reads `PrOpen` / `Mergeable` again.
pub fn plan_merge_arming(
    snapshot: &EpicSnapshot,
    policies: &lazybox_core::EpicPolicies,
    already_armed: &HashSet<WorkspaceKey>,
    opted_out: &HashSet<WorkspaceKey>,
) -> Vec<WorkspaceKey> {
    if !policies.armed(lazybox_core::EpicLatch::MergeInOrder) {
        return Vec::new();
    }
    snapshot
        .members
        .iter()
        .filter(|m| {
            matches!(
                m.status,
                EpicMemberStatus::PrOpen { .. } | EpicMemberStatus::Mergeable { .. }
            )
        })
        .map(|m| m.key.clone())
        .filter(|key| !already_armed.contains(key) && !opted_out.contains(key))
        .collect()
}

/// Whether a member is opted out of unattended action by label. The
/// `no-auto-fix` / `do-not-lazybox` labels have always meant "lazybox, keep
/// your hands off this row"; the autonomy dial honors them as
/// "do not auto-dispatch / auto-review" too.
fn labels_opt_out(ws: &Workspace, opt_out_labels: &[String]) -> bool {
    tasks_of(ws).any(|task| {
        task.labels.iter().any(|label| {
            opt_out_labels
                .iter()
                .any(|opt| opt.eq_ignore_ascii_case(&label.name))
        })
    })
}

/// Whether the merge of `key` is held by a blocking review (#1525). Checked
/// beside [`held_by`] on the auto-merge and manual-merge paths, so a PR the
/// Reviewer flagged does not land while the findings stand.
pub fn review_blocks_merge(config: &ServerConfig, key: &WorkspaceKey) -> bool {
    config
        .store
        .get_kv(&review_storage_key(key.as_str()))
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str::<ReviewState>(&json).ok())
        .is_some_and(|state| state.blocking)
}

/// A blackboard note just landed. When it is a Reviewer's verdict for a member
/// of a live epic, record it: `blocking` holds the member's merge and flips it
/// to `ReviewBlocked`; `clean` releases. When it is a contract published to a
/// live epic, the recompute both latches it ([`latch_published_contracts`]) and
/// unblocks the consumers waiting on it. Hooked into `post_note` so the latches
/// react at the write rather than polling the blackboard.
///
/// Note text is agent-authored and never parsed — only the tag vocabulary is
/// read, and **every** path to the recompute is gated, because `post_note`
/// holds the blackboard's process-wide write lock across this call and
/// `recompute_all` spawns agents and talks to GitHub. An ungated tag would let
/// one agent stall every other agent's `post_note` behind a full recompute
/// just by posting.
pub(crate) async fn on_note_posted(config: &ServerConfig, note: &crate::mcp::Note) {
    let contract = note.tags.iter().any(|t| t == CONTRACT_TAG) && names_a_live_epic(config, note);
    // Recorded before the branch, not inside it: a note carrying both a
    // contract and a verdict must land the verdict and then recompute once,
    // rather than recomputing on the contract with the verdict still unwritten
    // and again immediately after.
    let verdict = record_review_verdict(config, note);
    if contract || verdict {
        recompute_all(config).await;
    }
}

/// Whether `note` names an epic the daemon actually has. The `epic:<key>` tag
/// is the gate, so a bare `contract` tag — or one naming an epic that is
/// archived, deleted, or simply mistyped — drives nothing.
fn names_a_live_epic(config: &ServerConfig, note: &crate::mcp::Note) -> bool {
    list_all(config).unwrap_or_default().iter().any(|record| {
        !record.archived
            && note
                .tags
                .iter()
                .any(|t| *t == crate::mcp::epic_tag(record.key.as_str()))
    })
}

/// Record a Reviewer's verdict, returning whether it actually moved the hold.
///
/// Two gates, both derived from state lazybox itself wrote: the author must
/// have an open review row (so only a member the `REVIEW` latch actually
/// dispatched on can report), and the note must carry that row's own
/// `epic:<key>` tag (so a verdict cannot flip a hold a different epic raised).
fn record_review_verdict(config: &ServerConfig, note: &crate::mcp::Note) -> bool {
    if !note.tags.iter().any(|t| t == REVIEW_TAG) {
        return false;
    }
    let blocking = note.tags.iter().any(|t| t == BLOCKING_TAG);
    if !blocking && !note.tags.iter().any(|t| t == CLEAN_TAG) {
        return false; // a review note with no verdict says nothing about the merge.
    }
    let author = WorkspaceKey::new(note.author.clone());
    // The reviewer runs *in the member's own workspace* (a second agent beside
    // the worker), so the note's author IS the member under review — and the
    // open review row is what proves the stage actually started this run.
    let Some(mut state) = list_reviews(config).remove(&author) else {
        tracing::debug!(
            author = %author,
            "epics: review note for a member with no open review run — ignoring"
        );
        return false;
    };
    if !note
        .tags
        .iter()
        .any(|t| *t == crate::mcp::epic_tag(&state.epic))
    {
        tracing::debug!(
            author = %author,
            epic = %state.epic,
            "epics: review verdict does not name the epic that opened the run — ignoring"
        );
        return false;
    }
    if state.blocking == blocking {
        return false;
    }
    state.blocking = blocking;
    if let Err(error) = persist_review(config, &state) {
        tracing::warn!(%error, member = %author, "epics: persisting review verdict failed");
        return false;
    }
    tracing::info!(
        member = %author,
        epic = %state.epic,
        blocking,
        "epics: reviewer verdict recorded"
    );
    true
}

/// React to a freshly-broadcast epic snapshot by running whatever its armed
/// latches ask for. Called from [`recompute_all`] after the snapshot is stored,
/// once per epic whose status actually changed.
///
/// Deliberately reads the snapshot's **standing state**, not the `EpicDelta`s
/// that accompany it. Gating on the delta looks like the safer "act only on a
/// transition" rule and is in fact a dead latch: `diff` yields no member
/// deltas for a first-sight snapshot (`diff(None, _)` is empty — every epic
/// after a daemon restart) or for a policy-only change (arming a latch moves
/// no member's status), so the two moments a latch most needs to act are
/// exactly the two that produce nothing to act on.
///
/// Re-entry is safe without that gate because each latch carries its own
/// idempotency: dispatch has the store-backed `autospawn-epic:<epic>:<member>`
/// marker, review has the persisted `epic-review:<workspace>` row, and
/// merge-arming skips workspaces already armed. `recompute_all` also updates
/// its `last` snapshot under the `EpicMemory` lock *before* calling here, so a
/// concurrent recompute short-circuits on `same_status` and one transition
/// reaches this function once.
async fn run_latches(config: &ServerConfig, snapshot: &EpicSnapshot) {
    let Some(record) = load(config, &snapshot.key).unwrap_or_default() else {
        return;
    };
    if record.archived {
        return;
    }
    let policies = record.policies;
    let user_config = lazybox_config::Config::load().unwrap_or_default();
    let opt_out_labels = user_config.auto_fix.opt_out_labels.clone();
    let workspaces = crate::load_workspaces(&*config.store).values;
    let by_key: HashMap<&WorkspaceKey, &Workspace> =
        workspaces.iter().map(|w| (&w.key, w)).collect();

    let opted_out: HashSet<WorkspaceKey> = snapshot
        .members
        .iter()
        .filter(|m| {
            by_key
                .get(&m.key)
                .is_some_and(|ws| labels_opt_out(ws, &opt_out_labels))
        })
        .map(|m| m.key.clone())
        .collect();

    // ── REVIEW ──────────────────────────────────────────────────────────
    let reviews = list_reviews(config);
    let review_plan = plan_reviews(snapshot, &policies, &reviews, &opted_out);
    for member in &review_plan.clear {
        clear_review(config, member.as_str());
    }
    for member in &review_plan.dispatch {
        dispatch_reviewer(config, &record, member, &user_config).await;
    }

    // ── ORDER ───────────────────────────────────────────────────────────
    let already_armed: HashSet<WorkspaceKey> = workspaces
        .iter()
        .filter(|ws| ws.auto_merge_on_green)
        .map(|ws| ws.key.clone())
        .collect();
    for member in plan_merge_arming(snapshot, &policies, &already_armed, &opted_out) {
        tracing::info!(epic = %snapshot.key, %member, "epics: ORDER arming merge-on-green");
        crate::workspace::set_auto_merge_on_green(config, &member, true).await;
    }

    // ── AUTO ────────────────────────────────────────────────────────────
    if !policies.armed(lazybox_core::EpicLatch::AutoDispatch) {
        return;
    }
    let agent_states = config.terminal.agent_states_by_workspace().await;
    let excluded: HashSet<WorkspaceKey> = snapshot
        .members
        .iter()
        .filter(|m| {
            agent_states.contains_key(&m.key)
                || matches!(m.status, EpicMemberStatus::Claimed)
                || by_key.get(&m.key).is_some_and(|ws| has_working_claim(ws))
        })
        .map(|m| m.key.clone())
        .collect();
    let live_workers = snapshot
        .members
        .iter()
        .filter(|m| {
            by_key
                .get(&m.key)
                .is_some_and(|ws| ws.effective_role() == Some(Role::Worker))
        })
        .count();
    let ctx = DispatchContext {
        live_workers,
        max_workers: user_config
            .agent
            .max_epic_workers
            .unwrap_or(lazybox_config::DEFAULT_MAX_EPIC_WORKERS),
        excluded,
        opted_out,
    };
    for ticket in plan_dispatch(snapshot, &policies, &ctx) {
        let Some(ws) = by_key.get(&ticket.member) else {
            continue;
        };
        dispatch_worker(config, &record, &ticket, ws, &user_config).await;
    }
}

/// Start a Worker on `ticket.member` through the same dispatcher the
/// `@lazybox` / label auto-spawns use, so the singleton collapse, the
/// unattended-permission handling, and the footer notice are identical. The
/// `Role::Worker` rides in-band so `handle_spawn` frames the brief with the
/// Worker preamble (which quotes the epic's contracts).
async fn dispatch_worker(
    config: &ServerConfig,
    record: &EpicRecord,
    ticket: &DispatchTicket,
    workspace: &Workspace,
    user_config: &lazybox_config::Config,
) {
    let session_key = lazybox_core::SessionKey::new(ticket.member.as_str());
    let Some(prompt) = member_work_prompt(workspace, user_config) else {
        tracing::warn!(
            member = %ticket.member,
            "epics: AUTO has nothing to brief a worker with — skipping"
        );
        return;
    };
    tracing::info!(
        epic = %record.key,
        member = %ticket.member,
        "epics: AUTO dispatching a worker onto a ready member"
    );
    crate::polling::dispatch_action(
        config,
        "epic-auto",
        None,
        crate::polling::ProviderAction::AutoSpawnAgent {
            session_key,
            agent_id: default_agent(user_config),
            model_alias: None,
            prompt: Some(prompt),
            reason: format!("AUTO dispatch on epic {}", record.key),
            dedup_key: Some(ticket.dedup_key.clone()),
            // The brief is built from the member's own issue, which is the
            // same text a `w w` press would use — not foreign input the
            // latch introduced.
            untrusted: false,
            epic_role: Some(Role::Worker),
        },
    )
    .await;
}

/// Start a Reviewer beside the worker on a member whose PR turned green, and
/// open its review row so the stage fires exactly once per green run. The row
/// is written *before* the spawn: a spawn that fails leaves a row that the
/// next non-green tick clears, whereas a row written after would let a slow
/// spawn double-fire.
async fn dispatch_reviewer(
    config: &ServerConfig,
    record: &EpicRecord,
    member: &WorkspaceKey,
    user_config: &lazybox_config::Config,
) {
    let state = ReviewState {
        workspace: member.clone(),
        epic: record.key.as_str().to_string(),
        dispatched: true,
        blocking: false,
        since: chrono::Utc::now().timestamp_millis(),
    };
    if let Err(error) = persist_review(config, &state) {
        tracing::warn!(%error, %member, "epics: opening a review run failed — not spawning");
        return;
    }
    tracing::info!(
        epic = %record.key,
        %member,
        "epics: REVIEW dispatching a reviewer onto a green PR"
    );
    crate::polling::dispatch_action(
        config,
        "epic-auto",
        None,
        crate::polling::ProviderAction::AutoSpawnAgent {
            session_key: lazybox_core::SessionKey::new(member.as_str()),
            agent_id: default_agent(user_config),
            model_alias: None,
            prompt: Some(reviewer_brief(record.key.as_str())),
            reason: format!("REVIEW stage on epic {}", record.key),
            dedup_key: None,
            untrusted: false,
            epic_role: Some(Role::Reviewer),
        },
    )
    .await;
}

/// The task half of a Reviewer's prompt. The Reviewer role preamble
/// (`prompts::role_preamble`) supplies the framing; this names the one
/// mechanical requirement the latch depends on — a verdict-tagged note.
fn reviewer_brief(epic_key: &str) -> String {
    format!(
        "Review this workspace's open PR against its issue's Definition of Done.\n\n\
         Read the diff with `gh pr diff` and the issue with `gh issue view`. Do not push \
         changes and do not merge.\n\n\
         **End with exactly one note**, which is how lazybox records your verdict:\n\
         - findings that must be fixed before merge:\n  \
           `post_note(text=\"<your findings>\", tags=[\"review\", \"epic:{epic_key}\", \"blocking\"])`\n\
         - nothing blocking:\n  \
           `post_note(text=\"<what you checked>\", tags=[\"review\", \"epic:{epic_key}\", \"clean\"])`\n\n\
         A `blocking` verdict holds the PR's merge until you post a `clean` one."
    )
}

/// The brief an auto-dispatched Worker starts from: the member's own issue,
/// rendered with the same prompt builder the `w w` press and the label-spawn
/// path use. `None` when the member has no issue to work from — a PR-only
/// member is already past the point a Worker would be dispatched.
fn member_work_prompt(ws: &Workspace, user_config: &lazybox_config::Config) -> Option<String> {
    let issue = ws
        .gh_issues
        .iter()
        .chain(ws.linear_issues.iter())
        .find(|t| !matches!(t.state, TaskState::Merged | TaskState::Closed))?;
    Some(lazybox_core::prompts::build_implement_issue_prompt_with(
        issue,
        &user_config.conventions,
    ))
}

/// The agent id an epic latch spawns, resolved per dispatch from live config
/// so an operator's edit takes effect without a daemon restart — same source
/// the `spawn_worker` MCP tool reads.
fn default_agent(user_config: &lazybox_config::Config) -> String {
    user_config
        .setup
        .default_agent
        .clone()
        .unwrap_or_else(|| "claude".to_string())
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
            contracts: vec![],
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
        resolve_with_latches(record, workspaces, &LatchInputs::default())
    }

    fn resolve_with_latches(
        record: &EpicRecord,
        workspaces: &[Workspace],
        latches: &LatchInputs,
    ) -> EpicSnapshot {
        let mut latch = HashMap::new();
        resolve(
            record,
            workspaces,
            &HashMap::new(),
            &HashMap::new(),
            latches,
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
            &mut latch,
            1,
        );
        // b green now → merge-ready but held by a's still-open PR.
        let after = resolve(
            &record,
            &[a, make_b(CiStatus::Success)],
            &HashMap::new(),
            &HashMap::new(),
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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
            &LatchInputs::default(),
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

    // ── autonomy dial (#1525) ───────────────────────────────────────────

    /// Members `a` (ready) and `b` (ready), no edges — the shape the
    /// dispatcher plans over.
    fn ready_epic() -> (EpicRecord, Vec<Workspace>) {
        let mut a = ws("a");
        a.gh_issues = vec![task("github", "o/r#1")];
        let mut b = ws("b");
        b.gh_issues = vec![task("github", "o/r#2")];
        (record_with(&["a", "b"]), vec![a, b])
    }

    fn armed(latch: lazybox_core::EpicLatch) -> lazybox_core::EpicPolicies {
        let mut p = lazybox_core::EpicPolicies::default();
        p.set(latch, lazybox_core::PolicyArm::Arm);
        p
    }

    fn dispatch_ctx() -> DispatchContext {
        DispatchContext {
            live_workers: 0,
            max_workers: 6,
            excluded: HashSet::new(),
            opted_out: HashSet::new(),
        }
    }

    #[test]
    fn dispatch_is_off_by_default() {
        let (record, workspaces) = ready_epic();
        let snap = resolve_fresh(&record, &workspaces);
        assert_eq!(snap.ready, 2, "both members should read Ready");
        assert!(
            plan_dispatch(
                &snap,
                &lazybox_core::EpicPolicies::default(),
                &dispatch_ctx()
            )
            .is_empty(),
            "an unarmed epic must dispatch nothing"
        );
    }

    #[test]
    fn dispatch_plans_every_ready_member_when_armed() {
        let (record, workspaces) = ready_epic();
        let snap = resolve_fresh(&record, &workspaces);
        let tickets = plan_dispatch(
            &snap,
            &armed(lazybox_core::EpicLatch::AutoDispatch),
            &dispatch_ctx(),
        );
        let members: Vec<&str> = tickets.iter().map(|t| t.member.as_str()).collect();
        assert_eq!(members, vec!["a", "b"]);
        // Each ticket carries a per-(epic, member) marker so a re-fire on the
        // next recompute collapses instead of spawning twice.
        assert_eq!(tickets[0].dedup_key, "autospawn-epic:e:a");
        assert_ne!(tickets[0].dedup_key, tickets[1].dedup_key);
    }

    /// Ranked by how much each unblocks: a member two others wait on is
    /// dispatched before a leaf, so limited headroom buys the most.
    #[test]
    fn dispatch_ranks_by_downstream_unblocking() {
        let mut a = ws("a");
        a.gh_issues = vec![task("github", "o/r#1")];
        let mut leaf = ws("leaf");
        leaf.gh_issues = vec![task("github", "o/r#9")];
        // b and c both wait on a; leaf waits on nobody.
        let mut b = ws("b");
        let mut b_issue = task("github", "o/r#2");
        b_issue.blocked_by = vec![task("github", "o/r#1").id];
        b.gh_issues = vec![b_issue];
        let mut c = ws("c");
        let mut c_issue = task("github", "o/r#3");
        c_issue.blocked_by = vec![task("github", "o/r#1").id];
        c.gh_issues = vec![c_issue];

        let record = record_with(&["a", "b", "c", "leaf"]);
        let snap = resolve_fresh(&record, &[a, b, c, leaf]);
        let mut ctx = dispatch_ctx();
        ctx.max_workers = 1;
        let tickets = plan_dispatch(&snap, &armed(lazybox_core::EpicLatch::AutoDispatch), &ctx);
        assert_eq!(
            tickets
                .iter()
                .map(|t| t.member.as_str())
                .collect::<Vec<_>>(),
            vec!["a"],
            "the member holding two others must go first"
        );
    }

    #[test]
    fn dispatch_stands_down_at_the_worker_cap() {
        let (record, workspaces) = ready_epic();
        let snap = resolve_fresh(&record, &workspaces);
        let policies = armed(lazybox_core::EpicLatch::AutoDispatch);

        let mut at_cap = dispatch_ctx();
        at_cap.max_workers = 2;
        at_cap.live_workers = 2;
        assert!(plan_dispatch(&snap, &policies, &at_cap).is_empty());

        // One slot left → exactly one ticket, not two.
        let mut one_slot = dispatch_ctx();
        one_slot.max_workers = 2;
        one_slot.live_workers = 1;
        assert_eq!(plan_dispatch(&snap, &policies, &one_slot).len(), 1);

        // `max_epic_workers: 0` disables dispatch outright, exactly as it
        // disables the `spawn_worker` MCP tool.
        let mut disabled = dispatch_ctx();
        disabled.max_workers = 0;
        assert!(plan_dispatch(&snap, &policies, &disabled).is_empty());
    }

    #[test]
    fn dispatch_stands_down_on_a_claim_or_an_opt_out_label() {
        let (record, workspaces) = ready_epic();
        let snap = resolve_fresh(&record, &workspaces);
        let policies = armed(lazybox_core::EpicLatch::AutoDispatch);

        let mut claimed = dispatch_ctx();
        claimed.excluded.insert(WorkspaceKey::new("a"));
        assert_eq!(
            plan_dispatch(&snap, &policies, &claimed)
                .iter()
                .map(|t| t.member.as_str())
                .collect::<Vec<_>>(),
            vec!["b"],
            "a member claimed elsewhere (or already running an agent) is skipped"
        );

        let mut opted = dispatch_ctx();
        opted.opted_out.insert(WorkspaceKey::new("b"));
        assert_eq!(
            plan_dispatch(&snap, &policies, &opted)
                .iter()
                .map(|t| t.member.as_str())
                .collect::<Vec<_>>(),
            vec!["a"],
            "`no-auto-fix` / `do-not-lazybox` means do not auto-dispatch either"
        );
    }

    /// A blocked member is never dispatched even when the latch is armed —
    /// `plan_dispatch` reads the ready queue, not the member list.
    #[test]
    fn dispatch_never_starts_a_blocked_member() {
        let mut a = ws("a");
        a.gh_issues = vec![task("github", "o/r#1")];
        let mut b = ws("b");
        let mut b_issue = task("github", "o/r#2");
        b_issue.blocked_by = vec![task("github", "o/r#1").id];
        b.gh_issues = vec![b_issue];
        let record = record_with(&["a", "b"]);
        let snap = resolve_fresh(&record, &[a, b]);
        assert_eq!(
            plan_dispatch(
                &snap,
                &armed(lazybox_core::EpicLatch::AutoDispatch),
                &dispatch_ctx()
            )
            .iter()
            .map(|t| t.member.as_str())
            .collect::<Vec<_>>(),
            vec!["a"]
        );
    }

    /// A green-PR epic: `a` has an open PR, CI passing.
    fn green_pr_epic() -> (EpicRecord, Vec<Workspace>) {
        let mut a = ws("a");
        pr(&mut a, TaskState::Open, CiStatus::Success);
        (record_with(&["a"]), vec![a])
    }

    fn review_row(key: &str, blocking: bool) -> HashMap<WorkspaceKey, ReviewState> {
        let mut map = HashMap::new();
        map.insert(
            WorkspaceKey::new(key),
            ReviewState {
                workspace: WorkspaceKey::new(key),
                epic: "e".into(),
                dispatched: true,
                blocking,
                since: 1,
            },
        );
        map
    }

    #[test]
    fn review_is_off_by_default() {
        let (record, workspaces) = green_pr_epic();
        let snap = resolve_fresh(&record, &workspaces);
        let plan = plan_reviews(
            &snap,
            &lazybox_core::EpicPolicies::default(),
            &HashMap::new(),
            &HashSet::new(),
        );
        assert!(plan.dispatch.is_empty(), "an unarmed epic reviews nothing");
    }

    #[test]
    fn review_fires_once_per_green_run() {
        let (record, workspaces) = green_pr_epic();
        let snap = resolve_fresh(&record, &workspaces);
        let policies = armed(lazybox_core::EpicLatch::AutoReview);

        let first = plan_reviews(&snap, &policies, &HashMap::new(), &HashSet::new());
        assert_eq!(
            first.dispatch,
            vec![WorkspaceKey::new("a")],
            "a green PR with no review run starts one"
        );

        // With the row in place the same snapshot plans nothing more — the
        // "fires once" property.
        let again = plan_reviews(&snap, &policies, &review_row("a", false), &HashSet::new());
        assert!(again.dispatch.is_empty());
        assert!(again.clear.is_empty(), "a still-green member keeps its row");
    }

    /// CI goes red → the row is dropped, so the next green re-reviews once.
    #[test]
    fn a_re_green_after_fixes_reviews_again() {
        let mut a = ws("a");
        pr(&mut a, TaskState::Open, CiStatus::Failure);
        let record = record_with(&["a"]);
        let red = resolve_fresh(&record, &[a]);
        let policies = armed(lazybox_core::EpicLatch::AutoReview);

        let plan = plan_reviews(&red, &policies, &review_row("a", false), &HashSet::new());
        assert_eq!(plan.clear, vec![WorkspaceKey::new("a")]);
        assert!(plan.dispatch.is_empty(), "a red PR is not reviewed");

        // Green again with the row now cleared → one more review.
        let (record, workspaces) = green_pr_epic();
        let green = resolve_fresh(&record, &workspaces);
        assert_eq!(
            plan_reviews(&green, &policies, &HashMap::new(), &HashSet::new()).dispatch,
            vec![WorkspaceKey::new("a")]
        );
    }

    #[test]
    fn review_stands_down_on_an_opt_out_label() {
        let (record, workspaces) = green_pr_epic();
        let snap = resolve_fresh(&record, &workspaces);
        let mut opted = HashSet::new();
        opted.insert(WorkspaceKey::new("a"));
        assert!(
            plan_reviews(
                &snap,
                &armed(lazybox_core::EpicLatch::AutoReview),
                &HashMap::new(),
                &opted
            )
            .dispatch
            .is_empty()
        );
    }

    /// A blocking verdict flips the member's status and keeps the hold —
    /// the row must not be cleared just because the status is no longer the
    /// `PrOpen`/`Mergeable` shape `member_is_green` recognizes.
    #[test]
    fn a_blocking_review_holds_the_member_and_keeps_its_row() {
        let (record, workspaces) = green_pr_epic();
        let latches = LatchInputs {
            review_blocking: HashSet::from([WorkspaceKey::new("a")]),
            ..Default::default()
        };
        let snap = resolve_with_latches(&record, &workspaces, &latches);
        assert_eq!(snap.members[0].status, EpicMemberStatus::ReviewBlocked);
        assert_eq!(snap.blocked, 1);

        let plan = plan_reviews(
            &snap,
            &armed(lazybox_core::EpicLatch::AutoReview),
            &review_row("a", true),
            &HashSet::new(),
        );
        assert!(
            plan.clear.is_empty(),
            "the hold must outlive the green read"
        );
        assert!(plan.dispatch.is_empty());
    }

    /// A review verdict reads as its own delta, not a bare status move — and
    /// on a single-member epic it is the ONLY delta: a review-blocked member
    /// is actionable, so the epic must not also report itself Stalled.
    #[test]
    fn a_review_verdict_diffs_as_reviewed() {
        let (record, workspaces) = green_pr_epic();
        let clean = resolve_fresh(&record, &workspaces);
        let blocked = resolve_with_latches(
            &record,
            &workspaces,
            &LatchInputs {
                review_blocking: HashSet::from([WorkspaceKey::new("a")]),
                ..Default::default()
            },
        );
        assert_eq!(
            diff(Some(&clean), &blocked),
            vec![EpicDelta::Reviewed {
                key: WorkspaceKey::new("a"),
                blocking: true
            }]
        );
        assert_eq!(
            diff(Some(&blocked), &clean),
            vec![EpicDelta::Reviewed {
                key: WorkspaceKey::new("a"),
                blocking: false
            }]
        );
    }

    #[test]
    fn merge_in_order_is_off_by_default() {
        let (record, workspaces) = green_pr_epic();
        let snap = resolve_fresh(&record, &workspaces);
        assert!(
            plan_merge_arming(
                &snap,
                &lazybox_core::EpicPolicies::default(),
                &HashSet::new(),
                &HashSet::new()
            )
            .is_empty()
        );
    }

    /// Every PR member is armed regardless of the order it went green in —
    /// P3's hold, not the arming, supplies the sequence.
    #[test]
    fn merge_in_order_arms_every_pr_member_once() {
        let mut a = ws("a");
        pr(&mut a, TaskState::Open, CiStatus::Success);
        let mut b = ws("b");
        pr(&mut b, TaskState::Open, CiStatus::Failure);
        let mut c = ws("c"); // issue-only: nothing to arm.
        c.gh_issues = vec![task("github", "o/r#3")];

        let record = record_with(&["a", "b", "c"]);
        let snap = resolve_fresh(&record, &[a, b, c]);
        let policies = armed(lazybox_core::EpicLatch::MergeInOrder);

        let mut armed_now = plan_merge_arming(&snap, &policies, &HashSet::new(), &HashSet::new());
        armed_now.sort();
        assert_eq!(
            armed_now,
            vec![WorkspaceKey::new("a"), WorkspaceKey::new("b")],
            "both live PRs arm; the issue-only member has no PR to arm"
        );

        // Already-armed members are not re-sent — "fires once".
        let already = HashSet::from([WorkspaceKey::new("a"), WorkspaceKey::new("b")]);
        assert!(plan_merge_arming(&snap, &policies, &already, &HashSet::new()).is_empty());
    }

    #[test]
    fn merge_in_order_stands_down_on_an_opt_out_label() {
        let (record, workspaces) = green_pr_epic();
        let snap = resolve_fresh(&record, &workspaces);
        let opted = HashSet::from([WorkspaceKey::new("a")]);
        assert!(
            plan_merge_arming(
                &snap,
                &armed(lazybox_core::EpicLatch::MergeInOrder),
                &HashSet::new(),
                &opted
            )
            .is_empty()
        );
    }

    // ── contracts (#1525 step 5) ────────────────────────────────────────

    /// Consumer `b` declares `Contract: a`. Until `a` publishes, `b` is
    /// blocked with reason `contract` — and *not* via a dependency edge.
    fn contract_epic() -> (EpicRecord, Vec<Workspace>) {
        let mut a = ws("a");
        a.gh_issues = vec![task("github", "o/r#1")];
        let mut b = ws("b");
        let mut b_issue = task("github", "o/r#2");
        b_issue.contracts = vec![task("github", "o/r#1").id];
        b.gh_issues = vec![b_issue];
        (record_with(&["a", "b"]), vec![a, b])
    }

    #[test]
    fn an_unpublished_contract_blocks_the_consumer() {
        let (record, workspaces) = contract_epic();
        let snap = resolve_fresh(&record, &workspaces);
        let b = snap.members.iter().find(|m| m.key.as_str() == "b").unwrap();
        assert_eq!(b.status, EpicMemberStatus::Blocked);
        assert_eq!(b.blocked_reason.as_deref(), Some("contract"));
        assert!(
            b.blocked_by.is_empty() && b.external_blockers.is_empty(),
            "a contract is not a dependency edge and not an external task"
        );
        let blocker = b
            .blockers
            .iter()
            .find(|x| x.kind == BlockerKind::Contract)
            .expect("a contract blocker");
        assert_eq!(blocker.owner, BlockerOwner::Agent(WorkspaceKey::new("a")));
    }

    #[test]
    fn a_published_contract_unblocks_the_consumer() {
        let (record, workspaces) = contract_epic();
        let latches = LatchInputs {
            published_contracts: HashMap::from([(
                "e".to_string(),
                HashSet::from([WorkspaceKey::new("a")]),
            )]),
            ..Default::default()
        };
        let snap = resolve_with_latches(&record, &workspaces, &latches);
        let b = snap.members.iter().find(|m| m.key.as_str() == "b").unwrap();
        assert_eq!(b.status, EpicMemberStatus::Ready);
        assert!(b.blocked_reason.is_none());
        assert!(b.blockers.is_empty());
    }

    /// A contract edge does not gate the *merge* the way a dependency does,
    /// and it does not level waves — it only gates starting.
    #[test]
    fn a_contract_edge_is_typed_and_does_not_imply_merge_order() {
        let (record, workspaces) = contract_epic();
        let snap = resolve_fresh(&record, &workspaces);
        assert!(snap.edges.contains(&EpicEdge {
            from: WorkspaceKey::new("b"),
            to: WorkspaceKey::new("a"),
            kind: EdgeKind::Contract,
        }));
        assert!(
            !snap.edges.iter().any(|e| e.kind == EdgeKind::MergeAfter),
            "a contract must not imply a landing-order edge"
        );
        // The consumer stays in wave 0: a contract is satisfied by a note,
        // not by the producer finishing, so it does not deepen the graph.
        assert!(snap.members.iter().all(|m| m.wave == 0));
    }

    /// A contract naming a task the epic does not contain is out of scope —
    /// it could never be observed satisfied, so it must not block forever.
    #[test]
    fn a_contract_on_a_non_member_is_ignored() {
        let mut b = ws("b");
        let mut b_issue = task("github", "o/r#2");
        b_issue.contracts = vec![task("github", "other/repo#99").id];
        b.gh_issues = vec![b_issue];
        let snap = resolve_fresh(&record_with(&["b"]), &[b]);
        assert_eq!(snap.members[0].status, EpicMemberStatus::Ready);
        assert!(snap.members[0].blocked_reason.is_none());
    }

    /// End-to-end through `post_note`'s hook: a Reviewer's `blocking` verdict
    /// records the hold (which gates the merge and shows as `ReviewBlocked`),
    /// and a later `clean` verdict releases it.
    #[tokio::test]
    async fn a_review_note_records_then_releases_the_hold() {
        let config = ServerConfig::in_memory();
        let mut member = ws("w");
        pr(&mut member, TaskState::Open, CiStatus::Success);
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: "w".to_string(),
                created_at: Utc::now(),
                workspace_json: Some(serde_json::to_string(&member).unwrap()),
            })
            .unwrap();
        upsert(&config, record_with(&["w"])).await;

        let key = WorkspaceKey::new("w");
        // A verdict with no open review run is ignored — the stage did not
        // start it, so a stray note cannot invent a hold.
        on_note_posted(&config, &review_note(&["review", "epic:e", "blocking"])).await;
        assert!(!review_blocks_merge(&config, &key));

        // With a run open, the blocking verdict lands.
        persist_review(
            &config,
            &ReviewState {
                workspace: key.clone(),
                epic: "e".into(),
                dispatched: true,
                blocking: false,
                since: 1,
            },
        )
        .unwrap();
        on_note_posted(&config, &review_note(&["review", "epic:e", "blocking"])).await;
        assert!(review_blocks_merge(&config, &key));
        let snaps = all_snapshots(&config).await;
        assert_eq!(
            snaps.iter().find(|s| s.key == "e").unwrap().members[0].status,
            EpicMemberStatus::ReviewBlocked
        );

        on_note_posted(&config, &review_note(&["review", "epic:e", "clean"])).await;
        assert!(!review_blocks_merge(&config, &key));
    }

    /// A note that isn't a verdict (no `review` tag, or a `review` tag with no
    /// `blocking`/`clean`) says nothing about the merge and must not move it.
    #[tokio::test]
    async fn a_non_verdict_note_leaves_the_hold_alone() {
        let config = ServerConfig::in_memory();
        let key = WorkspaceKey::new("w");
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: "w".to_string(),
                created_at: Utc::now(),
                workspace_json: Some(serde_json::to_string(&ws("w")).unwrap()),
            })
            .unwrap();
        upsert(&config, record_with(&["w"])).await;
        persist_review(
            &config,
            &ReviewState {
                workspace: key.clone(),
                epic: "e".into(),
                dispatched: true,
                blocking: true,
                since: 1,
            },
        )
        .unwrap();
        on_note_posted(&config, &review_note(&["contract", "epic:e"])).await;
        on_note_posted(&config, &review_note(&["review", "epic:e"])).await;
        assert!(
            review_blocks_merge(&config, &key),
            "neither note carries a verdict, so the standing hold stands"
        );
    }

    /// A verdict tagged for an epic the daemon does not know is ignored — the
    /// tag names the epic, so an unknown one has no member to act on.
    #[tokio::test]
    async fn a_verdict_for_an_unknown_epic_is_ignored() {
        let config = ServerConfig::in_memory();
        let key = WorkspaceKey::new("w");
        persist_review(
            &config,
            &ReviewState {
                workspace: key.clone(),
                epic: "e".into(),
                dispatched: true,
                blocking: false,
                since: 1,
            },
        )
        .unwrap();
        on_note_posted(&config, &review_note(&["review", "epic:nope", "blocking"])).await;
        assert!(!review_blocks_merge(&config, &key));
    }

    /// Post a `contract` note the way the blackboard does: write the kv row,
    /// then run the `post_note` hook that latches it.
    async fn post_contract(config: &ServerConfig, seq: u64, author: &str, ts: i64, text: &str) {
        let note = crate::mcp::Note {
            author: author.into(),
            scope: "global".into(),
            tags: vec!["contract".into(), "epic:e".into()],
            ts,
            text: text.into(),
        };
        config
            .store
            .set_kv(
                &format!("lazybox:note:global:{seq:012}"),
                &serde_json::to_string(&note).unwrap(),
            )
            .unwrap();
        on_note_posted(config, &note).await;
    }

    /// The single latched row for `producer`, whatever epic it belongs to.
    fn latched_row(config: &ServerConfig, producer: &str) -> PublishedContract {
        list_published_contracts(config)
            .into_iter()
            .find(|r| r.producer.as_str() == producer)
            .expect("a latched contract")
    }

    /// Retention evicting a note. `post_note`'s own prune is exercised
    /// end-to-end in `mcp::tests::retention_cannot_un_publish_a_contract`;
    /// here the eviction is simulated so the epic-side assertions stay
    /// readable.
    fn evict_note(config: &ServerConfig, seq: u64) {
        config
            .store
            .delete_kv(&format!("lazybox:note:global:{seq:012}"))
            .unwrap();
    }

    fn review_note(tags: &[&str]) -> crate::mcp::Note {
        crate::mcp::Note {
            author: "w".into(),
            scope: "global".into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ts: 1,
            text: "findings".into(),
        }
    }

    /// Setting the latches persists them on the record and rides the snapshot,
    /// so a client renders the pills without a second read.
    #[tokio::test]
    async fn set_policies_persists_and_reaches_the_snapshot() {
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
        assert_eq!(
            load(&config, "e").unwrap().unwrap().policies,
            lazybox_core::EpicPolicies::default()
        );

        set_policies(&config, "e", armed(lazybox_core::EpicLatch::AutoReview)).await;
        assert!(
            load(&config, "e")
                .unwrap()
                .unwrap()
                .policies
                .armed(lazybox_core::EpicLatch::AutoReview)
        );
        let snaps = all_snapshots(&config).await;
        assert_eq!(
            snaps
                .iter()
                .find(|s| s.key == "e")
                .unwrap()
                .policies
                .armed_latches(),
            vec![lazybox_core::EpicLatch::AutoReview]
        );
    }

    /// The Worker preamble quotes the epic's published contracts, fenced as
    /// untrusted content — they are another agent's words.
    #[tokio::test]
    async fn worker_ctx_quotes_published_contracts_fenced() {
        let config = ServerConfig::in_memory();
        let mut member = ws("w");
        member.role = Some(Role::Worker);
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: "w".to_string(),
                created_at: Utc::now(),
                workspace_json: Some(serde_json::to_string(&member).unwrap()),
            })
            .unwrap();
        upsert(&config, record_with(&["w"])).await;
        post_contract(
            &config,
            1,
            "producer",
            1,
            "POST /v1/tokens returns {id, expires_at}",
        )
        .await;

        let (role, ctx) = role_prompt_ctx(&config, &member).await.expect("a role");
        assert_eq!(role, Role::Worker);
        assert_eq!(
            ctx.contract_notes,
            vec!["POST /v1/tokens returns {id, expires_at}".to_string()]
        );
        let preamble = lazybox_core::prompts::role_preamble(role, &ctx);
        assert!(preamble.contains("<untrusted-content source=\"agent-authored contract\">"));
        assert!(preamble.contains("POST /v1/tokens"));
    }

    /// **The latch must act on standing state, not on a transition.** Arming a
    /// latch moves no member's status, so `diff` yields an empty delta — and
    /// gating dispatch on that delta made every latch a no-op at exactly the
    /// moment the operator armed it. Asserted through `recompute_all`, because
    /// the bug lived in the caller, not in `plan_reviews`.
    ///
    /// The `REVIEW` latch is the observable one: `dispatch_reviewer` persists
    /// its row *before* spawning, so the row's existence proves the latch ran
    /// without depending on a real agent starting.
    #[tokio::test]
    async fn arming_a_latch_acts_on_members_that_are_already_eligible() {
        let config = ServerConfig::in_memory();
        let mut member = ws("w");
        pr(&mut member, TaskState::Open, CiStatus::Success);
        save_ws(&config, &member);

        // First recompute latches the snapshot; the epic is unarmed, so the
        // review stage does nothing yet.
        upsert(&config, record_with(&["w"])).await;
        assert!(
            list_reviews(&config).is_empty(),
            "an unarmed epic must not review"
        );

        // Arming changes no member status, so the delta is empty.
        set_policies(&config, "e", armed(lazybox_core::EpicLatch::AutoReview)).await;
        assert!(
            list_reviews(&config).contains_key(&WorkspaceKey::new("w")),
            "arming REVIEW on an already-green member must open a review run \
             even though no member transitioned"
        );
    }

    /// The same dead-latch bug in its second form: after a daemon restart the
    /// `EpicMemory` is empty, so `diff(None, _)` returns no deltas at all. An
    /// epic armed yesterday must still act on the state it comes back to.
    #[tokio::test]
    async fn a_first_sight_snapshot_acts_on_members_that_are_already_eligible() {
        let config = ServerConfig::in_memory();
        let mut member = ws("w");
        pr(&mut member, TaskState::Open, CiStatus::Success);
        save_ws(&config, &member);

        // Persist an already-armed record without ever recomputing — the state
        // a daemon boots into.
        let mut record = record_with(&["w"]);
        record.policies = armed(lazybox_core::EpicLatch::AutoReview);
        persist(&config, &record).unwrap();

        recompute_all(&config).await;
        assert!(
            list_reviews(&config).contains_key(&WorkspaceKey::new("w")),
            "the first recompute after a restart must act on standing state"
        );
    }

    /// **A hold must not outlive the epic that raised it.** `review_blocks_merge`
    /// is keyed on the workspace alone, so a row stranded by an archived or
    /// unassigned member would hold that PR's merge forever with no epic left
    /// to explain it.
    #[tokio::test]
    async fn archiving_an_epic_releases_the_merges_its_reviews_were_holding() {
        let config = ServerConfig::in_memory();
        let mut member = ws("w");
        pr(&mut member, TaskState::Open, CiStatus::Success);
        save_ws(&config, &member);
        upsert(&config, record_with(&["w"])).await;

        let key = WorkspaceKey::new("w");
        persist_review(
            &config,
            &ReviewState {
                workspace: key.clone(),
                epic: "e".into(),
                dispatched: true,
                blocking: true,
                since: 1,
            },
        )
        .unwrap();
        assert!(review_blocks_merge(&config, &key));

        archive(&config, "e").await;
        assert!(
            !review_blocks_merge(&config, &key),
            "archiving the epic must drop the review row it owns, or the PR's \
             merge is wedged forever"
        );
    }

    /// The same prune, reached the other way: the member leaves the epic while
    /// the epic itself stays live.
    #[tokio::test]
    async fn unassigning_a_member_releases_the_merge_its_review_was_holding() {
        let config = ServerConfig::in_memory();
        for k in ["w", "keep"] {
            let mut m = ws(k);
            pr(&mut m, TaskState::Open, CiStatus::Success);
            save_ws(&config, &m);
        }
        upsert(&config, record_with(&["w", "keep"])).await;

        let key = WorkspaceKey::new("w");
        let kept = WorkspaceKey::new("keep");
        for member in [&key, &kept] {
            persist_review(
                &config,
                &ReviewState {
                    workspace: member.clone(),
                    epic: "e".into(),
                    dispatched: true,
                    blocking: true,
                    since: 1,
                },
            )
            .unwrap();
        }

        assign(&config, "e", key.clone(), false).await;
        assert!(
            !review_blocks_merge(&config, &key),
            "the dropped member frees"
        );
        assert!(
            review_blocks_merge(&config, &kept),
            "a member still in the epic keeps its hold"
        );
    }

    /// A verdict must name the epic whose latch opened the run. Without that
    /// check the `epic:<key>` tag is decorative — any live epic's tag passes,
    /// letting a note flip a hold a different epic raised.
    #[tokio::test]
    async fn a_verdict_tagged_for_another_epic_does_not_flip_the_hold() {
        let config = ServerConfig::in_memory();
        save_ws(&config, &ws("w"));
        upsert(&config, record_with(&["w"])).await;
        let mut other = EpicRecord::new(EpicKey::new("other"), "Other", Utc::now());
        other.members = vec![WorkspaceKey::new("w")];
        upsert(&config, other).await;

        let key = WorkspaceKey::new("w");
        persist_review(
            &config,
            &ReviewState {
                workspace: key.clone(),
                epic: "e".into(),
                dispatched: true,
                blocking: false,
                since: 1,
            },
        )
        .unwrap();

        on_note_posted(&config, &review_note(&["review", "epic:other", "blocking"])).await;
        assert!(
            !review_blocks_merge(&config, &key),
            "a verdict naming a different epic must not raise this run's hold"
        );

        on_note_posted(&config, &review_note(&["review", "epic:e", "blocking"])).await;
        assert!(
            review_blocks_merge(&config, &key),
            "the run's own epic tag is accepted"
        );
    }

    /// A contract published for one epic says nothing about another epic's
    /// interface — the satisfied set is keyed by epic, not flattened.
    #[test]
    fn a_contract_published_for_another_epic_does_not_satisfy_this_one() {
        let (record, workspaces) = contract_epic();
        let elsewhere = LatchInputs {
            published_contracts: HashMap::from([(
                "other".to_string(),
                HashSet::from([WorkspaceKey::new("a")]),
            )]),
            ..Default::default()
        };
        let snap = resolve_with_latches(&record, &workspaces, &elsewhere);
        let b = snap.members.iter().find(|m| m.key.as_str() == "b").unwrap();
        assert_eq!(
            b.blocked_reason.as_deref(),
            Some("contract"),
            "only a contract published for THIS epic satisfies its edge"
        );
    }

    /// One blackboard read per recompute, not one per epic: the resolver must
    /// bucket a single scan by epic tag.
    #[test]
    fn latch_inputs_bucket_one_scan_across_every_epic() {
        let config = ServerConfig::in_memory();
        let mut first = EpicRecord::new(EpicKey::new("one"), "One", Utc::now());
        first.members = vec![WorkspaceKey::new("a")];
        let mut second = EpicRecord::new(EpicKey::new("two"), "Two", Utc::now());
        second.members = vec![WorkspaceKey::new("b")];
        let mut archived = EpicRecord::new(EpicKey::new("gone"), "Gone", Utc::now());
        archived.archived = true;

        for (seq, author, tags) in [
            (1u64, "a", vec!["contract", "epic:one"]),
            (2, "b", vec!["contract", "epic:two"]),
            (3, "c", vec!["contract", "epic:gone"]),
            (4, "d", vec!["review", "epic:one"]),
        ] {
            let note = crate::mcp::Note {
                author: author.into(),
                scope: "global".into(),
                tags: tags.iter().map(|t| t.to_string()).collect(),
                ts: seq as i64,
                text: "x".into(),
            };
            config
                .store
                .set_kv(
                    &format!("lazybox:note:global:{seq:012}"),
                    &serde_json::to_string(&note).unwrap(),
                )
                .unwrap();
        }

        let latches = LatchInputs::load(&config, &[first, second, archived]);
        assert_eq!(
            latches.contracts_for("one"),
            Some(&HashSet::from([WorkspaceKey::new("a")]))
        );
        assert_eq!(
            latches.contracts_for("two"),
            Some(&HashSet::from([WorkspaceKey::new("b")]))
        );
        assert_eq!(
            latches.contracts_for("gone"),
            None,
            "an archived epic contributes no bucket"
        );
        assert_eq!(
            latches.contracts_for("one").map(|s| s.len()),
            Some(1),
            "a `review` note must not land in a contract bucket"
        );
    }

    /// An epic whose only outstanding member is review-blocked is waiting on
    /// findings someone can act on — not stalled on an external task or cycle.
    #[test]
    fn a_review_blocked_member_does_not_stall_the_epic() {
        let (record, workspaces) = green_pr_epic();
        let snap = resolve_with_latches(
            &record,
            &workspaces,
            &LatchInputs {
                review_blocking: HashSet::from([WorkspaceKey::new("a")]),
                ..Default::default()
            },
        );
        assert_eq!(snap.members[0].status, EpicMemberStatus::ReviewBlocked);
        assert!(
            !stalled(&snap),
            "a review-blocked member is actionable, so the epic is not stalled"
        );
    }

    /// ORDER leaves a review-blocked PR alone rather than painting an `ARM`
    /// pill over a merge the review hold is refusing.
    #[test]
    fn merge_in_order_skips_a_review_blocked_member() {
        let (record, workspaces) = green_pr_epic();
        let snap = resolve_with_latches(
            &record,
            &workspaces,
            &LatchInputs {
                review_blocking: HashSet::from([WorkspaceKey::new("a")]),
                ..Default::default()
            },
        );
        assert!(
            plan_merge_arming(
                &snap,
                &armed(lazybox_core::EpicLatch::MergeInOrder),
                &HashSet::new(),
                &HashSet::new()
            )
            .is_empty()
        );
    }

    /// **The bug (#1577).** The blackboard is a rolling buffer, so a producer
    /// that keeps posting evicts its own contract note. Re-deriving
    /// satisfaction from the notes alone re-blocks a consumer whose interface
    /// was agreed — and, under `AUTO`, one a Worker is already running on.
    /// Satisfaction is latched at the first observation instead.
    #[tokio::test]
    async fn an_evicted_contract_note_leaves_the_consumer_unblocked() {
        let config = ServerConfig::in_memory();
        let (record, workspaces) = contract_epic();
        upsert(&config, record.clone()).await;
        post_contract(&config, 1, "a", 1, "GET /v1/things -> [{id}]").await;

        let satisfied = LatchInputs::load(&config, std::slice::from_ref(&record));
        assert_eq!(
            satisfied.contracts_for("e"),
            Some(&HashSet::from([WorkspaceKey::new("a")]))
        );

        evict_note(&config, 1);
        let after = LatchInputs::load(&config, std::slice::from_ref(&record));
        assert_eq!(
            after.contracts_for("e"),
            Some(&HashSet::from([WorkspaceKey::new("a")])),
            "the latch outlives the note that took it"
        );
        let b = resolve_with_latches(&record, &workspaces, &after);
        let b = b.members.iter().find(|m| m.key.as_str() == "b").unwrap();
        assert_eq!(b.blocked_reason, None);
        assert_ne!(b.status, EpicMemberStatus::Blocked);
    }

    /// The consumer's Worker preamble reads the latched contract too, so a
    /// Worker dispatched after the note aged out is still briefed with the
    /// interface rather than with nothing.
    #[tokio::test]
    async fn an_evicted_contract_is_still_quoted_to_a_worker() {
        let config = ServerConfig::in_memory();
        let mut member = ws("w");
        member.role = Some(Role::Worker);
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: "w".to_string(),
                created_at: Utc::now(),
                workspace_json: Some(serde_json::to_string(&member).unwrap()),
            })
            .unwrap();
        upsert(&config, record_with(&["w"])).await;
        post_contract(&config, 1, "producer", 1, "POST /v1/tokens -> {id}").await;
        evict_note(&config, 1);

        let (_, ctx) = role_prompt_ctx(&config, &member).await.expect("a role");
        assert_eq!(ctx.contract_notes, vec!["POST /v1/tokens -> {id}"]);
    }

    /// **Regression (review finding 1).** `notes_with_tags` orders by
    /// `(ts, seq)`, so two notes posted inside one millisecond are ordered by
    /// `seq` alone — which a row does not carry. Deciding "is this newer?" on
    /// `ts` therefore discarded a correction posted straight after its first
    /// draft, and the row went on quoting the superseded interface for the
    /// life of the epic. Satisfaction compares the interface, not the clock.
    #[tokio::test]
    async fn a_same_millisecond_correction_still_replaces_the_contract() {
        let config = ServerConfig::in_memory();
        upsert(&config, record_with(&["a"])).await;
        post_contract(&config, 1, "a", 7, "GET /v1/thing -> {id}").await;
        post_contract(&config, 2, "a", 7, "GET /v1/thing -> {id, etag}").await;

        let row = latched_row(&config, "a");
        assert_eq!(
            row.text, "GET /v1/thing -> {id, etag}",
            "the correction wins even though both notes share a millisecond"
        );
        assert_eq!(row.revision, 2);
    }

    /// A re-post of the *same* interface is not a new revision — `revision`
    /// counts changes, which is what makes the log line worth reading.
    #[tokio::test]
    async fn re_publishing_an_unchanged_contract_is_not_a_revision() {
        let config = ServerConfig::in_memory();
        upsert(&config, record_with(&["a"])).await;
        post_contract(&config, 1, "a", 10, "v1").await;
        post_contract(&config, 2, "a", 20, "v1").await;
        assert_eq!(latched_row(&config, "a").revision, 1);

        post_contract(&config, 3, "a", 30, "v2").await;
        let changed = latched_row(&config, "a");
        assert_eq!(changed.revision, 2);
        assert_eq!(changed.text, "v2");
        assert_eq!(changed.published_at, 30);
    }

    /// **Regression (review finding 2).** Contract rows were pruned when their
    /// epic disappeared, copying the review rows' cache semantics onto a
    /// durable record. Once the note is evicted the row holds the only copy of
    /// the interface, and an epic key is `slugify(name)` — so deleting an epic
    /// and recreating it from the same name re-blocked every consumer on a
    /// contract that no longer existed anywhere. Rows outlive their epic.
    #[tokio::test]
    async fn a_contract_outlives_the_deletion_and_recreation_of_its_epic() {
        let config = ServerConfig::in_memory();
        let (record, workspaces) = contract_epic();
        upsert(&config, record.clone()).await;
        post_contract(&config, 1, "a", 1, "GET /v1/things -> [{id}]").await;
        evict_note(&config, 1);

        // The operator deletes the epic (to re-anchor it, say) and recreates it
        // from the same name, so `EpicKey::from_name` yields the same key.
        config.store.delete_kv("epic:e").unwrap();
        recompute_all(&config).await;
        upsert(&config, record.clone()).await;

        let latches = LatchInputs::load(&config, std::slice::from_ref(&record));
        assert_eq!(
            latches.contracts_for("e"),
            Some(&HashSet::from([WorkspaceKey::new("a")])),
            "the interface was published; recreating the epic must not un-publish it"
        );
        let snap = resolve_with_latches(&record, &workspaces, &latches);
        let b = snap.members.iter().find(|m| m.key.as_str() == "b").unwrap();
        assert_eq!(b.blocked_reason, None);
        assert_eq!(
            latched_row(&config, "a").text,
            "GET /v1/things -> [{id}]",
            "and the only surviving copy of the text is still there"
        );
    }

    /// An archived epic keeps its contracts, so unarchiving does not demand
    /// every interface be published again.
    #[tokio::test]
    async fn an_archived_epic_keeps_its_contracts() {
        let config = ServerConfig::in_memory();
        let mut record = record_with(&["a"]);
        upsert(&config, record.clone()).await;
        post_contract(&config, 1, "a", 1, "v1").await;

        record.archived = true;
        upsert(&config, record).await;
        assert_eq!(list_published_contracts(&config).len(), 1);
    }

    /// **Regression (review finding 3).** `post_note` holds the blackboard's
    /// process-wide write lock across `on_note_posted`, and `recompute_all`
    /// spawns agents and calls GitHub. A bare `contract` tag used to reach it
    /// with no gate at all, so one agent could stall every other agent's
    /// `post_note` just by posting. The `epic:<key>` tag is the gate.
    #[tokio::test]
    async fn a_contract_note_naming_no_live_epic_drives_nothing() {
        let config = ServerConfig::in_memory();
        upsert(&config, record_with(&["a"])).await;

        for tags in [
            vec!["contract".to_string()],
            vec!["contract".to_string(), "epic:typo".to_string()],
        ] {
            let note = crate::mcp::Note {
                author: "a".into(),
                scope: "global".into(),
                tags,
                ts: 1,
                text: "not for any epic here".into(),
            };
            assert!(!names_a_live_epic(&config, &note));
        }

        let mut archived = record_with(&["a"]);
        archived.archived = true;
        upsert(&config, archived).await;
        let note = crate::mcp::Note {
            author: "a".into(),
            scope: "global".into(),
            tags: vec!["contract".into(), "epic:e".into()],
            ts: 1,
            text: "for an archived epic".into(),
        };
        assert!(
            !names_a_live_epic(&config, &note),
            "an archived epic is not a live one"
        );
    }
}
