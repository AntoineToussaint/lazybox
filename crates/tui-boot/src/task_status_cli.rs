//! `lazybox task status <ref>` — the documented shell answer to "are we
//! working on `owner/repo#151`?" (#1785).
//!
//! The daemon does the interpreting; this is a renderer. Both halves of the
//! answer — the MCP `task_status` tool an agent calls and this command —
//! go through [`lazybox_ipc::Command::QueryTaskStatus`], so a user at a shell
//! and an agent in a session read the same facts and the same verdict.
//!
//! This is also the fallback path when a session has no MCP tools at all
//! (Codex, `--strict-mcp-config`, a restricted profile): the daemon socket is
//! already the authenticated local channel, so nothing has to be provisioned
//! or copied into a prompt.

use anyhow::Context as _;
use lazybox_ipc::task_status::{
    ClaimFacts, TaskStatusError, TaskStatusReport, WorkState, WorkspaceStatus,
};
use lazybox_server::lifecycle;
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "lazybox task status <owner/repo#N | URL | LINEAR-KEY> [--repo <owner/repo>] \
                     [--json] [--socket <path>]";

/// How long to wait for the daemon's reply. The lookup is a store scan plus an
/// in-memory read, so anything approaching this is the daemon being wedged —
/// which must be reported as such, not as "no worker".
const TIMEOUT: Duration = Duration::from_secs(10);

/// `lazybox task ...`.
///
/// Failures are printed to **stdout** before bubbling: `init_tracing` has
/// redirected fd 2 into the log file by the time a subcommand runs, so an
/// `Err` alone reaches the log and leaves the caller staring at a silent
/// non-zero exit — the one outcome a status lookup must never produce.
pub async fn task_subcommand(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("status") => status(&args[1..]).await.inspect_err(|error| {
            println!("lazybox task status: {error:#}");
        }),
        _ => {
            println!("{USAGE}");
            std::process::exit(2);
        }
    }
}

async fn status(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let json = crate::take_flag(&mut args, "--json");
    let socket_path = crate::take_value(&mut args, "--socket")
        .map(PathBuf::from)
        .unwrap_or_else(lifecycle::socket_path);
    // `--issue` / `--pr` / `--ticket` are aliases for the positional reference:
    // the issue's proposed spelling, and what a caller reaches for first. All
    // three are consumed unconditionally rather than short-circuited, so giving
    // two of them is diagnosed as the ambiguity it is — left in `args`, the
    // second one surfaced as "unknown flag", which sends the caller looking for
    // a typo that isn't there.
    let aliases: Vec<String> = ["--issue", "--pr", "--ticket"]
        .into_iter()
        .filter_map(|flag| crate::take_value(&mut args, flag))
        .collect();
    if aliases.len() > 1 {
        println!(
            "lazybox task status: pass one record, not {} (--issue / --pr / --ticket are \
             aliases for the same argument)\n{USAGE}",
            aliases.len()
        );
        std::process::exit(2);
    }
    let reference = aliases.into_iter().next();
    let default_repo = crate::take_value(&mut args, "--repo");

    if let Some(unknown) = args.iter().find(|arg| arg.starts_with("--")) {
        println!("lazybox task status: unknown flag {unknown}\n{USAGE}");
        std::process::exit(2);
    }
    let Some(reference) = reference.or_else(|| args.first().cloned()) else {
        println!("lazybox task status: name a record to look up\n{USAGE}");
        std::process::exit(2);
    };

    let (mut client, _peer) = lazybox_ipc::socket::connect(&socket_path)
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "connect to daemon at {}: {error} (is lazybox running?)",
                socket_path.display(),
            )
        })
        .context(
            "lazybox task status could not reach the daemon, so it cannot tell you whether \
                  anyone is working on this record",
        )?;
    // Deliberately no `Subscribe`: the daemon answers on this connection, so
    // subscribing would only buy a full `Snapshot` of every workspace and
    // terminal — and put the reply on a bus that drops events for a lagging
    // client.
    let client_request_id = uuid::Uuid::new_v4().to_string();
    client.send(lazybox_ipc::Command::QueryTaskStatus {
        reference: reference.clone(),
        default_repo,
        client_request_id: Some(client_request_id.clone()),
    })?;

    let report = match await_report(&mut client, &client_request_id).await? {
        Ok(report) => report,
        // A lookup that could not be established exits non-zero: a caller
        // scripting this must never read a failure as "nobody is working on it".
        Err(error) => {
            println!("lazybox task status: {error}");
            std::process::exit(match error {
                TaskStatusError::UnresolvedReference { .. } => 2,
                TaskStatusError::Unavailable { .. } => 1,
            });
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render(&report));
    }
    Ok(())
}

