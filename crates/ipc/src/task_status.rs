//! The read model behind "are we working on `owner/repo#151`?" (#1785).
//!
//! One question, three surfaces — the `lazybox task status` CLI, the
//! `task_status` MCP tool, and the daemon that answers both. They share these
//! types and [`TaskStatusReport::verdict`] so a user and an agent can never be
//! told different things about the same record.
//!
//! The report's whole job is to keep apart five facts that are easy to
//! conflate, and that the observed session in #1785 did conflate:
//!
//! | Fact | Source of truth |
//! |---|---|
//! | tracker lifecycle | the provider poll's cached [`lazybox_core::Task`] |
//! | assignment / claim | the `lazybox:w:` label's own expiry |
//! | session lifecycle | the persisted [`lazybox_core::WorkspaceSession`] |
//! | agent turn | the live [`crate::AgentState`] of a running PTY |
//! | review / CI | the PR's own check + review state |
//!
//! None of them implies another. A session row reading `Active` beside an
//! agent turn reading `Done` is the *normal* shape of "the worker finished a
//! turn and the task is still open", not a contradiction — and an agent turn
//! ending never means the task is complete. Every variant below exists to make
//! one of those distinctions representable instead of collapsing it into a
//! single "is it being worked on" boolean.
//!
//! Deliberately *not* modelled: a disposition inferred from what an agent
//! wrote. "Partial delivery" is a claim in prose; a declared blocker, a
//! pending check and a closed tracker record are evidence. The report carries
//! the evidence and leaves the prose to whoever reads the session.

use crate::AgentState;
use chrono::{DateTime, Utc};
use lazybox_core::{
    CiStatus, ReviewStatus, Role, SessionRunState, TaskId, TaskKind, TaskState, WorkspaceKey,
};
use serde::{Deserialize, Serialize};

/// Version of the [`TaskStatusReport`] shape, emitted as `schema_version` so a
/// consumer can refuse a payload it does not understand. Bump on any change
/// that is not purely additive.
pub const TASK_STATUS_SCHEMA_VERSION: u32 = 1;

/// Cap on the free-text evidence lines in a [`Verdict`]. The report is meant to
/// be pasteable into an agent's context, so it stays bounded no matter how many
/// workspaces or agents a record accumulates.
pub const MAX_EVIDENCE: usize = 8;

/// The answer to "what is happening on this tracker record?".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct TaskStatusReport {
    pub schema_version: u32,
    /// The record asked about, as resolved from the caller's reference.
    pub task: TaskRefInfo,
    /// When the daemon assembled this report.
    pub observed_at: DateTime<Utc>,
    /// Every workspace that holds this record under any of its task ids —
    /// normally one, more only when the fleet has genuinely split the work.
    pub workspaces: Vec<WorkspaceStatus>,
    pub verdict: Verdict,
}

/// The record a report is about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct TaskRefInfo {
    pub id: TaskId,
    /// `owner/repo` for GitHub; `None` for sources without a repo (Linear).
    pub repo: Option<String>,
    /// The trailing `#N`, when the source numbers its records.
    pub number: Option<u64>,
}

/// One workspace holding the record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct WorkspaceStatus {
    pub key: WorkspaceKey,
    pub name: String,
    /// Which of the workspace's task ids the query matched. When a query for
    /// an issue resolves through a workspace the PR has since taken over, this
    /// is the issue while [`Self::tracker`] describes the PR — the visible
    /// trace of the issue→PR fold (#151 answered by `…-187`).
    pub matched: TaskId,
    /// Lifecycle of the workspace's *headline* record, which is the PR once one
    /// exists. `None` for a workspace with no provider task at all.
    pub tracker: Option<TrackerFacts>,
    /// The matched record's own lifecycle, when it is not the headline one —
    /// so a query for an issue reports the issue's state as well as its PR's.
    pub matched_tracker: Option<TrackerFacts>,
    pub role: Option<Role>,
    /// Fleet working-claims on the matched record, live and expired kept apart.
    pub claim: ClaimFacts,
    /// A blocker the worker declared on itself via `report_blocker`.
    pub blocker: Option<BlockerFacts>,
    /// Persisted sessions (worktrees). A session outliving its agent is normal.
    pub sessions: Vec<SessionFacts>,
    /// Agent terminals the daemon is running *right now*. Empty means no live
    /// agent — never that no work happened.
    pub agents: Vec<AgentFacts>,
}

