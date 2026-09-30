//! The task/plan store: a unit of work with an immutable id.
//!
//! `docs/agent-coordination-v2.md` names this the missing pillar, and #1898
//! (phase 1) says so from the other side: it ships the TODO checklist as a
//! field on [`Workspace`](crate::Workspace), and flags in its own
//! body that "a checklist hanging off a workspace cannot be the shared plan
//! that phases 3–4 subscribe to". This module is that plan.
//!
//! Two properties are the whole point, and both are defended by tests below.
//!
//! **The id is immutable and collision-free.** [`WorkId`] is a fresh uuid,
//! deliberately *not* [`TaskId`]. That type is
//! `{ source, key }` rendered `github:owner/repo#N`, minted by the provider,
//! and it does not identify a unit of work across its life: at the issue→PR
//! fold the addressing key changes and every keyed row has to be hand-moved,
//! a bug class that has shipped twice (#1793 blockers, #1837 MCP tokens) with
//! notes, requests and reviews still unmoved. Tracker records live in
//! [`Link`]s instead, so a fold rewrites a link and never an id. The id must
//! also survive the kv key sanitizer injectively — which maps every byte
//! outside `[A-Za-z0-9_.-]` to `-`, so `github:my-org/tools#42` and
//! `github:my/org-tools#42` collide (that collision *is* #1836). A uuid's
//! rendering uses only hex and `-`, so it passes through untouched.
//!
//! **Lifecycle is declared intent, not observed liveness.** It is the third
//! state enum in this codebase and must not be confused with either of the
//! others: `AgentState` (in `lazybox-ipc`) is liveness *observed* from the PTY
//! and lifecycle hooks, and [`TaskState`](crate::TaskState) is the
//! *tracker record's* state, owned by the provider. [`Lifecycle`] is what
//! whoever owns the work says about it. They disagree routinely and neither is
//! wrong when they do — an agent parked at a permission prompt is
//! `AgentState::InputNeeded` while its task is legitimately [`Lifecycle::Underway`],
//! and an agent whose turn ended is `Done` while its task stays `Underway`
//! until it reports. So `Lifecycle` never moves on an `AgentState` change
//! alone, with exactly one exception that must not be silent:
//! [`Task::fail_from_agent_exit`].

use crate::task::TaskId;
use crate::workspace::{SessionId, WorkspaceKey};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Bumped when a stored row's shape changes in a way a reader must notice.
pub const WORK_SCHEMA_VERSION: u32 = 1;

/// kv prefix for a stored [`Task`]. The id is appended raw: it is already
/// sanitizer-safe, which is the property `sanitize_key` collisions cost us.
pub const WORK_KEY_PREFIX: &str = "work:";
/// kv prefix for a stored [`Plan`].
pub const PLAN_KEY_PREFIX: &str = "plan:";

/// A unit of work's identity, for its whole life. Daemon-minted at create.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkId(pub Uuid);

impl WorkId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// The kv key this row is stored under.
    pub fn storage_key(&self) -> String {
        format!("{WORK_KEY_PREFIX}{}", self.0)
    }
}

impl Default for WorkId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for WorkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for WorkId {
    type Err = uuid::Error;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(raw)?))
    }
}

/// A plan's identity — the root of a TODO tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PlanId(pub Uuid);

impl PlanId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn storage_key(&self) -> String {
        format!("{PLAN_KEY_PREFIX}{}", self.0)
    }
}

impl Default for PlanId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for PlanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Who is asking or doing. Exactly three variants, and unassigned is not one
/// of them — that is `owner: None`. An agent is addressed by its *workspace*;
/// `session` rides along only as provenance, recording which session was live
/// when the row was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Party {
    Human,
    Agent {
        workspace: WorkspaceKey,
        #[serde(default)]
        session: Option<SessionId>,
    },
    /// lazybox itself (auto-fix, a resume). A legal `requester` and never an
    /// `owner`: work lazybox starts is owned by whoever runs it.
    Lazybox,
}

impl Party {
    /// Whether this party may own work. `Lazybox` requests; it never owns.
    pub fn can_own(&self) -> bool {
        !matches!(self, Self::Lazybox)
    }
}