async fn await_report(
    client: &mut lazybox_ipc::Client,
    client_request_id: &str,
) -> anyhow::Result<Result<TaskStatusReport, TaskStatusError>> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, client.recv()).await {
            Ok(Some(lazybox_ipc::Event::TaskStatus {
                client_request_id: id,
                result,
            })) if id.as_deref() == Some(client_request_id) => return Ok(result),
            Ok(Some(_)) => continue,
            Ok(None) => anyhow::bail!("daemon closed the connection before answering"),
            Err(_) => anyhow::bail!("timed out waiting for the daemon to answer"),
        }
    }
}

/// The human rendering: a headline verdict, then the facts that back it, each
/// labelled with what it is evidence *of*. Deliberately a few lines — a status
/// check is read at a glance, and `--json` is there for everything else.
fn render(report: &TaskStatusReport) -> String {
    let mut out = String::new();
    let label = report
        .task
        .repo
        .as_deref()
        .zip(report.task.number)
        .map(|(repo, number)| format!("{repo}#{number}"))
        .unwrap_or_else(|| report.task.id.to_string());

    out.push_str(&format!("{label}  {}\n", headline(report.verdict.state)));
    out.push_str(&format!("  {}\n", report.verdict.reason));

    for workspace in &report.workspaces {
        out.push('\n');
        out.push_str(&format!(
            "  workspace  {} ({})\n",
            workspace.key, workspace.name
        ));
        if let Some(role) = workspace.role {
            out.push_str(&format!("  role       {role:?}\n"));
        }
        render_tracker(&mut out, workspace);
        render_agents(&mut out, workspace);
        render_sessions(&mut out, workspace);
        render_claim(&mut out, &workspace.claim);
        if let Some(blocker) = &workspace.blocker {
            out.push_str(&format!(
                "  blocker    {} ({}, declared {})\n",
                blocker.reason,
                blocker.kind,
                blocker
                    .since
                    .map_or_else(|| "at an unreadable time".to_string(), |at| at.to_rfc3339()),
            ));
        }
    }

    if !report.unreadable_workspaces.is_empty() {
        out.push_str(&format!(
            "\n  warning    {} workspace row(s) could not be read, so this answer may be \
             incomplete: {}\n",
            report.unreadable_workspaces.len(),
            report.unreadable_workspaces.join(", "),
        ));
    }
    out.push_str(&format!(
        "\n  observed   {}\n",
        report.observed_at.to_rfc3339()
    ));
    out
}

fn headline(state: WorkState) -> &'static str {
    match state {
        WorkState::Working => "an agent is working on it now",
        WorkState::AwaitingInput => "an agent is waiting on input",
        WorkState::TurnEnded => "an agent turn has ended (not necessarily the task)",
        WorkState::AgentExited => "no agent is running",
        WorkState::ClaimedElsewhere => "claimed by a worker this box cannot see",
        WorkState::NotStarted => "not started here",
        WorkState::NoWorkspace => "no workspace here",
        WorkState::Archived => "archived here",
        WorkState::Unknown => "cannot be established",
        WorkState::Stalled => "an agent stopped on an error",
    }
}

