//! The four work verbs, once, for both surfaces.
//!
//! `create_work` / `my_work` / `update_work` / `work_status` reach agents two
//! ways — as MCP tools (`mcp.rs`) and as `lazybox work …` for agents with no
//! MCP at all (`tui-boot`, over [`lazybox_ipc::Command::WorkCall`]). This
//! module is the one implementation underneath, for the reason
//! `task_status.rs` already carries: two renderings of one record diverge the
//! first time either is edited, and an agent and the user at a shell must not
//! be told different things about the same row.
//!
//! The caller supplies the acting [`Party`] and gets back a
//! [`WorkReport`] — the shaping is here, not at either surface, so a field
//! added to a row appears in both without a second edit.

use lazybox_core::work::{
    ArtifactRef, Lifecycle, Link, Party, Plan, PlanId, Task, WorkId, WorkResult,
};
use lazybox_core::{SessionKey, WorkspaceKey};
use lazybox_ipc::work::{
    PlanRow, WorkDelivery, WorkError, WorkEventView, WorkReport, WorkRequest, WorkResultView,
    WorkRow,
};

use crate::ServerConfig;

/// Longest `brief` a create may carry. A brief is the four lines the design
/// doc names — objective, done-criteria, boundaries, output shape — not a
/// pasted transcript, and it is delivered into another agent's context.
pub const MAX_BRIEF_BYTES: usize = 4000;

/// How long to wait for a delivery to land before reporting it as queued.
/// Long enough that an idle agent reads as delivered, short enough that a
/// busy one does not hold the caller's turn.
const DELIVERY_RECEIPT_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);
/// How long the delivery owner may wait out a chooser or a turn.
const DELIVERY_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Answer one work call.
pub async fn call(config: &ServerConfig, request: WorkRequest) -> Result<WorkReport, WorkError> {
    match request {
        WorkRequest::Mine {
            workspace,
            include_done,
        } => mine(config, &workspace, include_done).await,
        WorkRequest::Create {
            requester,
            title,
            brief,
            owner,
            deliver,
            plan,
            parent,
            links,
        } => {
            create(
                config,
                CreateArgs {
                    requester,
                    title,
                    brief,
                    owner,
                    deliver,
                    plan,
                    parent,
                    links,
                },
            )
            .await
        }
        WorkRequest::Update {
            by,
            id,
            lifecycle,
            detail,
            summary,
            artifacts,
        } => {
            update(
                config,
                UpdateArgs {
                    by,
                    id,
                    lifecycle,
                    detail,
                    summary,
                    artifacts,
                },
            )
            .await
        }
        WorkRequest::Status { plan } => status(config, plan.as_deref()).await,
    }
}

pub struct CreateArgs {
    pub requester: SessionKey,
    pub title: String,
    pub brief: String,
    pub owner: Option<SessionKey>,
    pub deliver: bool,
    pub plan: Option<String>,
    pub parent: Option<String>,
    pub links: Vec<String>,
}

pub struct UpdateArgs {
    pub by: SessionKey,
    pub id: String,
    pub lifecycle: String,
    pub detail: Option<String>,
    pub summary: Option<String>,
    pub artifacts: Vec<String>,
}

async fn mine(
    config: &ServerConfig,
    workspace: &SessionKey,
    include_done: bool,
) -> Result<WorkReport, WorkError> {
    let key = WorkspaceKey::new(workspace.as_str());
    let lookup = key.clone();
    let (owned, requested) = crate::store_blocking(&config.store, move |store| {
        let owned = crate::work_store::tasks_owned_by(store, &lookup)?;
        let requested = crate::work_store::tasks_requested_by(store, &lookup)?;
        Ok::<_, lazybox_store::StoreError>((owned, requested))
    })
    .await
    .map_err(unavailable)?;

    // Three lists: "I requested it" splits three ways and conflating them
    // misreports who is on the hook. Work I also own is mine (it would
    // otherwise appear twice); work nobody owns is my own backlog; only the
    // rest is a sibling's to report.
    let mut waiting = Vec::new();
    let mut unassigned = Vec::new();
    for task in requested {
        match crate::work_store::owner_workspace(&task) {
            Some(owner) if owner == &key => {}
            Some(_) => waiting.push(task),
            None => unassigned.push(task),
        }
    }
    let rows = |tasks: Vec<Task>| -> Vec<WorkRow> {
        tasks
            .iter()
            .filter(|task| include_done || !task.lifecycle.is_terminal())
            .map(row)
            .collect()
    };
    Ok(WorkReport::Mine {
        workspace: workspace.as_str().to_string(),
        mine: rows(owned),
        waiting_on_others: rows(waiting),
        unassigned: rows(unassigned),
    })
}