/// What a task points at. Tracker records are links, never identity, so the
/// issue→PR fold rewrites one of these and leaves the [`WorkId`] alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Link {
    Workspace(WorkspaceKey),
    /// A provider record — the issue or PR this work is for.
    Tracker(TaskId),
    Url(String),
}

/// Declared intent about a unit of work. Maps onto A2A's `TaskState` so a
/// bridge later is a mapping rather than a redesign: `Pending`/submitted,
/// `Underway`/working, `AwaitingAnswer`/input-required. `Held` is lazybox's
/// own — an operator-owned blocker — and degrades to input-required carrying
/// its reason as the question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lifecycle {
    Pending,
    Underway,
    AwaitingAnswer { question: String },
    Held { reason: String },
    Completed,
    Failed { reason: String },
    Canceled,
}

impl Lifecycle {
    /// Terminal states reject further work. This is what stops a finished task
    /// being quietly reopened by a late report from a dead agent.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed { .. } | Self::Canceled)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Underway => "underway",
            Self::AwaitingAnswer { .. } => "awaiting answer",
            Self::Held { .. } => "held",
            Self::Completed => "completed",
            Self::Failed { .. } => "failed",
            Self::Canceled => "canceled",
        }
    }
}

/// A finished task's result. Artifacts are *by reference*: the daemon stores
/// them, and a result never carries scraped scrollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkResult {
    pub summary: String,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
}

/// A pointer to an artifact the daemon holds, by the name it was spooled under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub name: String,
    /// The workspace whose artifact spool holds it.
    pub workspace: WorkspaceKey,
}

/// One provenance entry: who changed what, when. A task's history is why a
/// disagreement between two agents about its state is answerable after the
/// fact rather than a matter of opinion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkEvent {
    pub at: DateTime<Utc>,
    pub by: Party,
    /// What changed, in the daemon's own words (`"underway → completed"`), plus
    /// a cause where one exists (`"agent-exited"`).
    pub change: String,
}

/// A refused transition, with the state that refused it — so a caller reports
/// why rather than silently dropping the change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionRefused {
    pub from: Lifecycle,
    pub to: Lifecycle,
}

impl std::fmt::Display for TransitionRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a {} task rejects further work (asked for {})",
            self.from.label(),
            self.to.label()
        )
    }
}

impl std::error::Error for TransitionRefused {}

/// A unit of work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: WorkId,
    /// The TODO tree it belongs to, if any.
    #[serde(default)]
    pub plan: Option<PlanId>,
    /// Nesting — a sub-TODO. Lightweight: a checklist item is not a workspace.
    #[serde(default)]
    pub parent: Option<WorkId>,
    pub title: String,
    /// Objective · done-criteria · boundaries · output shape.
    #[serde(default)]
    pub brief: String,
    /// `None` is unassigned; it is not a [`Party`] variant.
    #[serde(default)]
    pub owner: Option<Party>,
    pub requester: Party,
    #[serde(default)]
    pub links: Vec<Link>,
    pub lifecycle: Lifecycle,
    #[serde(default)]
    pub result: Option<WorkResult>,
    #[serde(default)]
    pub history: Vec<WorkEvent>,
    /// Defaulted so rows written before a bump still decode.
    #[serde(default)]
    pub schema: u32,
}

impl Task {
    pub fn new(title: impl Into<String>, requester: Party, now: DateTime<Utc>) -> Self {
        let id = WorkId::new();
        Self {
            id,
            plan: None,
            parent: None,
            title: title.into(),
            brief: String::new(),
            owner: None,
            requester: requester.clone(),
            links: Vec::new(),
            lifecycle: Lifecycle::Pending,
            result: None,
            history: vec![WorkEvent {
                at: now,
                by: requester,
                change: "created".into(),
            }],
            schema: WORK_SCHEMA_VERSION,
        }
    }

    /// Move the task, recording provenance. Refused once terminal.
    pub fn transition(
        &mut self,
        to: Lifecycle,
        by: Party,
        now: DateTime<Utc>,
    ) -> Result<(), TransitionRefused> {
        if self.lifecycle.is_terminal() {
            return Err(TransitionRefused {
                from: self.lifecycle.clone(),
                to,
            });
        }
        self.record(&to, by, now, None);
        self.lifecycle = to;
        Ok(())
    }