fn render_tracker(out: &mut String, workspace: &WorkspaceStatus) {
    if let Some(tracker) = &workspace.tracker {
        out.push_str(&format!(
            "  {:<10} {} {:?} · CI {:?} · review {:?} (as of {})\n",
            format!("{:?}", tracker.kind).to_lowercase(),
            tracker.id,
            tracker.state,
            tracker.ci,
            tracker.review,
            tracker.updated_at.to_rfc3339(),
        ));
    }
    if let Some(matched) = &workspace.matched_tracker {
        out.push_str(&format!(
            "  {:<10} {} {:?} (the record you asked about)\n",
            format!("{:?}", matched.kind).to_lowercase(),
            matched.id,
            matched.state,
        ));
    }
}

fn render_agents(out: &mut String, workspace: &WorkspaceStatus) {
    if workspace.agents.is_empty() {
        out.push_str("  agent      none running\n");
        return;
    }
    for agent in &workspace.agents {
        let turn = agent
            .turn
            .map(|turn| format!("{turn:?}"))
            .unwrap_or_else(|| "not reported".to_string());
        out.push_str(&format!("  agent      {} · turn {turn}\n", agent.agent));
    }
}

fn render_sessions(out: &mut String, workspace: &WorkspaceStatus) {
    for session in &workspace.sessions {
        out.push_str(&format!(
            "  session    {} {:?}{}\n",
            session.name,
            session.state,
            if session.has_live_agent {
                ""
            } else {
                " (no live agent in it)"
            },
        ));
    }
}