async fn create(config: &ServerConfig, args: CreateArgs) -> Result<WorkReport, WorkError> {
    let title = args.title.trim();
    if title.is_empty() {
        return Err(WorkError::BadRequest("work needs a title".into()));
    }
    if args.brief.len() > MAX_BRIEF_BYTES {
        return Err(WorkError::BadRequest(format!(
            "brief exceeds {MAX_BRIEF_BYTES} bytes — a brief is objective, done-criteria, \
             boundaries and output shape, not a transcript"
        )));
    }

    let mut task = Task::new(title, party(&args.requester), chrono::Utc::now());
    task.brief = args.brief.trim().to_string();

    if let Some(raw) = &args.plan {
        let id = parse_plan_id(raw)?;
        let exists = crate::store_blocking(&config.store, move |store| {
            crate::work_store::load_plan(store, id)
        })
        .await
        .map_err(unavailable)?
        .is_some();
        if !exists {
            return Err(WorkError::BadRequest(format!(
                "no plan {raw} — call work_status to list the plans that exist"
            )));
        }
        task.plan = Some(id);
    }

    if let Some(raw) = &args.parent {
        let id = parse_work_id(raw)?;
        let found = crate::store_blocking(&config.store, move |store| {
            crate::work_store::load_task(store, id)
        })
        .await
        .map_err(unavailable)?;
        let Some(found) = found else {
            return Err(WorkError::BadRequest(format!(
                "no work {raw} to nest under"
            )));
        };
        task.parent = Some(id);
        // A sub-task inherits its parent's plan unless one was named: a child
        // on no plan would not count toward the roll-up its parent shows.
        if task.plan.is_none() {
            task.plan = found.plan;
        }
    }

    for raw in &args.links {
        let link = parse_link(raw)?;
        if !task.links.contains(&link) {
            task.links.push(link);
        }
    }

    if let Some(owner) = &args.owner {
        task.owner = Some(party(owner));
        // The owner's workspace is also a link, so a cross-repo plan has its
        // member list without the caller restating it.
        let link = Link::Workspace(WorkspaceKey::new(owner.as_str()));
        if !task.links.contains(&link) {
            task.links.push(link);
        }
    }

    let stored = task.clone();
    crate::store_blocking(&config.store, move |store| {
        crate::work_store::save_task(store, &stored)
    })
    .await
    .map_err(unavailable)?;

    tracing::info!(
        from = %args.requester.as_str(),
        work = %task.id,
        owner = args.owner.as_ref().map(|k| k.as_str()).unwrap_or("-"),
        "work: minted a unit of work"
    );

    let delivery = match (&args.owner, args.deliver) {
        (Some(owner), _) if owner == &args.requester => Some(WorkDelivery::Skipped {
            reason: "the owner is the caller".into(),
        }),
        (Some(owner), true) => Some(deliver_brief(config, &args.requester, owner, &task).await),
        (Some(_), false) => Some(WorkDelivery::Skipped {
            reason: "deliver=false".into(),
        }),
        (None, true) => {
            return Err(WorkError::BadRequest(
                "deliver=true needs an `owner` to deliver to".into(),
            ));
        }
        (None, false) => None,
    };

    Ok(WorkReport::One {
        work: Box::new(row(&task)),
        delivery,
        requester_notified: None,
    })
}

async fn update(config: &ServerConfig, args: UpdateArgs) -> Result<WorkReport, WorkError> {
    let id = parse_work_id(&args.id)?;
    let mut task = crate::store_blocking(&config.store, move |store| {
        crate::work_store::load_task(store, id)
    })
    .await
    .map_err(unavailable)?
    .ok_or_else(|| WorkError::BadRequest(format!("no work {id}")))?;

    let by = party(&args.by);
    let now = chrono::Utc::now();
    let moved = if is_completion(&args.lifecycle) {
        let summary = args
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                WorkError::BadRequest(
                    "a completion needs a `summary` — it is what the requester reads instead of \
                     your scrollback"
                        .into(),
                )
            })?;
        let workspace = WorkspaceKey::new(args.by.as_str());
        task.complete(
            WorkResult {
                summary: summary.to_string(),
                artifacts: args
                    .artifacts
                    .iter()
                    .map(|name| ArtifactRef {
                        name: name.trim().to_string(),
                        workspace: workspace.clone(),
                    })
                    .collect(),
            },
            by,
            now,
        )
    } else {
        task.transition(
            parse_lifecycle(&args.lifecycle, args.detail.as_deref())?,
            by,
            now,
        )
    };
    // A terminal task refusing a late report is the model's rule; reporting it
    // is how the caller learns its result landed nowhere.
    moved.map_err(|refused| WorkError::BadRequest(refused.to_string()))?;

    let stored = task.clone();
    crate::store_blocking(&config.store, move |store| {
        crate::work_store::save_task(store, &stored)
    })
    .await
    .map_err(unavailable)?;

    tracing::info!(
        from = %args.by.as_str(),
        work = %task.id,
        lifecycle = task.lifecycle.label(),
        "work: work moved"
    );

    let requester_notified = notify_requester(config, &task, &args.by).await;
    Ok(WorkReport::One {
        work: Box::new(row(&task)),
        delivery: None,
        requester_notified: Some(requester_notified),
    })
}