/// A tracker record's own lifecycle, as of the last provider poll.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct TrackerFacts {
    pub id: TaskId,
    pub kind: TaskKind,
    pub title: String,
    pub url: String,
    pub state: TaskState,
    pub ci: CiStatus,
    pub review: ReviewStatus,
    /// Provider `updated_at` — how fresh the cached record is. This is a
    /// *cache* timestamp, not the observation time of the report.
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

/// Working-claims (`lazybox:w:<device>:<session>:<expiry>`) on a record.
///
/// A claim is an assertion with an expiry, never proof that a process is alive:
/// the fleet renews it on a heartbeat, so a crashed worker leaves one standing
/// until it lapses. Active and expired are reported separately so a reader can
/// see a stale claim for what it is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct ClaimFacts {
    pub active: Vec<ClaimHolder>,
    /// Claims whose expiry has passed — evidence of a worker that stopped
    /// renewing, not of one still working.
    pub expired: Vec<ClaimHolder>,
    /// The bare `working` label, which carries no holder or expiry.
    pub unqualified: bool,
}

impl ClaimFacts {
    pub fn is_empty(&self) -> bool {
        self.active.is_empty() && self.expired.is_empty() && !self.unqualified
    }
}

/// One `lazybox:w:` claim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct ClaimHolder {
    /// Opaque device id from the label — enough to tell "this box" from
    /// "another box" without naming anyone.
    pub device: String,
    pub session: String,
    pub expires_at: DateTime<Utc>,
    /// Whether a live agent on *this* daemon accounts for the claim. `false`
    /// on an active claim means the holder is remote or unverifiable from here.
    pub verified_locally: bool,
}

/// A blocker the worker declared on its own workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct BlockerFacts {
    pub reason: String,
    pub kind: String,
    pub owner: String,
    pub since: DateTime<Utc>,
}

/// A persisted session (one worktree). Its state is the *session's*, which
/// says nothing about whether an agent turn is running inside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct SessionFacts {
    pub id: String,
    pub name: String,
    pub state: SessionRunState,
    pub created_at: DateTime<Utc>,
    pub last_output_at: Option<DateTime<Utc>>,
    /// Whether this daemon is running an agent terminal in this session now.
    pub has_live_agent: bool,
}

/// A live agent terminal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct AgentFacts {
    pub agent: String,
    /// `None` when the agent has not reported a state yet — unknown, which is
    /// not the same as idle.
    pub turn: Option<AgentState>,
    pub model: Option<String>,
    pub last_prompt_at: Option<DateTime<Utc>>,
    pub on_main: bool,
}

/// The compact derived answer, always carrying why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct Verdict {
    pub state: WorkState,
    /// One sentence naming the evidence the state rests on.
    pub reason: String,
    /// Bounded supporting observations, most significant first.
    pub evidence: Vec<String>,
}