fn render_claim(out: &mut String, claim: &ClaimFacts) {
    for held in &claim.active {
        out.push_str(&format!(
            "  claim      active until {} — {}\n",
            held.expires_at.to_rfc3339(),
            if held.verified_locally {
                "held by this box"
            } else {
                "held elsewhere; not proof of a running worker"
            },
        ));
    }
    for held in &claim.expired {
        out.push_str(&format!(
            "  claim      EXPIRED {} — lapsed, not a running worker\n",
            held.expires_at.to_rfc3339(),
        ));
    }
    if claim.unqualified {
        out.push_str("  claim      bare `working` label — no holder, no expiry\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_ipc::task_status::{
        AgentFacts, TASK_STATUS_SCHEMA_VERSION, TaskRefInfo, TrackerFacts, Verdict,
    };

    fn report(state: WorkState, workspaces: Vec<WorkspaceStatus>) -> TaskStatusReport {
        TaskStatusReport {
            schema_version: TASK_STATUS_SCHEMA_VERSION,
            task: TaskRefInfo {
                id: lazybox_core::TaskId {
                    source: "github".into(),
                    key: "obin-ai/core-solutions#151".into(),
                },
                repo: Some("obin-ai/core-solutions".into()),
                number: Some(151),
            },
            observed_at: chrono::Utc::now(),
            workspaces,
            unreadable_workspaces: Vec::new(),
            verdict: Verdict {
                state,
                reason: "because".into(),
                evidence: Vec::new(),
            },
        }
    }

    fn workspace() -> WorkspaceStatus {
        WorkspaceStatus {
            key: lazybox_core::WorkspaceKey::new("github-obin-ai-core-solutions-187"),
            name: "fixture integrity guard".into(),
            matched: lazybox_core::TaskId {
                source: "github".into(),
                key: "obin-ai/core-solutions#151".into(),
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

    #[test]
    fn the_headline_names_the_record_the_caller_asked_about() {
        let text = render(&report(WorkState::NoWorkspace, Vec::new()));
        assert!(text.starts_with("obin-ai/core-solutions#151"), "{text}");
    }

    /// The distinction the whole feature exists for must survive rendering.
    #[test]
    fn a_finished_turn_is_not_rendered_as_a_finished_task() {
        let mut workspace = workspace();
        workspace.agents.push(AgentFacts {
            agent: "claude".into(),
            turn: Some(lazybox_ipc::AgentState::Done),
            model: None,
            last_prompt_at: None,
            on_main: false,
        });
        let text = render(&report(WorkState::TurnEnded, vec![workspace]));
        assert!(
            text.contains("not necessarily the task"),
            "the headline must not read as task completion: {text}"
        );
        assert!(text.contains("turn Done"), "{text}");
    }

    #[test]
    fn a_remote_claim_is_rendered_as_unproven() {
        let mut workspace = workspace();
        workspace
            .claim
            .active
            .push(lazybox_ipc::task_status::ClaimHolder {
                device: "0123456789abcdef0123".into(),
                session: "0123456789".into(),
                expires_at: chrono::Utc::now() + chrono::Duration::minutes(30),
                verified_locally: false,
            });
        let text = render(&report(WorkState::ClaimedElsewhere, vec![workspace]));
        assert!(
            text.contains("not proof of a running worker"),
            "a claim must never render as a confirmed worker: {text}"
        );
    }

    #[test]
    fn an_expired_claim_says_it_lapsed() {
        let mut workspace = workspace();
        workspace
            .claim
            .expired
            .push(lazybox_ipc::task_status::ClaimHolder {
                device: "0123456789abcdef0123".into(),
                session: "0123456789".into(),
                expires_at: chrono::Utc::now() - chrono::Duration::hours(2),
                verified_locally: false,
            });
        let text = render(&report(WorkState::AgentExited, vec![workspace]));
        assert!(text.contains("EXPIRED"), "{text}");
    }

    /// A query for the issue must show the PR that took over its row.
    #[test]
    fn the_matched_record_is_rendered_beside_the_headline_one() {
        let mut workspace = workspace();
        workspace.tracker = Some(TrackerFacts {
            id: lazybox_core::TaskId {
                source: "github".into(),
                key: "obin-ai/core-solutions#187".into(),
            },
            kind: lazybox_core::TaskKind::Pr,
            title: "guard".into(),
            url: "https://example.invalid".into(),
            state: lazybox_core::TaskState::Open,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            updated_at: chrono::Utc::now(),
            closed_at: None,
        });
        workspace.matched_tracker = Some(TrackerFacts {
            id: lazybox_core::TaskId {
                source: "github".into(),
                key: "obin-ai/core-solutions#151".into(),
            },
            kind: lazybox_core::TaskKind::Issue,
            title: "reusable suite".into(),
            url: "https://example.invalid".into(),
            state: lazybox_core::TaskState::Open,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            updated_at: chrono::Utc::now(),
            closed_at: None,
        });
        let text = render(&report(WorkState::TurnEnded, vec![workspace]));
        assert!(text.contains("core-solutions#187"), "{text}");
        assert!(text.contains("core-solutions#151"), "{text}");
        assert!(text.contains("the record you asked about"), "{text}");
    }

    #[test]
    fn a_workspace_with_no_agent_says_so_explicitly() {
        let text = render(&report(WorkState::NotStarted, vec![workspace()]));
        assert!(text.contains("agent      none running"), "{text}");
    }

    /// A partial answer must announce itself; rendering only the rows that
    /// decoded would present it as complete.
    #[test]
    fn unreadable_rows_are_surfaced_in_the_human_output() {
        let mut report = report(WorkState::NotStarted, vec![workspace()]);
        report.unreadable_workspaces = vec!["github-o-r-corrupt".into()];
        let text = render(&report);
        assert!(text.contains("incomplete"), "{text}");
        assert!(text.contains("github-o-r-corrupt"), "{text}");
    }

    #[test]
    fn a_blocker_with_an_unreadable_time_does_not_render_a_fabricated_date() {
        let mut workspace = workspace();
        workspace.blocker = Some(lazybox_ipc::task_status::BlockerFacts {
            reason: "waiting on legal".into(),
            kind: "decision".into(),
            owner: "operator".into(),
            since: None,
        });
        let text = render(&report(WorkState::NotStarted, vec![workspace]));
        assert!(text.contains("at an unreadable time"), "{text}");
    }

    #[test]
    fn every_work_state_has_a_headline() {
        for state in [
            WorkState::Working,
            WorkState::AwaitingInput,
            WorkState::TurnEnded,
            WorkState::AgentExited,
            WorkState::ClaimedElsewhere,
            WorkState::NotStarted,
            WorkState::NoWorkspace,
            WorkState::Archived,
            WorkState::Unknown,
            WorkState::Stalled,
        ] {
            assert!(!headline(state).is_empty(), "{state:?}");
        }
    }
}