async fn status(config: &ServerConfig, plan: Option<&str>) -> Result<WorkReport, WorkError> {
    let wanted = match plan {
        Some(raw) => Some(parse_plan_id(raw)?),
        None => None,
    };
    let (plans, tasks, skipped) = crate::store_blocking(&config.store, move |store| {
        let plans = crate::work_store::all_plans(store)?;
        let (tasks, skipped) = crate::work_store::all_tasks(store)?;
        Ok::<_, lazybox_store::StoreError>((plans, tasks, skipped))
    })
    .await
    .map_err(unavailable)?;

    let rows: Vec<PlanRow> = plans
        .iter()
        .filter(|plan: &&Plan| wanted.is_none_or(|id| plan.id == id))
        .map(|plan| {
            let progress = lazybox_core::work::plan_progress(&tasks, plan.id);
            PlanRow {
                plan: plan.id.to_string(),
                title: plan.title.clone(),
                done: progress.done,
                total: progress.total,
                members: lazybox_core::work::plan_members(&tasks, plan.id)
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                tasks: tasks
                    .iter()
                    .filter(|task| task.plan == Some(plan.id))
                    .map(row)
                    .collect(),
            }
        })
        .collect();

    if let (Some(id), true) = (wanted, rows.is_empty()) {
        return Err(WorkError::BadRequest(format!("no plan {id}")));
    }
    Ok(WorkReport::Status {
        plans: rows,
        unplanned: tasks
            .iter()
            .filter(|task| task.plan.is_none() && !task.lifecycle.is_terminal())
            .map(row)
            .collect(),
        undecodable_rows: skipped,
    })
}

/// Put a work brief in front of its owner, through `crate::delivery` — the
/// same gate, dedupe and receipt every other path uses.
async fn deliver_brief(
    config: &ServerConfig,
    from: &SessionKey,
    owner: &SessionKey,
    task: &Task,
) -> WorkDelivery {
    let Some(terminal_id) = config.terminal.running_agent_terminal(owner).await else {
        return WorkDelivery::Refused {
            reason: format!(
                "no running agent in workspace {} — the work is recorded and assigned, and is in \
                 its owner's my_work",
                owner.as_str()
            ),
        };
    };
    let body = format!(
        "<lazybox-work id=\"{id}\" from=\"{from}\">\nYou own this unit of work. Report with \
         `update_work(id=\"{id}\", lifecycle=\"completed\", summary=…)` when it is done, or \
         lifecycle `awaiting-answer` / `held` with a `detail` if you cannot proceed — the \
         requester reads your result, not your scrollback.\n\n{title}\n\n{brief}\n</lazybox-work>",
        id = task.id,
        from = from.as_str(),
        title = task.title,
        brief = task.brief,
    );
    send(
        config,
        terminal_id,
        body,
        crate::delivery::Party::Agent(from.clone()),
        owner,
    )
    .await
}