/// What the daemon can honestly say about work on the record.
///
/// These describe *observation*, not task completion: the closest thing to
/// "done" here is [`WorkState::TurnEnded`], which means an agent stopped —
/// the tracker's own lifecycle is reported separately and is what says whether
/// the work actually landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum WorkState {
    /// No workspace holds this record — nobody here is on it. Note this is a
    /// statement about *this* daemon's inbox, not about the fleet.
    NoWorkspace,
    /// The record's workspace was archived (`x x`), which deletes the row. A
    /// distinct answer from [`Self::NoWorkspace`]: the work was deliberately
    /// put down here, not never picked up.
    Archived,
    /// A workspace exists but has never had an agent and holds no claim.
    NotStarted,
    /// An agent terminal is live and mid-turn.
    Working,
    /// A live agent is parked on the user: a permission prompt, a usage limit,
    /// or an exhausted credit chooser.
    AwaitingInput,
    /// A live agent is at rest — its turn ended. Says nothing about the task.
    TurnEnded,
    /// The agent process is gone (clean exit or crash) with no live successor.
    AgentExited,
    /// An unexpired claim is held by a worker this daemon cannot see — another
    /// box, or a process that died without releasing it.
    ClaimedElsewhere,
    /// Evidence disagrees or is too stale to read. Reported rather than guessed.
    Unknown,
    /// A live agent stopped because something broke — a 5xx from its gateway,
    /// a refused connection, a failed background command (#1787's
    /// [`AgentState::Stalled`]). Deliberately not folded into
    /// [`Self::TurnEnded`]: "came to rest after working" is exactly what made
    /// `Done` swallow this case, and re-merging them here would rebuild the
    /// conflation that variant exists to undo. Nor is it
    /// [`Self::AwaitingInput`] — nothing is prompting, so sending a reader to
    /// look for a question wastes the trip. The work needs a human to retry or
    /// resume; lazybox does not retry on its own.
    ///
    /// Appended last: this type crosses the socket inside `Event`, which
    /// bincode encodes by ordinal.
    Stalled,
}

impl WorkState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoWorkspace => "no_workspace",
            Self::Archived => "archived",
            Self::NotStarted => "not_started",
            Self::Working => "working",
            Self::AwaitingInput => "awaiting_input",
            Self::TurnEnded => "turn_ended",
            Self::AgentExited => "agent_exited",
            Self::ClaimedElsewhere => "claimed_elsewhere",
            Self::Unknown => "unknown",
            Self::Stalled => "stalled",
        }
    }

    /// Whether an agent turn is executing right now. Deliberately narrow:
    /// only [`Self::Working`] qualifies, because a quiet terminal, a standing
    /// claim and a retained session are each compatible with nothing running.
    pub fn is_currently_working(self) -> bool {
        matches!(self, Self::Working)
    }
}

/// Why a lookup produced no report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum TaskStatusError {
    /// The reference could not be parsed into a tracker record.
    UnresolvedReference { reference: String },
    /// The daemon could not read its own state. Explicitly not "no worker".
    Unavailable { detail: String },
}

impl std::fmt::Display for TaskStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnresolvedReference { reference } => write!(
                f,
                "cannot resolve {reference:?} to a tracker record — pass owner/repo#N, a \
                 GitHub issue/PR URL, a Linear key, or #N with --repo"
            ),
            Self::Unavailable { detail } => {
                write!(f, "daemon could not establish status: {detail}")
            }
        }
    }
}