    /// Finish the task with its result.
    pub fn complete(
        &mut self,
        result: WorkResult,
        by: Party,
        now: DateTime<Utc>,
    ) -> Result<(), TransitionRefused> {
        self.transition(Lifecycle::Completed, by, now)?;
        self.result = Some(result);
        Ok(())
    }

    /// The one place a `Lifecycle` moves because of an `AgentState` change, and
    /// it is deliberately loud. An agent that exited while its task is still
    /// `Underway` leaves the work neither done nor abandoned: leaving it
    /// `Underway` strands it forever, and calling it `Completed` invents a
    /// result nobody produced. So it fails, with the cause in its history.
    ///
    /// Returns whether it moved: an already-terminal task is left alone, which
    /// is what makes this safe to call from a state-change handler.
    pub fn fail_from_agent_exit(&mut self, by: Party, now: DateTime<Utc>) -> bool {
        if self.lifecycle != Lifecycle::Underway {
            return false;
        }
        let to = Lifecycle::Failed {
            reason: "the agent exited before reporting a result".into(),
        };
        self.record(&to, by, now, Some("agent-exited"));
        self.lifecycle = to;
        true
    }

    /// Whether this task points at `link`.
    pub fn links_to(&self, link: &Link) -> bool {
        self.links.contains(link)
    }

    fn record(&mut self, to: &Lifecycle, by: Party, now: DateTime<Utc>, cause: Option<&str>) {
        let change = match cause {
            Some(cause) => format!("{} → {} ({cause})", self.lifecycle.label(), to.label()),
            None => format!("{} → {}", self.lifecycle.label(), to.label()),
        };
        self.history.push(WorkEvent {
            at: now,
            by,
            change,
        });
    }
}

/// A TODO tree. Membership is by `plan` on each [`Task`]; the plan row carries
/// only what the tasks cannot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub id: PlanId,
    pub title: String,
    #[serde(default)]
    pub schema: u32,
}

impl Plan {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            id: PlanId::new(),
            title: title.into(),
            schema: WORK_SCHEMA_VERSION,
        }
    }
}

/// Rolled-up progress over a tree, for `▰▰▱ 2/3` per line and a total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
}

impl Progress {
    pub fn is_complete(&self) -> bool {
        self.total > 0 && self.done == self.total
    }
}

/// Progress for `root` and everything nested under it. Canceled tasks are
/// skipped entirely — they are neither done nor outstanding, and counting them
/// as either makes a plan that was pruned look stalled or finished.
pub fn progress(tasks: &[Task], root: WorkId) -> Progress {
    let mut progress = Progress::default();
    accumulate(tasks, Some(root), &mut progress);
    progress
}

/// Progress for a whole plan — every root it holds.
pub fn plan_progress(tasks: &[Task], plan: PlanId) -> Progress {
    let mut progress = Progress::default();
    for task in tasks
        .iter()
        .filter(|task| task.plan == Some(plan) && task.parent.is_none())
    {
        accumulate(tasks, Some(task.id), &mut progress);
    }
    progress
}

fn accumulate(tasks: &[Task], id: Option<WorkId>, progress: &mut Progress) {
    let Some(id) = id else { return };
    let Some(task) = tasks.iter().find(|task| task.id == id) else {
        return;
    };
    if task.lifecycle != Lifecycle::Canceled {
        progress.total += 1;
        if task.lifecycle == Lifecycle::Completed {
            progress.done += 1;
        }
    }
    for child in tasks.iter().filter(|other| other.parent == Some(id)) {
        accumulate(tasks, Some(child.id), progress);
    }
}

