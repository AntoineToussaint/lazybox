//! The read/write model behind the task/plan store, for every surface.
//!
//! One store, three callers — the `create_work` / `my_work` / `update_work` /
//! `work_status` MCP tools, the `lazybox work …` CLI that is their twin for
//! agents with no MCP at all (Codex, Cursor, `--strict-mcp-config`), and the
//! daemon that answers both. They share these types for the reason
//! [`crate::task_status`] does: a user at a shell and an agent in a session
//! must not be told different things about the same row, and two hand-rolled
//! renderings of one record diverge the first time either is edited.
//!
//! These are *views*, deliberately not `lazybox_core::work::Task` on the wire.
//! A stored task carries its whole provenance history, which grows without
//! bound and is the one field a caller almost never wants: [`WorkRow`] carries
//! the history's length and its latest entry instead, so reading a long-lived
//! plan costs a reader a bounded number of bytes rather than every transition
//! it ever made. The lifecycle is rendered as its label plus an optional
//! `detail` for the same reason the model keeps them apart — a question, a
//! blocker and a failure reason are the same shape to a renderer and different
//! things to a reader.

use crate::SessionKey;
use serde::{Deserialize, Serialize};

/// What a caller asks of the work store.
///
/// Every variant that writes carries the party doing it, because provenance is
/// the point of the history: "who moved this" has to come from the channel, not
/// from a field the caller fills in about itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum WorkRequest {
    /// What `workspace` owns, is owed, and has queued.
    Mine {
        workspace: SessionKey,
        include_done: bool,
    },
    /// Mint a unit of work. `owner` assigns it; `deliver` hands the brief over
    /// in the same breath.
    Create {
        requester: SessionKey,
        title: String,
        brief: String,
        owner: Option<SessionKey>,
        deliver: bool,
        plan: Option<String>,
        parent: Option<String>,
        links: Vec<String>,
    },
    /// Move one unit of work, carrying its result when it completes.
    Update {
        by: SessionKey,
        id: String,
        lifecycle: String,
        detail: Option<String>,
        summary: Option<String>,
        artifacts: Vec<String>,
    },
    /// A plan's roll-up, or every plan when `plan` is `None`.
    Status { plan: Option<String> },
}

/// One unit of work, as a caller reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct WorkRow {
    pub id: String,
    pub title: String,
    pub brief: String,
    /// The lifecycle's label — `pending`, `underway`, `awaiting answer`,
    /// `held`, `completed`, `failed`, `canceled`.
    pub lifecycle: String,
    /// The question for `awaiting answer`, the blocker for `held`, the cause
    /// for `failed`. One field because they are one shape to a renderer.
    pub detail: Option<String>,
    /// The owner's workspace, or `human` / `lazybox`. `None` is unassigned,
    /// which is the absence of an owner rather than a party of its own.
    pub owner: Option<String>,
    pub requester: Option<String>,
    pub plan: Option<String>,
    pub parent: Option<String>,
    /// Links in the form a caller may pass straight back — the `ws:` prefix
    /// for a workspace, `<source>:<key>` for a tracker record, a URL — and the
    /// round trip is a tested property, not an aspiration: a rendering that
    /// did not parse back silently produced a *different* link, and the merge
    /// auto-check matches a link exactly, so the row it was on was never
    /// ticked off. The prefix is kept because a workspace key and its tracker
    /// record are the same string and mean different things.
    pub links: Vec<String>,
    pub result: Option<WorkResultView>,
    /// How many transitions this row has recorded. The history itself is not
    /// on the wire: it grows without bound.
    pub events: usize,
    pub last_event: Option<WorkEventView>,
}

/// A finished unit of work's result. Artifacts are names, by reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct WorkResultView {
    pub summary: String,
    pub artifacts: Vec<String>,
}

/// The latest provenance entry — who changed what, when.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct WorkEventView {
    /// RFC 3339.
    pub at: String,
    pub by: Option<String>,
    pub change: String,
}