/// Derive the compact verdict from the assembled facts.
///
/// Ordered by how much each signal proves. Live agent state is the only
/// evidence of execution, so it decides whenever it exists; a claim is
/// consulted only once no local agent can account for it, and a bare session
/// row never promotes to "working" on its own.
pub fn derive_verdict(workspaces: &[WorkspaceStatus], archived: bool) -> Verdict {
    if workspaces.is_empty() {
        return if archived {
            Verdict {
                state: WorkState::Archived,
                reason: "this record's workspace was archived in lazybox, so no work is in \
                         progress here"
                    .to_string(),
                evidence: Vec::new(),
            }
        } else {
            Verdict {
                state: WorkState::NoWorkspace,
                reason: "no workspace on this daemon holds this record".to_string(),
                evidence: Vec::new(),
            }
        };
    }

    let mut evidence = Vec::new();
    for workspace in workspaces {
        for agent in &workspace.agents {
            evidence.push(match agent.turn {
                Some(state) => format!(
                    "{}: live {} agent, turn {}",
                    workspace.key,
                    agent.agent,
                    agent_state_word(state)
                ),
                None => format!(
                    "{}: live {} agent, turn not yet reported",
                    workspace.key, agent.agent
                ),
            });
        }
        for claim in &workspace.claim.active {
            evidence.push(format!(
                "{}: claim by device {} expires {}{}",
                workspace.key,
                short_device(&claim.device),
                claim.expires_at.to_rfc3339(),
                if claim.verified_locally {
                    " (a live agent here accounts for it)"
                } else {
                    " (no live agent here accounts for it)"
                }
            ));
        }
        for claim in &workspace.claim.expired {
            evidence.push(format!(
                "{}: expired claim by device {} lapsed {}",
                workspace.key,
                short_device(&claim.device),
                claim.expires_at.to_rfc3339()
            ));
        }
        if let Some(blocker) = &workspace.blocker {
            evidence.push(format!(
                "{}: declared blocker ({}) since {} — {}",
                workspace.key,
                blocker.kind,
                blocker.since.to_rfc3339(),
                blocker.reason
            ));
        }
        if let Some(tracker) = &workspace.tracker {
            evidence.push(format!(
                "{}: {} is {}",
                workspace.key,
                tracker.id,
                tracker_state_word(tracker.state)
            ));
        }
    }
    evidence.truncate(MAX_EVIDENCE);

    let turns: Vec<Option<AgentState>> = workspaces
        .iter()
        .flat_map(|workspace| workspace.agents.iter().map(|agent| agent.turn))
        .collect();

    // A live agent is the only proof of execution, so it outranks every other
    // signal — including a claim that says otherwise.
    if turns.contains(&Some(AgentState::Working)) {
        return Verdict {
            state: WorkState::Working,
            reason: "a live agent is mid-turn".to_string(),
            evidence,
        };
    }
    // Ahead of the parked states: a stall is the signal most easily missed
    // (it renders like a finished turn), so a concurrent permission prompt
    // must not bury it.
    if turns.contains(&Some(AgentState::Stalled)) {
        return Verdict {
            state: WorkState::Stalled,
            reason: "a live agent stopped on an error (gateway failure, refused connection or \
                     a failed command); it needs a retry or resume, and lazybox does not retry \
                     on its own"
                .to_string(),
            evidence,
        };
    }
    if turns.iter().any(|turn| {
        matches!(
            turn,
            Some(
                AgentState::InputNeeded
                    | AgentState::LimitReached
                    | AgentState::CreditExhausted
                    | AgentState::AwaitingReset
            )
        )
    }) {
        return Verdict {
            state: WorkState::AwaitingInput,
            reason: "a live agent is parked waiting on the user or a usage reset".to_string(),
            evidence,
        };
    }
    // A terminal whose state never arrived proves nothing either way, and
    // silence during a long tool call is not idleness.
    if !turns.is_empty() && turns.iter().all(Option::is_none) {
        return Verdict {
            state: WorkState::Unknown,
            reason: "an agent terminal is live but has not reported a turn state".to_string(),
            evidence,
        };
    }
    if turns
        .iter()
        .any(|turn| matches!(turn, Some(AgentState::Done | AgentState::Idle)))
    {
        return Verdict {
            state: WorkState::TurnEnded,
            reason: "the latest agent turn has ended; this does not mean the task is complete"
                .to_string(),
            evidence,
        };
    }
    if turns
        .iter()
        .any(|turn| matches!(turn, Some(AgentState::Exited { .. })))
    {
        return Verdict {
            state: WorkState::AgentExited,
            reason: "the agent process has ended".to_string(),
            evidence,
        };
    }

    // No live agent anywhere. An unexpired claim now becomes the strongest
    // signal — but it is an assertion by a worker this daemon cannot observe.
    let unverified_claim = workspaces.iter().any(|workspace| {
        workspace
            .claim
            .active
            .iter()
            .any(|claim| !claim.verified_locally)
    });
    if unverified_claim {
        return Verdict {
            state: WorkState::ClaimedElsewhere,
            reason: "an unexpired working-claim is held by a worker this daemon cannot see; \
                     a claim is not proof of a running process"
                .to_string(),
            evidence,
        };
    }
    if workspaces
        .iter()
        .any(|workspace| workspace.claim.unqualified)
    {
        return Verdict {
            state: WorkState::Unknown,
            reason: "a bare `working` label carries no holder or expiry, and no live agent \
                     accounts for it"
                .to_string(),
            evidence,
        };
    }

    // An exited agent leaves no live terminal, so its evidence is the session
    // row rather than a turn state.
    let had_agent = workspaces.iter().any(|workspace| {
        workspace
            .sessions
            .iter()
            .any(|session| !matches!(session.state, SessionRunState::Stopped))
    });
    let expired_claim = workspaces
        .iter()
        .any(|workspace| !workspace.claim.expired.is_empty());
    if expired_claim {
        return Verdict {
            state: WorkState::AgentExited,
            reason: "no live agent, and the working-claim has lapsed".to_string(),
            evidence,
        };
    }
    if had_agent {
        return Verdict {
            state: WorkState::AgentExited,
            reason: "a session worktree remains but no agent is running in it".to_string(),
            evidence,
        };
    }
    Verdict {
        state: WorkState::NotStarted,
        reason: "a workspace exists but no agent has run in it".to_string(),
        evidence,
    }
}