/// The workspaces a plan's tasks point at — the explicit member list for the
/// `EpicRecord` a cross-repo plan projects onto. The projection uses
/// `anchor: None` deliberately: an `EpicRecord` with an anchor re-resolves its
/// membership from that issue's sub-issue chain on every poll, so anchoring a
/// plan would sweep in rows the user never added. A TODO item's link to an
/// issue therefore stays an inert link, and anchoring stays a separate,
/// deliberate act.
pub fn plan_members(tasks: &[Task], plan: PlanId) -> Vec<WorkspaceKey> {
    let mut members: Vec<WorkspaceKey> = Vec::new();
    for task in tasks.iter().filter(|task| task.plan == Some(plan)) {
        for link in &task.links {
            if let Link::Workspace(key) = link
                && !members.contains(key)
            {
                members.push(key.clone());
            }
        }
    }
    members
}

/// Complete every task linked to `link` — the auto-check the TODO tree needs
/// when a PR merges or an issue closes. Returns the ids that moved, so a
/// caller can report what it ticked off rather than guessing. Terminal tasks
/// are skipped, so a re-delivered merge event is idempotent.
pub fn complete_linked_to(
    tasks: &mut [Task],
    link: &Link,
    by: Party,
    now: DateTime<Utc>,
) -> Vec<WorkId> {
    let mut moved = Vec::new();
    for task in tasks.iter_mut() {
        if !task.links_to(link) || task.lifecycle.is_terminal() {
            continue;
        }
        let result = WorkResult {
            summary: format!("completed automatically: {} landed", describe(link)),
            artifacts: Vec::new(),
        };
        if task.complete(result, by.clone(), now).is_ok() {
            moved.push(task.id);
        }
    }
    moved
}