/// A plan with its roll-up and the workspaces it spans.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct PlanRow {
    pub plan: String,
    pub title: String,
    pub done: usize,
    pub total: usize,
    /// The workspaces this plan's tasks point at — the member list a
    /// cross-repo plan projects onto an epic. Ordered by the store's keys, so
    /// stable across reads but not creation order.
    pub members: Vec<String>,
    pub tasks: Vec<WorkRow>,
}

impl PlanRow {
    /// Whether every counted task is done. `total == 0` is not complete: an
    /// empty plan is unstarted, and calling it finished would tick a plan
    /// nobody has filled in yet.
    pub fn is_complete(&self) -> bool {
        self.total > 0 && self.done == self.total
    }
}

/// What the daemon reports back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum WorkReport {
    /// Three lists, because "I requested it" splits three ways and conflating
    /// them misreports who is on the hook: work I own, work a sibling owes me,
    /// and work I filed that nobody owns yet.
    Mine {
        workspace: String,
        mine: Vec<WorkRow>,
        waiting_on_others: Vec<WorkRow>,
        unassigned: Vec<WorkRow>,
    },
    /// One row, after a create or an update, with the delivery receipt when
    /// the call tried to hand it over.
    One {
        work: Box<WorkRow>,
        delivery: Option<WorkDelivery>,
        /// Where a terminal state's notice went, if one was sent.
        requester_notified: Option<WorkDelivery>,
    },
    /// Plans and the open work on none of them.
    Status {
        plans: Vec<PlanRow>,
        unplanned: Vec<WorkRow>,
        /// Rows this build could not decode. Surfaced rather than hidden: a
        /// listing that silently drops rows reads as a shorter plan.
        undecodable_rows: usize,
    },
}

/// What became of a brief or a notice put in front of an agent. The same three
/// outcomes the one delivery owner reports, so a caller can tell "the agent
/// started reading it" from "it is behind a turn" from "it never landed".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum WorkDelivery {
    /// It is in the target's input, delivered between turns.
    Delivered { workspace: String },
    /// The target is mid-turn; it lands when that turn ends.
    Queued { workspace: String },
    /// It never landed, and why. The work is still recorded and assigned —
    /// a refused delivery is not a refused assignment.
    Refused { reason: String },
    /// Nothing was sent, and why (the owner is the caller, `deliver=false`, a
    /// non-terminal state). Distinct from `Refused`: nobody tried.
    Skipped { reason: String },
}

/// Why a work call could not be answered.
///
/// `BadRequest` is the caller's to fix and `Unavailable` is the daemon's, which
/// is the distinction a script needs: the first will fail the same way on a
/// retry and the second may not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum WorkError {
    #[error("{0}")]
    BadRequest(String),
    #[error("the work store could not be read: {0}")]
    Unavailable(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_plan_is_unstarted_not_complete() {
        let empty = PlanRow {
            plan: "p".into(),
            title: "nothing yet".into(),
            done: 0,
            total: 0,
            members: Vec::new(),
            tasks: Vec::new(),
        };
        assert!(
            !empty.is_complete(),
            "a plan nobody has filled in must not read as finished"
        );
    }

    #[test]
    fn a_plan_is_complete_only_when_every_counted_task_is_done() {
        let mut plan = PlanRow {
            plan: "p".into(),
            title: "t".into(),
            done: 1,
            total: 2,
            members: Vec::new(),
            tasks: Vec::new(),
        };
        assert!(!plan.is_complete());
        plan.done = 2;
        assert!(plan.is_complete());
    }

    #[test]
    fn the_two_errors_read_differently_because_a_retry_differs() {
        assert_eq!(
            WorkError::BadRequest("no such lifecycle".into()).to_string(),
            "no such lifecycle"
        );
        assert!(
            WorkError::Unavailable("disk".into())
                .to_string()
                .contains("could not be read"),
            "a daemon-side failure must not read as the caller's mistake"
        );
    }

    #[test]
    fn a_refused_delivery_round_trips_with_its_reason() {
        let refused = WorkDelivery::Refused {
            reason: "no running agent".into(),
        };
        let json = serde_json::to_string(&refused).expect("encode");
        assert_eq!(
            serde_json::from_str::<WorkDelivery>(&json).expect("decode"),
            refused
        );
    }
}