fn agent_state_word(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "working",
        AgentState::InputNeeded => "awaiting input",
        AgentState::Idle => "idle",
        AgentState::Done => "done",
        AgentState::Exited { .. } => "exited",
        AgentState::LimitReached => "rate-limited",
        AgentState::CreditExhausted => "credit exhausted",
        AgentState::AwaitingReset => "awaiting limit reset",
        AgentState::Stalled => "stopped on an error",
    }
}

fn tracker_state_word(state: TaskState) -> &'static str {
    match state {
        TaskState::Open => "open",
        TaskState::InProgress => "in progress",
        TaskState::InReview => "in review",
        TaskState::Closed => "closed",
        TaskState::Merged => "merged",
        TaskState::Draft => "draft",
    }
}

/// Claim device ids are 20 hex chars; the leading 8 distinguish boxes without
/// filling a one-line summary.
fn short_device(device: &str) -> &str {
    device.get(..8).unwrap_or(device)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-17T12:00:00Z")
            .expect("fixture")
            .with_timezone(&Utc)
    }

    fn workspace(key: &str) -> WorkspaceStatus {
        WorkspaceStatus {
            key: WorkspaceKey::new(key),
            name: key.to_string(),
            matched: TaskId {
                source: "github".into(),
                key: "o/r#151".into(),
            },
            tracker: None,
            matched_tracker: None,
            role: None,
            claim: ClaimFacts::default(),
            blocker: None,
            sessions: Vec::new(),
            agents: Vec::new(),
        }
    }

    fn agent(turn: Option<AgentState>) -> AgentFacts {
        AgentFacts {
            agent: "claude".into(),
            turn,
            model: None,
            last_prompt_at: None,
            on_main: false,
        }
    }

    fn claim(expires_at: DateTime<Utc>, verified_locally: bool) -> ClaimHolder {
        ClaimHolder {
            device: "0123456789abcdef0123".into(),
            session: "0123456789".into(),
            expires_at,
            verified_locally,
        }
    }

    fn session(state: SessionRunState) -> SessionFacts {
        SessionFacts {
            id: "s1".into(),
            name: "claude".into(),
            state,
            created_at: now(),
            last_output_at: None,
            has_live_agent: false,
        }
    }

    #[test]
    fn no_workspace_is_distinct_from_no_worker() {
        let verdict = derive_verdict(&[], false);
        assert_eq!(verdict.state, WorkState::NoWorkspace);
        assert!(!verdict.state.is_currently_working());
    }

    #[test]
    fn an_archived_record_is_distinct_from_an_unknown_one() {
        let verdict = derive_verdict(&[], true);
        assert_eq!(verdict.state, WorkState::Archived);
        assert!(verdict.reason.contains("archived"), "{}", verdict.reason);
    }

    #[test]
    fn a_live_working_agent_is_the_only_working_verdict() {
        let mut ws = workspace("w");
        ws.agents.push(agent(Some(AgentState::Working)));
        let verdict = derive_verdict(&[ws], false);
        assert_eq!(verdict.state, WorkState::Working);
        assert!(verdict.state.is_currently_working());
    }

    /// The #1785 reproduction: an `Active` session row beside a `Done` agent
    /// turn must report that nothing is executing, and must not read as the
    /// task being finished.
    #[test]
    fn active_session_with_done_turn_reports_turn_ended_not_complete() {
        let mut ws = workspace("github-obin-ai-core-solutions-187");
        ws.sessions.push(session(SessionRunState::Active));
        ws.agents.push(agent(Some(AgentState::Done)));
        ws.tracker = Some(TrackerFacts {
            id: TaskId {
                source: "github".into(),
                key: "o/r#187".into(),
            },
            kind: TaskKind::Pr,
            title: "partial guard".into(),
            url: "https://example.invalid/187".into(),
            state: TaskState::Open,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            updated_at: now(),
            closed_at: None,
        });
        let verdict = derive_verdict(&[ws], false);
        assert_eq!(verdict.state, WorkState::TurnEnded);
        assert!(!verdict.state.is_currently_working());
        assert!(
            verdict
                .reason
                .contains("does not mean the task is complete"),
            "{}",
            verdict.reason
        );
    }

    #[test]
    fn parked_agents_report_awaiting_input() {
        for state in [
            AgentState::InputNeeded,
            AgentState::LimitReached,
            AgentState::CreditExhausted,
            AgentState::AwaitingReset,
        ] {
            let mut ws = workspace("w");
            ws.agents.push(agent(Some(state)));
            assert_eq!(
                derive_verdict(&[ws], false).state,
                WorkState::AwaitingInput,
                "{state:?}"
            );
        }
    }

    /// #1787 split "finished" from "died on a 502" in `AgentState`; the
    /// verdict must not put them back together.
    #[test]
    fn a_stalled_agent_is_neither_a_finished_turn_nor_a_prompt() {
        let mut ws = workspace("w");
        ws.agents.push(agent(Some(AgentState::Stalled)));
        let verdict = derive_verdict(&[ws], false);
        assert_eq!(verdict.state, WorkState::Stalled);
        assert!(!verdict.state.is_currently_working());
        assert!(
            verdict.reason.contains("stopped on an error"),
            "{}",
            verdict.reason
        );
    }

    /// A stall renders like a finished turn in the pane, so a concurrent
    /// permission prompt must not bury it.
    #[test]
    fn a_stall_outranks_a_concurrent_prompt() {
        let mut ws = workspace("w");
        ws.agents.push(agent(Some(AgentState::InputNeeded)));
        ws.agents.push(agent(Some(AgentState::Stalled)));
        assert_eq!(derive_verdict(&[ws], false).state, WorkState::Stalled);
    }

    /// Execution evidence still wins: a second agent genuinely working means
    /// work *is* happening on the record.
    #[test]
    fn a_working_agent_outranks_a_stalled_sibling() {
        let mut ws = workspace("w");
        ws.agents.push(agent(Some(AgentState::Stalled)));
        ws.agents.push(agent(Some(AgentState::Working)));
        assert_eq!(derive_verdict(&[ws], false).state, WorkState::Working);
    }

    #[test]
    fn a_silent_terminal_with_no_reported_turn_is_unknown_not_idle() {
        let mut ws = workspace("w");
        ws.agents.push(agent(None));
        let verdict = derive_verdict(&[ws], false);
        assert_eq!(verdict.state, WorkState::Unknown);
        assert!(!verdict.state.is_currently_working());
    }

    #[test]
    fn a_live_agent_outranks_a_claim_that_says_otherwise() {
        let mut ws = workspace("w");
        ws.agents.push(agent(Some(AgentState::Working)));
        ws.claim.expired.push(claim(
            now() - chrono::Duration::hours(2),
            /* verified_locally */ false,
        ));
        let verdict = derive_verdict(&[ws], false);
        assert_eq!(
            verdict.state,
            WorkState::Working,
            "a lapsed claim must not override fresh execution evidence"
        );
    }

    #[test]
    fn an_unverified_active_claim_is_claimed_elsewhere_not_working() {
        let mut ws = workspace("w");
        ws.claim
            .active
            .push(claim(now() + chrono::Duration::minutes(30), false));
        let verdict = derive_verdict(&[ws], false);
        assert_eq!(verdict.state, WorkState::ClaimedElsewhere);
        assert!(!verdict.state.is_currently_working());
        assert!(verdict.reason.contains("not proof"), "{}", verdict.reason);
    }

    #[test]
    fn an_expired_claim_with_no_agent_reads_as_exited() {
        let mut ws = workspace("w");
        ws.claim
            .expired
            .push(claim(now() - chrono::Duration::hours(1), false));
        assert_eq!(derive_verdict(&[ws], false).state, WorkState::AgentExited);
    }

    #[test]
    fn a_bare_working_label_alone_is_unknown() {
        let mut ws = workspace("w");
        ws.claim.unqualified = true;
        let verdict = derive_verdict(&[ws], false);
        assert_eq!(verdict.state, WorkState::Unknown);
    }

    #[test]
    fn a_stopped_session_with_nothing_else_is_not_started() {
        let mut ws = workspace("w");
        ws.sessions.push(session(SessionRunState::Stopped));
        assert_eq!(derive_verdict(&[ws], false).state, WorkState::NotStarted);
    }

    #[test]
    fn a_retained_session_without_an_agent_reads_as_exited() {
        let mut ws = workspace("w");
        ws.sessions.push(session(SessionRunState::Active));
        assert_eq!(derive_verdict(&[ws], false).state, WorkState::AgentExited);
    }

    #[test]
    fn a_declared_blocker_alone_does_not_claim_the_worker_is_still_blocked() {
        let mut ws = workspace("w");
        ws.blocker = Some(BlockerFacts {
            reason: "waiting on legal".into(),
            kind: "decision".into(),
            owner: "operator".into(),
            since: now() - chrono::Duration::days(3),
        });
        ws.agents.push(agent(Some(AgentState::Working)));
        let verdict = derive_verdict(&[ws], false);
        assert_eq!(
            verdict.state,
            WorkState::Working,
            "a saved blocker must not override fresh execution evidence"
        );
        assert!(
            verdict.evidence.iter().any(|e| e.contains("legal")),
            "the blocker is still reported as evidence: {:?}",
            verdict.evidence
        );
    }

    #[test]
    fn evidence_is_bounded() {
        let mut ws = workspace("w");
        for _ in 0..40 {
            ws.agents.push(agent(Some(AgentState::Done)));
        }
        let verdict = derive_verdict(&[ws], false);
        assert!(verdict.evidence.len() <= MAX_EVIDENCE);
    }

    #[test]
    fn work_state_words_are_stable() {
        assert_eq!(WorkState::NoWorkspace.as_str(), "no_workspace");
        assert_eq!(WorkState::TurnEnded.as_str(), "turn_ended");
        assert_eq!(WorkState::ClaimedElsewhere.as_str(), "claimed_elsewhere");
    }

    #[test]
    fn unresolved_reference_error_names_the_accepted_shapes() {
        let err = TaskStatusError::UnresolvedReference {
            reference: "nonsense".into(),
        };
        let text = err.to_string();
        assert!(text.contains("owner/repo#N"), "{text}");
        assert!(text.contains("--repo"), "{text}");
    }
}