fn describe(link: &Link) -> String {
    match link {
        Link::Workspace(key) => key.to_string(),
        Link::Tracker(id) => id.to_string(),
        Link::Url(url) => url.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("a valid timestamp")
    }

    fn agent(workspace: &str) -> Party {
        Party::Agent {
            workspace: WorkspaceKey::new(workspace),
            session: None,
        }
    }

    fn task(title: &str) -> Task {
        Task::new(title, Party::Human, at(0))
    }

    fn tracker(key: &str) -> Link {
        Link::Tracker(TaskId {
            source: "github".into(),
            key: key.into(),
        })
    }

    /// The whole reason the id is a uuid and not a `TaskId`. `sanitize_key`
    /// maps every byte outside `[A-Za-z0-9_.-]` to `-`, which is why
    /// `github:my-org/tools#42` and `github:my/org-tools#42` both become the
    /// same kv key and evicted each other's notes (#1836). A uuid's rendering
    /// is hex and `-`, so the sanitizer is the identity on it.
    #[test]
    fn the_id_survives_the_kv_sanitizer_untouched() {
        for _ in 0..64 {
            let id = WorkId::new();
            let rendered = id.to_string();
            assert!(
                rendered.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
                "{rendered} contains a byte the kv sanitizer would rewrite",
            );
            assert!(
                !rendered.starts_with('-') && !rendered.ends_with('-'),
                "{rendered} would be trimmed by the sanitizer",
            );
            assert_eq!(id.storage_key(), format!("work:{rendered}"));
        }
    }

    /// Two ids minted back to back differ: identity is per unit of work, not
    /// per workspace or per tracker record.
    #[test]
    fn every_unit_of_work_gets_its_own_id() {
        let ids: std::collections::HashSet<_> = (0..256).map(|_| WorkId::new()).collect();
        assert_eq!(ids.len(), 256);
    }

    /// Terminal means terminal. Without this, a late report from an agent that
    /// already died reopens finished work, which is how a plan starts
    /// disagreeing with itself.
    #[test]
    fn a_terminal_task_rejects_further_work() {
        for terminal in [
            Lifecycle::Completed,
            Lifecycle::Failed {
                reason: "boom".into(),
            },
            Lifecycle::Canceled,
        ] {
            let mut work = task("ship it");
            work.lifecycle = terminal.clone();
            let refused = work
                .transition(Lifecycle::Underway, agent("w"), at(10))
                .expect_err("a terminal task must refuse");
            assert_eq!(refused.from, terminal);
            assert_eq!(work.lifecycle, terminal, "the refusal must not mutate");
            assert!(
                refused.to_string().contains("rejects further work"),
                "the caller needs a reason it can report: {refused}",
            );
        }
    }

    /// The one exception where observed liveness moves declared intent, and it
    /// has to be loud: leaving it `Underway` strands the task forever, and
    /// calling it `Completed` invents a result nobody produced.
    #[test]
    fn an_agent_that_exits_mid_task_fails_it_with_the_cause_recorded() {
        let mut work = task("write the store");
        work.transition(Lifecycle::Underway, agent("w"), at(1))
            .expect("pending → underway");
        assert!(work.fail_from_agent_exit(Party::Lazybox, at(2)));
        assert!(matches!(work.lifecycle, Lifecycle::Failed { .. }));
        assert!(
            work.history
                .last()
                .expect("history")
                .change
                .contains("agent-exited"),
            "the cause must be in the provenance, not inferred later",
        );
        assert!(work.result.is_none(), "a failure invents no result");
    }

    /// Safe to call from a state-change handler: anything not `Underway` is
    /// left exactly as it was, so a `Done` agent on a completed task is not
    /// retroactively a failure.
    #[test]
    fn an_agent_exit_leaves_any_other_state_alone() {
        for state in [
            Lifecycle::Pending,
            Lifecycle::Completed,
            Lifecycle::Canceled,
            Lifecycle::Held {
                reason: "waiting on the operator".into(),
            },
        ] {
            let mut work = task("t");
            work.lifecycle = state.clone();
            let history = work.history.len();
            assert!(!work.fail_from_agent_exit(Party::Lazybox, at(5)));
            assert_eq!(work.lifecycle, state);
            assert_eq!(work.history.len(), history, "no phantom provenance");
        }
    }

    /// Progress rolls the tree up and skips canceled items: a pruned plan must
    /// read neither stalled nor finished.
    #[test]
    fn progress_rolls_nested_items_up_and_skips_canceled() {
        let mut root = task("root");
        let mut done = task("done child");
        let mut pruned = task("canceled child");
        let mut grandchild = task("nested, done");
        done.parent = Some(root.id);
        pruned.parent = Some(root.id);
        grandchild.parent = Some(done.id);
        done.lifecycle = Lifecycle::Completed;
        pruned.lifecycle = Lifecycle::Canceled;
        grandchild.lifecycle = Lifecycle::Completed;
        root.lifecycle = Lifecycle::Underway;
        let root_id = root.id;
        let tasks = vec![root, done, pruned, grandchild];

        // root (underway) + done + grandchild = 3 counted, 2 of them complete;
        // the canceled child is counted in neither column.
        assert_eq!(progress(&tasks, root_id), Progress { done: 2, total: 3 });
        assert!(!progress(&tasks, root_id).is_complete());
    }

    /// A plan totals every root it holds, and nothing from another plan.
    #[test]
    fn plan_progress_totals_its_own_roots_only() {
        let plan = PlanId::new();
        let other = PlanId::new();
        let mut mine = task("mine");
        let mut child = task("my child");
        let mut theirs = task("theirs");
        mine.plan = Some(plan);
        child.plan = Some(plan);
        child.parent = Some(mine.id);
        child.lifecycle = Lifecycle::Completed;
        theirs.plan = Some(other);
        theirs.lifecycle = Lifecycle::Completed;
        let tasks = vec![mine, child, theirs];
        assert_eq!(plan_progress(&tasks, plan), Progress { done: 1, total: 2 });
    }

    /// The auto-check: when a PR merges, every item pointing at it ticks off —
    /// and only those. Terminal items are skipped, so a re-delivered merge
    /// event changes nothing the second time.
    #[test]
    fn completing_by_link_ticks_off_exactly_the_items_pointing_at_it() {
        let merged = tracker("owner/repo#7");
        let unrelated = tracker("owner/repo#8");
        let mut linked = task("the work the PR does");
        let mut also_linked = task("a sibling item on the same PR");
        let mut elsewhere = task("another PR's work");
        let mut already = task("already canceled");
        linked.links = vec![merged.clone()];
        also_linked.links = vec![merged.clone()];
        elsewhere.links = vec![unrelated];
        already.links = vec![merged.clone()];
        already.lifecycle = Lifecycle::Canceled;
        let (linked_id, also_id) = (linked.id, also_linked.id);
        let mut tasks = vec![linked, also_linked, elsewhere, already];

        let moved = complete_linked_to(&mut tasks, &merged, Party::Lazybox, at(9));
        assert_eq!(moved, vec![linked_id, also_id]);
        assert_eq!(tasks[0].lifecycle, Lifecycle::Completed);
        assert_eq!(tasks[1].lifecycle, Lifecycle::Completed);
        assert_eq!(tasks[2].lifecycle, Lifecycle::Pending, "not this PR's item");
        assert_eq!(tasks[3].lifecycle, Lifecycle::Canceled, "left terminal");
        assert!(
            tasks[0]
                .result
                .as_ref()
                .expect("an auto-completed task carries a result")
                .summary
                .contains("automatically"),
            "an automatic tick must be distinguishable from one a human made",
        );

        // Idempotent: the same event again moves nothing.
        assert!(complete_linked_to(&mut tasks, &merged, Party::Lazybox, at(10)).is_empty());
    }

    /// A cross-repo plan projects onto an `EpicRecord` with explicit members.
    /// Deduplicated, and tracker links are NOT members: anchoring is a separate
    /// deliberate act, and an anchor re-resolves membership from a sub-issue
    /// sweep that would pull in rows the user never added.
    #[test]
    fn plan_members_are_the_workspaces_it_links_deduplicated() {
        let plan = PlanId::new();
        let mut one = task("one");
        let mut two = task("two");
        let mut tracker_only = task("an inert issue link");
        one.plan = Some(plan);
        two.plan = Some(plan);
        tracker_only.plan = Some(plan);
        one.links = vec![Link::Workspace(WorkspaceKey::new("repo-a-1"))];
        two.links = vec![
            Link::Workspace(WorkspaceKey::new("repo-b-2")),
            Link::Workspace(WorkspaceKey::new("repo-a-1")),
        ];
        tracker_only.links = vec![tracker("owner/repo#9")];
        let tasks = vec![one, two, tracker_only];
        assert_eq!(
            plan_members(&tasks, plan),
            vec![WorkspaceKey::new("repo-a-1"), WorkspaceKey::new("repo-b-2")],
        );
    }

    /// `Lazybox` requests work and never owns it.
    #[test]
    fn lazybox_can_request_but_never_own() {
        assert!(!Party::Lazybox.can_own());
        assert!(Party::Human.can_own());
        assert!(agent("w").can_own());
    }

    /// A row written before a field existed still decodes: every added field is
    /// defaulted, which is what lets the daemon roll forward without a
    /// migration on read.
    #[test]
    fn a_row_missing_every_optional_field_still_decodes() {
        let id = WorkId::new();
        let json = serde_json::json!({
            "id": id,
            "title": "a row from an older daemon",
            "requester": "Human",
            "lifecycle": "Pending",
        });
        let decoded: Task = serde_json::from_value(json).expect("older rows must decode");
        assert_eq!(decoded.id, id);
        assert_eq!(decoded.lifecycle, Lifecycle::Pending);
        assert!(decoded.links.is_empty());
        assert!(decoded.history.is_empty());
        assert_eq!(decoded.schema, 0, "an unversioned row reads as schema 0");
    }

    /// Round-trip with every field populated, so a stored task comes back
    /// identical — history and result included.
    #[test]
    fn a_fully_populated_task_round_trips() {
        let mut work = task("round trip");
        work.plan = Some(PlanId::new());
        work.parent = Some(WorkId::new());
        work.brief = "objective · done-criteria · boundaries · output".into();
        work.owner = Some(agent("w"));
        work.links = vec![
            Link::Workspace(WorkspaceKey::new("w")),
            tracker("owner/repo#1"),
            Link::Url("https://example.invalid/x".into()),
        ];
        work.transition(Lifecycle::Underway, agent("w"), at(1))
            .expect("underway");
        work.complete(
            WorkResult {
                summary: "did the thing".into(),
                artifacts: vec![ArtifactRef {
                    name: "findings.md".into(),
                    workspace: WorkspaceKey::new("w"),
                }],
            },
            agent("w"),
            at(2),
        )
        .expect("completed");
        let json = serde_json::to_string(&work).expect("serialize");
        assert_eq!(serde_json::from_str::<Task>(&json).expect("decode"), work);
    }
}