/// Tell the requester its work reached a terminal state. Best-effort: a
/// requester that is not running still has the result on the row, which is
/// why it is stored rather than only announced.
async fn notify_requester(config: &ServerConfig, task: &Task, by: &SessionKey) -> WorkDelivery {
    if !task.lifecycle.is_terminal() {
        return WorkDelivery::Skipped {
            reason: "not a terminal state".into(),
        };
    }
    let Party::Agent { workspace, .. } = &task.requester else {
        return WorkDelivery::Skipped {
            reason: "the requester is not an agent".into(),
        };
    };
    // Never report a result back to whoever just reported it. The guard is on
    // the acting party, not on ownership: work a session filed for itself and
    // finished itself is usually *unassigned*, so an owner-based check misses
    // it and pastes an agent's own summary back into its own session.
    if workspace.as_str() == by.as_str() {
        return WorkDelivery::Skipped {
            reason: "the requester reported it".into(),
        };
    }
    let target = SessionKey::from(workspace.as_str());
    let Some(terminal_id) = config.terminal.running_agent_terminal(&target).await else {
        return WorkDelivery::Skipped {
            reason: "the requester has no running agent".into(),
        };
    };
    let detail = match &task.lifecycle {
        Lifecycle::Failed { reason } => format!("\nreason: {reason}"),
        _ => String::new(),
    };
    let result = task.result.as_ref();
    let summary = result.map(|r| r.summary.as_str()).unwrap_or_default();
    let artifacts = result
        .map(|r| {
            r.artifacts
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let body = format!(
        "<lazybox-work-result id=\"{id}\" lifecycle=\"{state}\">\nWork you requested reached a \
         terminal state. Read it with `my_work`; nothing is waiting on you unless this changes \
         your plan.\n\n{title}{detail}\n\n{summary}{artifacts}\n</lazybox-work-result>",
        id = task.id,
        state = task.lifecycle.label(),
        title = task.title,
        artifacts = if artifacts.is_empty() {
            String::new()
        } else {
            format!("\n\nartifacts: {artifacts}")
        },
    );
    send(
        config,
        terminal_id,
        body,
        crate::delivery::Party::Lazybox("work result"),
        &target,
    )
    .await
}

async fn send(
    config: &ServerConfig,
    terminal_id: lazybox_ipc::TerminalId,
    body: String,
    from: crate::delivery::Party,
    to: &SessionKey,
) -> WorkDelivery {
    let mut pending = crate::delivery::deliver(
        config,
        crate::delivery::DeliveryRequest {
            terminal_id,
            body,
            submit: true,
            // Idle-gated: a message from another party lands between turns.
            // Pasting into a working agent interleaves it with a turn about
            // something else, and that turn's end then looks like an answer.
            gate: crate::delivery::Gate::Idle,
            from,
            wait_limit: Some(DELIVERY_WAIT_LIMIT),
        },
    )
    .await;
    let early = pending.landed_within(DELIVERY_RECEIPT_WAIT).await;
    {
        // Keep the handle alive so the final outcome is still logged after
        // this call has already reported `queued`.
        let to = to.as_str().to_string();
        tokio::spawn(async move {
            let outcome = pending.receipt().await;
            tracing::info!(to = %to, ?outcome, "work: delivery resolved");
        });
    }
    match early {
        Some(crate::delivery::EarlyOutcome::Landed) => WorkDelivery::Delivered {
            workspace: to.as_str().to_string(),
        },
        Some(crate::delivery::EarlyOutcome::Refused { reason }) => WorkDelivery::Refused { reason },
        None => WorkDelivery::Queued {
            workspace: to.as_str().to_string(),
        },
    }
}

/// One stored task as a wire row. The history is reduced to its length and
/// latest entry on purpose: it grows without bound and is the one field a
/// caller almost never wants.
fn row(task: &Task) -> WorkRow {
    WorkRow {
        id: task.id.to_string(),
        title: task.title.clone(),
        brief: task.brief.clone(),
        lifecycle: task.lifecycle.label().to_string(),
        detail: match &task.lifecycle {
            Lifecycle::AwaitingAnswer { question } => Some(question.clone()),
            Lifecycle::Held { reason } | Lifecycle::Failed { reason } => Some(reason.clone()),
            _ => None,
        },
        owner: task.owner.as_ref().and_then(party_label),
        requester: party_label(&task.requester),
        plan: task.plan.map(|id| id.to_string()),
        parent: task.parent.map(|id| id.to_string()),
        links: task.links.iter().map(link_label).collect(),
        result: task.result.as_ref().map(|result| WorkResultView {
            summary: result.summary.clone(),
            artifacts: result.artifacts.iter().map(|a| a.name.clone()).collect(),
        }),
        events: task.history.len(),
        last_event: task.history.last().map(|event| WorkEventView {
            at: event.at.to_rfc3339(),
            by: party_label(&event.by),
            change: event.change.clone(),
        }),
    }
}

/// How a party renders. An agent is its *workspace* — the address work is
/// actually sent to — and the session id beside it is provenance, so it is
/// deliberately not shown as identity.
fn party_label(party: &Party) -> Option<String> {
    match party {
        Party::Human => Some("human".to_string()),
        Party::Lazybox => Some("lazybox".to_string()),
        Party::Agent { workspace, .. } => Some(workspace.to_string()),
    }
}

/// How a link renders. The `ws:` prefix is kept on the way out because it is
/// required on the way in.
fn link_label(link: &Link) -> String {
    match link {
        Link::Workspace(key) => format!("ws:{key}"),
        Link::Tracker(id) => id.to_string(),
        Link::Url(url) => url.clone(),
    }
}

fn party(key: &SessionKey) -> Party {
    Party::Agent {
        workspace: WorkspaceKey::new(key.as_str()),
        // Provenance only, and left unset here: ownership is addressed by
        // workspace because a session id is replaced on every respawn.
        session: None,
    }
}

/// Whether `name` asks for a completion. Checked separately from
/// [`parse_lifecycle`] because a completion needs a summary the parser has no
/// access to.
pub fn is_completion(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "completed" | "complete" | "done"
    )
}

/// Parse a requested lifecycle. Rejects `completed`, which goes through
/// [`is_completion`] and carries a result.
pub fn parse_lifecycle(name: &str, detail: Option<&str>) -> Result<Lifecycle, WorkError> {
    let detail = detail.unwrap_or("").trim();
    match name.trim().to_ascii_lowercase().as_str() {
        "pending" => Ok(Lifecycle::Pending),
        "underway" | "working" | "in-progress" => Ok(Lifecycle::Underway),
        "awaiting-answer" | "awaiting_answer" | "input-required" => {
            if detail.is_empty() {
                return Err(WorkError::BadRequest(
                    "awaiting-answer needs `detail`: the question you are waiting on".into(),
                ));
            }
            Ok(Lifecycle::AwaitingAnswer {
                question: detail.to_string(),
            })
        }
        "held" | "blocked" => {
            if detail.is_empty() {
                return Err(WorkError::BadRequest(
                    "held needs `detail`: what is blocking it".into(),
                ));
            }
            Ok(Lifecycle::Held {
                reason: detail.to_string(),
            })
        }
        "failed" => Ok(Lifecycle::Failed {
            reason: if detail.is_empty() {
                "no reason given".to_string()
            } else {
                detail.to_string()
            },
        }),
        "canceled" | "cancelled" => Ok(Lifecycle::Canceled),
        "completed" | "complete" | "done" => Err(WorkError::BadRequest(
            "a completion needs a `summary` — pass one with lifecycle=\"completed\"".into(),
        )),
        other => Err(WorkError::BadRequest(format!(
            "unknown lifecycle {other:?} — one of: underway, awaiting-answer, held, completed, \
             failed, canceled"
        ))),
    }
}

/// Parse one link. The accepted forms are explicit because a workspace key
/// and its tracker record render identically (`github:owner/repo#7` is both)
/// and mean different things: `plan_members` reads workspace links, the
/// auto-check on merge reads tracker links.
pub fn parse_link(raw: &str) -> Result<Link, WorkError> {
    let raw = raw.trim();
    if let Some(key) = raw.strip_prefix("ws:") {
        if key.is_empty() {
            return Err(WorkError::BadRequest(
                "`ws:` with no workspace key after it".into(),
            ));
        }
        return Ok(Link::Workspace(WorkspaceKey::new(key)));
    }
    if let Some(id) = lazybox_core::task_ref::parse_task_ref(raw, None) {
        return Ok(Link::Tracker(id));
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Ok(Link::Url(raw.to_string()));
    }
    Err(WorkError::BadRequest(format!(
        "cannot read {raw:?} as a link — use `owner/repo#N` or an issue URL for a tracker \
         record, `ws:<workspace key>` for a workspace, or an http(s) URL"
    )))
}

fn parse_work_id(raw: &str) -> Result<WorkId, WorkError> {
    raw.trim()
        .parse()
        .map_err(|_| WorkError::BadRequest(format!("{raw:?} is not a work id")))
}

fn parse_plan_id(raw: &str) -> Result<PlanId, WorkError> {
    raw.trim()
        .parse::<uuid::Uuid>()
        .map(PlanId)
        .map_err(|_| WorkError::BadRequest(format!("{raw:?} is not a plan id")))
}

fn unavailable(error: lazybox_store::StoreError) -> WorkError {
    WorkError::Unavailable(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_ipc::work::WorkReport;

    fn key(raw: &str) -> SessionKey {
        SessionKey::from(raw)
    }

    fn create(requester: &str, title: &str) -> CreateArgs {
        CreateArgs {
            requester: key(requester),
            title: title.into(),
            brief: String::new(),
            owner: None,
            deliver: false,
            plan: None,
            parent: None,
            links: Vec::new(),
        }
    }

    fn one(report: WorkReport) -> (WorkRow, Option<WorkDelivery>, Option<WorkDelivery>) {
        match report {
            WorkReport::One {
                work,
                delivery,
                requester_notified,
            } => (*work, delivery, requester_notified),
            other => panic!("expected one row, got {other:?}"),
        }
    }

    fn mine_lists(report: WorkReport) -> (Vec<WorkRow>, Vec<WorkRow>, Vec<WorkRow>) {
        match report {
            WorkReport::Mine {
                mine,
                waiting_on_others,
                unassigned,
                ..
            } => (mine, waiting_on_others, unassigned),
            other => panic!("expected the three lists, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_minted_row_reads_back_pending_and_unassigned() {
        let config = ServerConfig::in_memory();
        let mut args = create("github:acme/widget#1", "wire the store");
        args.brief = "objective · done · bounds".into();

        let (work, delivery, _) = one(super::create(&config, args).await.expect("created"));
        assert_eq!(work.lifecycle, "pending");
        assert_eq!(work.requester.as_deref(), Some("github:acme/widget#1"));
        assert_eq!(work.owner, None, "unassigned is the absence of an owner");
        assert_eq!(delivery, None, "nothing to deliver without an owner");
        assert_eq!(work.events, 1, "the create is its first provenance entry");
    }

    #[tokio::test]
    async fn assigning_adds_the_owner_as_a_link_so_a_plan_has_its_members() {
        let config = ServerConfig::in_memory();
        let mut args = create("github:acme/widget#1", "do this");
        args.owner = Some(key("github:acme/other#2"));

        let (work, _, _) = one(super::create(&config, args).await.expect("created"));
        assert_eq!(work.owner.as_deref(), Some("github:acme/other#2"));
        assert_eq!(work.links, vec!["ws:github:acme/other#2".to_string()]);
    }

    #[tokio::test]
    async fn a_handoff_to_a_dead_workspace_still_records_the_assignment() {
        // The receipt must separate "nobody was there to read it" from "it
        // never happened": the work is assigned either way.
        let config = ServerConfig::in_memory();
        let mut args = create("github:acme/widget#1", "do this");
        args.owner = Some(key("github:acme/other#2"));
        args.deliver = true;

        let (_, delivery, _) = one(super::create(&config, args).await.expect("created"));
        match delivery.expect("a receipt") {
            WorkDelivery::Refused { reason } => {
                assert!(reason.contains("no running agent"), "{reason}");
                assert!(
                    reason.contains("recorded and assigned"),
                    "the receipt has to say the work still exists: {reason}"
                );
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        let (theirs, _, _) = mine_lists(
            super::mine(&config, &key("github:acme/other#2"), false)
                .await
                .expect("mine"),
        );
        assert_eq!(theirs.len(), 1, "it is waiting in its owner's list");
    }

    #[tokio::test]
    async fn work_is_never_delivered_to_the_session_that_created_it() {
        let config = ServerConfig::in_memory();
        let me = key("github:acme/widget#1");
        let mut args = create(me.as_str(), "note to self");
        args.owner = Some(me.clone());
        args.deliver = true;

        let (_, delivery, _) = one(super::create(&config, args).await.expect("created"));
        assert!(matches!(
            delivery.expect("a receipt"),
            WorkDelivery::Skipped { reason } if reason.contains("the caller")
        ));
    }

    #[tokio::test]
    async fn deliver_without_an_owner_is_rejected_rather_than_ignored() {
        let config = ServerConfig::in_memory();
        let mut args = create("a", "x");
        args.deliver = true;
        assert!(matches!(
            super::create(&config, args).await,
            Err(WorkError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn an_empty_title_and_an_oversized_brief_are_both_refused() {
        let config = ServerConfig::in_memory();
        assert!(super::create(&config, create("a", "   ")).await.is_err());

        let mut big = create("a", "x");
        big.brief = "b".repeat(MAX_BRIEF_BYTES + 1);
        let error = super::create(&config, big).await.expect_err("refused");
        assert!(error.to_string().contains("not a transcript"), "{error}");
    }

    #[tokio::test]
    async fn a_dangling_plan_or_parent_is_refused_not_stored() {
        let config = ServerConfig::in_memory();
        let mut bad_plan = create("a", "x");
        bad_plan.plan = Some(uuid::Uuid::new_v4().to_string());
        assert!(
            super::create(&config, bad_plan).await.is_err(),
            "a dangling plan id would make the work invisible to work_status"
        );

        let mut bad_parent = create("a", "x");
        bad_parent.parent = Some(uuid::Uuid::new_v4().to_string());
        assert!(super::create(&config, bad_parent).await.is_err());

        let mut not_a_uuid = create("a", "x");
        not_a_uuid.plan = Some("plan-7".into());
        assert!(super::create(&config, not_a_uuid).await.is_err());

        // And nothing was written on the way to any of those refusals.
        let report = super::status(&config, None).await.expect("status");
        assert!(matches!(
            report,
            WorkReport::Status { ref unplanned, .. } if unplanned.is_empty()
        ));
    }

    #[test]
    fn link_parsing_keeps_a_workspace_and_its_tracker_record_apart() {
        // The two render identically; only the prefix says which is meant.
        assert_eq!(
            parse_link("ws:github:acme/widget#7").expect("ws"),
            Link::Workspace(WorkspaceKey::new("github:acme/widget#7"))
        );
        assert!(matches!(
            parse_link("acme/widget#7").expect("tracker"),
            Link::Tracker(_)
        ));
        assert!(matches!(
            parse_link("https://example.test/doc").expect("url"),
            Link::Url(_)
        ));
        assert!(parse_link("ws:").is_err());
        assert!(parse_link("not a reference").is_err());
    }

    #[test]
    fn a_lifecycle_that_needs_a_detail_will_not_be_set_without_one() {
        assert!(parse_lifecycle("awaiting-answer", None).is_err());
        assert!(parse_lifecycle("held", Some("   ")).is_err());
        assert!(parse_lifecycle("underway", None).is_ok());
        // A failure may be bare — the cause is often an exit nobody narrated.
        assert!(matches!(
            parse_lifecycle("failed", None),
            Ok(Lifecycle::Failed { .. })
        ));
        assert!(parse_lifecycle("nearly-done", None).is_err());
        assert!(
            parse_lifecycle("completed", None).is_err(),
            "a completion carries a result and goes through is_completion"
        );
        assert!(is_completion("Done") && is_completion("completed"));
    }

    #[tokio::test]
    async fn a_sub_task_inherits_its_parents_plan_so_the_rollup_counts_it() {
        let config = ServerConfig::in_memory();
        let plan = Plan::new("ship it");
        crate::work_store::save_plan(&*config.store, &plan).expect("plan");

        let mut root = create("a", "root");
        root.plan = Some(plan.id.to_string());
        let (root, _, _) = one(super::create(&config, root).await.expect("created"));

        let mut child = create("a", "child");
        child.parent = Some(root.id.clone());
        let (child, _, _) = one(super::create(&config, child).await.expect("created"));
        assert_eq!(child.plan.as_deref(), Some(plan.id.to_string().as_str()));

        let WorkReport::Status { plans, .. } = super::status(&config, Some(&plan.id.to_string()))
            .await
            .expect("status")
        else {
            panic!("expected a status report");
        };
        assert_eq!((plans[0].done, plans[0].total), (0, 2));
        assert!(!plans[0].is_complete());
    }

    #[tokio::test]
    async fn a_completion_needs_a_summary_and_then_carries_it() {
        let config = ServerConfig::in_memory();
        let (work, _, _) = one(super::create(&config, create("a", "x")).await.expect("c"));

        let bare = UpdateArgs {
            by: key("a"),
            id: work.id.clone(),
            lifecycle: "completed".into(),
            detail: None,
            summary: None,
            artifacts: Vec::new(),
        };
        let error = super::update(&config, bare).await.expect_err("refused");
        assert!(
            error.to_string().contains("scrollback"),
            "the refusal has to say why a summary is required: {error}"
        );

        let done = UpdateArgs {
            by: key("a"),
            id: work.id.clone(),
            lifecycle: "completed".into(),
            detail: None,
            summary: Some("landed in #1".into()),
            artifacts: vec!["findings.md".into()],
        };
        let (work, _, notified) = one(super::update(&config, done).await.expect("completed"));
        assert_eq!(work.lifecycle, "completed");
        let result = work.result.expect("a result");
        assert_eq!(result.summary, "landed in #1");
        assert_eq!(result.artifacts, vec!["findings.md".to_string()]);
        assert!(
            matches!(
                notified.expect("a notice outcome"),
                WorkDelivery::Skipped { reason } if reason.contains("reported it")
            ),
            "an agent must not be told about the result it just filed"
        );
    }

    #[tokio::test]
    async fn a_terminal_task_refuses_a_later_report_and_keeps_the_first_result() {
        // The property that makes a result trustworthy: a replaced session
        // reporting late cannot overwrite what already landed.
        let config = ServerConfig::in_memory();
        let (work, _, _) = one(super::create(&config, create("a", "x")).await.expect("c"));
        let finish = |summary: &str| UpdateArgs {
            by: key("a"),
            id: work.id.clone(),
            lifecycle: "completed".into(),
            detail: None,
            summary: Some(summary.into()),
            artifacts: Vec::new(),
        };
        super::update(&config, finish("first")).await.expect("done");

        let error = super::update(&config, finish("second"))
            .await
            .expect_err("refused");
        assert!(
            error.to_string().contains("rejects further work"),
            "{error}"
        );

        let reopen = UpdateArgs {
            by: key("a"),
            id: work.id.clone(),
            lifecycle: "underway".into(),
            detail: None,
            summary: None,
            artifacts: Vec::new(),
        };
        assert!(
            super::update(&config, reopen).await.is_err(),
            "nor may a finished task be reopened"
        );

        let stored = crate::work_store::load_task(&*config.store, work.id.parse().unwrap())
            .expect("read")
            .expect("row");
        assert_eq!(stored.result.expect("result").summary, "first");
    }

    #[tokio::test]
    async fn an_unknown_work_id_is_an_error_not_a_silently_created_row() {
        let config = ServerConfig::in_memory();
        let args = UpdateArgs {
            by: key("a"),
            id: uuid::Uuid::new_v4().to_string(),
            lifecycle: "underway".into(),
            detail: None,
            summary: None,
            artifacts: Vec::new(),
        };
        assert!(super::update(&config, args).await.is_err());
    }

    #[tokio::test]
    async fn a_question_is_carried_as_the_rows_detail() {
        let config = ServerConfig::in_memory();
        let (work, _, _) = one(super::create(&config, create("a", "x")).await.expect("c"));
        let asking = UpdateArgs {
            by: key("a"),
            id: work.id.clone(),
            lifecycle: "awaiting-answer".into(),
            detail: Some("which base branch?".into()),
            summary: None,
            artifacts: Vec::new(),
        };
        let (work, _, notified) = one(super::update(&config, asking).await.expect("moved"));
        assert_eq!(work.lifecycle, "awaiting answer");
        assert_eq!(work.detail.as_deref(), Some("which base branch?"));
        assert!(
            matches!(notified.expect("outcome"), WorkDelivery::Skipped { reason } if reason.contains("terminal")),
            "a question is not a result; nobody is notified"
        );
    }

    #[tokio::test]
    async fn the_three_lists_keep_apart_who_is_on_the_hook() {
        let config = ServerConfig::in_memory();
        let me = key("github:acme/widget#1");
        let them = key("github:acme/other#2");

        let mut asked = create(me.as_str(), "do this for me");
        asked.owner = Some(them.clone());
        super::create(&config, asked).await.expect("c");

        let mut own = create(me.as_str(), "my own job");
        own.owner = Some(me.clone());
        super::create(&config, own).await.expect("c");

        super::create(&config, create(me.as_str(), "queued, nobody on it"))
            .await
            .expect("c");

        let (mine, waiting, unassigned) =
            mine_lists(super::mine(&config, &me, false).await.expect("mine"));
        let titles =
            |rows: Vec<WorkRow>| -> Vec<String> { rows.into_iter().map(|row| row.title).collect() };
        assert_eq!(titles(mine), vec!["my own job".to_string()]);
        assert_eq!(
            titles(waiting),
            vec!["do this for me".to_string()],
            "self-assigned work must not appear in both lists"
        );
        assert_eq!(
            titles(unassigned),
            vec!["queued, nobody on it".to_string()],
            "work nobody owns is my backlog, not a sibling's debt"
        );
    }

    #[tokio::test]
    async fn finished_work_is_hidden_unless_asked_for() {
        let config = ServerConfig::in_memory();
        let me = key("github:acme/widget#1");
        let mut own = create(me.as_str(), "job");
        own.owner = Some(me.clone());
        let (work, _, _) = one(super::create(&config, own).await.expect("c"));
        super::update(
            &config,
            UpdateArgs {
                by: me.clone(),
                id: work.id,
                lifecycle: "completed".into(),
                detail: None,
                summary: Some("done".into()),
                artifacts: Vec::new(),
            },
        )
        .await
        .expect("done");

        let (open, _, _) = mine_lists(super::mine(&config, &me, false).await.expect("mine"));
        assert!(open.is_empty());
        let (all, _, _) = mine_lists(super::mine(&config, &me, true).await.expect("mine"));
        assert_eq!(all.len(), 1);
    }

    #[tokio::test]
    async fn status_lists_unplanned_work_and_refuses_an_unknown_plan() {
        let config = ServerConfig::in_memory();
        super::create(&config, create("a", "loose end"))
            .await
            .expect("c");

        let WorkReport::Status {
            plans,
            unplanned,
            undecodable_rows,
        } = super::status(&config, None).await.expect("status")
        else {
            panic!("expected a status report");
        };
        assert!(plans.is_empty());
        assert_eq!(unplanned[0].title, "loose end");
        assert_eq!(undecodable_rows, 0);

        assert!(
            super::status(&config, Some(&uuid::Uuid::new_v4().to_string()))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_row_carries_its_latest_event_but_not_its_whole_history() {
        let config = ServerConfig::in_memory();
        let (work, _, _) = one(super::create(&config, create("a", "x")).await.expect("c"));
        let (work, _, _) = one(super::update(
            &config,
            UpdateArgs {
                by: key("a"),
                id: work.id,
                lifecycle: "underway".into(),
                detail: None,
                summary: None,
                artifacts: Vec::new(),
            },
        )
        .await
        .expect("moved"));
        assert_eq!(work.events, 2, "the count is on the wire");
        let last = work.last_event.expect("the latest entry");
        assert_eq!(last.change, "pending → underway");
        assert_eq!(last.by.as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn a_sibling_that_requested_the_work_is_the_one_notified() {
        // The complement of the guard above: when someone else reported it,
        // a notice is attempted rather than skipped. There is no live agent
        // here, so the honest outcome is "nobody to tell" — not "no notice
        // was due".
        let config = ServerConfig::in_memory();
        let mut args = create("github:acme/widget#1", "do this for me");
        args.owner = Some(key("github:acme/other#2"));
        let (work, _, _) = one(super::create(&config, args).await.expect("c"));

        let (_, _, notified) = one(super::update(
            &config,
            UpdateArgs {
                by: key("github:acme/other#2"),
                id: work.id,
                lifecycle: "completed".into(),
                detail: None,
                summary: Some("shipped".into()),
                artifacts: Vec::new(),
            },
        )
        .await
        .expect("completed"));
        assert!(
            matches!(
                notified.expect("outcome"),
                WorkDelivery::Skipped { reason } if reason.contains("no running agent")
            ),
            "the requester was due a notice and had nowhere to receive it"
        );
    }

    #[tokio::test]
    async fn a_dispatched_call_reaches_the_same_code_as_a_direct_one() {
        // `call` is what both surfaces go through; a variant that did not
        // dispatch would leave one of them silently answering nothing.
        let config = ServerConfig::in_memory();
        let report = super::call(
            &config,
            WorkRequest::Create {
                requester: key("a"),
                title: "through the dispatcher".into(),
                brief: String::new(),
                owner: None,
                deliver: false,
                plan: None,
                parent: None,
                links: Vec::new(),
            },
        )
        .await
        .expect("created");
        let (work, _, _) = one(report);

        let mine = super::call(
            &config,
            WorkRequest::Mine {
                workspace: key("a"),
                include_done: false,
            },
        )
        .await
        .expect("mine");
        let (_, _, unassigned) = mine_lists(mine);
        assert_eq!(unassigned.len(), 1);

        let updated = super::call(
            &config,
            WorkRequest::Update {
                by: key("a"),
                id: work.id,
                lifecycle: "underway".into(),
                detail: None,
                summary: None,
                artifacts: Vec::new(),
            },
        )
        .await
        .expect("moved");
        assert_eq!(one(updated).0.lifecycle, "underway");

        assert!(matches!(
            super::call(&config, WorkRequest::Status { plan: None }).await,
            Ok(WorkReport::Status { .. })
        ));
    }
}
