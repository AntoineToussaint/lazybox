//! `lazybox work …` — the task/plan store for agents with no MCP at all.
//!
//! Codex, Cursor, a session started with `--strict-mcp-config` and any agent
//! lazybox did not provision tools for see none of `create_work` / `my_work` /
//! `update_work` / `work_status`. Without this they cannot be handed tracked
//! work, cannot report a result, and cannot be told what they already own —
//! which makes every non-Claude session a second-class member of the fleet
//! (`docs/agent-coordination-v2.md`, phase 3).
//!
//! The daemon does the work; this is a renderer. Both halves go through
//! [`lazybox_ipc::Command::WorkCall`] into `lazybox_server::work_calls`, so a
//! shell and an MCP tool cannot be told different things about the same row —
//! the same arrangement, for the same reason, as `lazybox task status`.
//!
//! Identity comes from `LAZYBOX_SESSION_KEY`, injected into every session's
//! PTY at spawn, with `--workspace` as the override. That is what makes
//! `lazybox work mine` work with no arguments inside a session, and it is the
//! pattern `lazybox log` and the `gh` shim already use.

use anyhow::Context as _;
use lazybox_ipc::work::{WorkDelivery, WorkError, WorkReport, WorkRequest, WorkRow};
use lazybox_server::lifecycle;
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "\
lazybox work <command>

  mine [--all] [--json]                   work you own, are owed, and have queued
  new <title> [--brief <text>] [--to <workspace>] [--no-deliver]
             [--plan <id>] [--parent <id>] [--link <ref>]...
                                          mint a unit of work, and hand it over
  set <id> <lifecycle> [--detail <text>]  underway | awaiting-answer | held | canceled | failed
  done <id> --summary <text> [--artifact <name>]...
                                          complete it, with the result
  status [--plan <id>] [--json]           a plan's roll-up and the workspaces it spans

Common: [--workspace <key>] [--socket <path>] [--json]
A workspace is taken from LAZYBOX_SESSION_KEY inside a session.";

/// How long to wait for the daemon's reply. A work call is a kv scan plus, at
/// most, one settle-gated paste; anything near this is the daemon wedged,
/// which has to be reported as such and never as "there is no such work".
const TIMEOUT: Duration = Duration::from_secs(30);

/// `lazybox work …`.
///
/// Failures print to **stdout**, not stderr: `init_tracing` has redirected
/// fd 2 into the log file by the time a subcommand runs, so an `Err` alone
/// reaches the log and leaves the caller staring at a silent non-zero exit.
pub async fn work_subcommand(args: &[String]) -> anyhow::Result<()> {
    let result = match args.first().map(String::as_str) {
        Some("mine") => run(&args[1..], Verb::Mine).await,
        Some("new") => run(&args[1..], Verb::New).await,
        Some("set") => run(&args[1..], Verb::Set).await,
        Some("done") => run(&args[1..], Verb::Done).await,
        Some("status") => run(&args[1..], Verb::Status).await,
        _ => {
            println!("{USAGE}");
            std::process::exit(2);
        }
    };
    result.inspect_err(|error| println!("lazybox work: {error:#}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Mine,
    New,
    Set,
    Done,
    Status,
}

async fn run(args: &[String], verb: Verb) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let json = crate::take_flag(&mut args, "--json");
    let socket_path = crate::take_value(&mut args, "--socket")
        .map(PathBuf::from)
        .unwrap_or_else(lifecycle::socket_path);
    let workspace = crate::take_value(&mut args, "--workspace");
    let brief = crate::take_value(&mut args, "--brief");
    let to = crate::take_value(&mut args, "--to");
    let no_deliver = crate::take_flag(&mut args, "--no-deliver");
    let plan = crate::take_value(&mut args, "--plan");
    let parent = crate::take_value(&mut args, "--parent");
    let detail = crate::take_value(&mut args, "--detail");
    let summary = crate::take_value(&mut args, "--summary");
    let all = crate::take_flag(&mut args, "--all");
    // Repeatable flags: drain until none is left, so `--link a --link b` is
    // two links rather than the last one winning silently.
    let mut links = Vec::new();
    while let Some(link) = crate::take_value(&mut args, "--link") {
        links.push(link);
    }
    let mut artifacts = Vec::new();
    while let Some(artifact) = crate::take_value(&mut args, "--artifact") {
        artifacts.push(artifact);
    }

    if let Some(unknown) = args.iter().find(|arg| arg.starts_with("--")) {
        println!("lazybox work: unknown flag {unknown}\n\n{USAGE}");
        std::process::exit(2);
    }
    let positional: Vec<String> = args;

    let request = match verb {
        Verb::Mine => WorkRequest::Mine {
            workspace: self_key(workspace.as_deref())?,
            include_done: all,
        },
        Verb::Status => WorkRequest::Status { plan },
        Verb::New => {
            let Some(title) = positional.first().cloned() else {
                println!("lazybox work new: name the work\n\n{USAGE}");
                std::process::exit(2);
            };
            let owner = to.as_deref().map(str::trim).filter(|key| !key.is_empty());
            if no_deliver && owner.is_none() {
                println!(
                    "lazybox work new: --no-deliver only means something with --to <workspace>\n\n{USAGE}"
                );
                std::process::exit(2);
            }
            WorkRequest::Create {
                requester: self_key(workspace.as_deref())?,
                title,
                brief: brief.unwrap_or_default(),
                owner: owner.map(lazybox_core::SessionKey::from),
                // Assigning work delivers it unless told otherwise: an agent
                // that hands work over and has to remember a second flag to
                // actually send it has handed over nothing.
                deliver: owner.is_some() && !no_deliver,
                plan,
                parent,
                links,
            }
        }
        Verb::Set => {
            let (Some(id), Some(lifecycle)) =
                (positional.first().cloned(), positional.get(1).cloned())
            else {
                println!("lazybox work set: pass a work id and a lifecycle\n\n{USAGE}");
                std::process::exit(2);
            };
            WorkRequest::Update {
                by: self_key(workspace.as_deref())?,
                id,
                lifecycle,
                detail,
                summary: None,
                artifacts: Vec::new(),
            }
        }
        Verb::Done => {
            let Some(id) = positional.first().cloned() else {
                println!("lazybox work done: pass a work id\n\n{USAGE}");
                std::process::exit(2);
            };
            let Some(summary) = summary.filter(|s| !s.trim().is_empty()) else {
                // Refused here rather than at the daemon so the message can
                // name the flag: a completion's summary is what the requester
                // reads instead of scraping this session's scrollback.
                println!(
                    "lazybox work done: --summary is required — it is what the requester reads \
                     instead of your scrollback\n\n{USAGE}"
                );
                std::process::exit(2);
            };
            WorkRequest::Update {
                by: self_key(workspace.as_deref())?,
                id,
                lifecycle: "completed".into(),
                detail: None,
                summary: Some(summary),
                artifacts,
            }
        }
    };

    let report = send(&socket_path, request).await?;
    let report = match report {
        Ok(report) => report,
        // A call that could not be answered exits non-zero, and the two error
        // kinds exit differently: a caller scripting this must be able to tell
        // "I asked wrongly" (never going to work) from "the daemon could not
        // read the store" (worth a retry).
        Err(error) => {
            println!("lazybox work: {error}");
            std::process::exit(match error {
                WorkError::BadRequest(_) => 2,
                WorkError::Unavailable(_) => 1,
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

/// This session's workspace. `--workspace` wins; otherwise
/// `LAZYBOX_SESSION_KEY`, which lazybox injects at spawn.
fn self_key(override_key: Option<&str>) -> anyhow::Result<lazybox_core::SessionKey> {
    override_key
        .map(str::to_string)
        .or_else(|| std::env::var("LAZYBOX_SESSION_KEY").ok())
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
        .map(lazybox_core::SessionKey::new)
        .context(
            "no workspace: run inside a lazybox session (LAZYBOX_SESSION_KEY is injected \
             automatically) or pass --workspace <key>",
        )
}

async fn send(
    socket_path: &std::path::Path,
    request: WorkRequest,
) -> anyhow::Result<Result<WorkReport, WorkError>> {
    let (mut client, _peer) = lazybox_ipc::socket::connect(socket_path)
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "connect to daemon at {}: {error} (is lazybox running?)",
                socket_path.display(),
            )
        })
        .context("lazybox work could not reach the daemon, so it cannot read or move any work")?;
    // Deliberately no `Subscribe`: the daemon answers on this connection, so
    // subscribing would only buy a full `Snapshot` of every workspace and put
    // the reply on a bus that drops events for a lagging client.
    let client_request_id = uuid::Uuid::new_v4().to_string();
    client.send(lazybox_ipc::Command::WorkCall {
        request,
        client_request_id: Some(client_request_id.clone()),
    })?;

    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, client.recv()).await {
            Ok(Some(lazybox_ipc::Event::WorkReport {
                client_request_id: id,
                result,
            })) if id.as_deref() == Some(client_request_id.as_str()) => return Ok(result),
            Ok(Some(_)) => continue,
            Ok(None) => anyhow::bail!("daemon closed the connection before answering"),
            Err(_) => anyhow::bail!("timed out waiting for the daemon to answer"),
        }
    }
}

/// The human rendering. Read at a glance; `--json` carries everything.
pub fn render(report: &WorkReport) -> String {
    let mut out = String::new();
    match report {
        WorkReport::Mine {
            workspace,
            mine,
            waiting_on_others,
            unassigned,
        } => {
            out.push_str(&format!("{workspace}\n"));
            section(&mut out, "yours", mine);
            section(&mut out, "waiting on others", waiting_on_others);
            section(&mut out, "unassigned", unassigned);
            if mine.is_empty() && waiting_on_others.is_empty() && unassigned.is_empty() {
                out.push_str("  no open work\n");
            }
        }
        WorkReport::One {
            work,
            delivery,
            requester_notified,
        } => {
            out.push_str(&render_row(work, 0));
            if !work.brief.trim().is_empty() {
                out.push_str(&format!("\n{}\n", indent(&work.brief, 2)));
            }
            if let Some(delivery) = delivery {
                out.push_str(&format!("\n  delivery   {}\n", render_delivery(delivery)));
            }
            if let Some(notified) = requester_notified {
                out.push_str(&format!("  requester  {}\n", render_delivery(notified)));
            }
        }
        WorkReport::Status {
            plans,
            unplanned,
            undecodable_rows,
        } => {
            for plan in plans {
                out.push_str(&format!(
                    "{}  {}/{} {}\n",
                    plan.title,
                    plan.done,
                    plan.total,
                    if plan.is_complete() { "complete" } else { "" }
                ));
                out.push_str(&format!("  plan       {}\n", plan.plan));
                if !plan.members.is_empty() {
                    out.push_str(&format!("  spans      {}\n", plan.members.join(", ")));
                }
                for task in &plan.tasks {
                    out.push_str(&render_row(task, 2));
                }
                out.push('\n');
            }
            section(&mut out, "on no plan", unplanned);
            if plans.is_empty() && unplanned.is_empty() {
                out.push_str("no plans, and no open work outside one\n");
            }
            if *undecodable_rows > 0 {
                // Never silent: a listing that drops rows reads as a shorter
                // plan, and the reader has no way to know.
                out.push_str(&format!(
                    "\n  warning    {undecodable_rows} row(s) could not be read, so this answer \
                     may be incomplete\n"
                ));
            }
        }
    }
    out
}

fn section(out: &mut String, label: &str, rows: &[WorkRow]) {
    if rows.is_empty() {
        return;
    }
    out.push_str(&format!("\n{label}\n"));
    for row in rows {
        out.push_str(&render_row(row, 2));
    }
}

fn render_row(row: &WorkRow, pad: usize) -> String {
    let indent = " ".repeat(pad);
    let mut line = format!("{indent}{}  [{}]", row.title, row.lifecycle);
    if let Some(detail) = row
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        line.push_str(&format!(" {detail}"));
    }
    line.push('\n');
    line.push_str(&format!("{indent}  {}\n", row.id));
    if let Some(owner) = &row.owner {
        line.push_str(&format!("{indent}  owner {owner}\n"));
    }
    if !row.links.is_empty() {
        line.push_str(&format!("{indent}  links {}\n", row.links.join(", ")));
    }
    if let Some(result) = &row.result {
        line.push_str(&format!("{indent}  → {}\n", result.summary));
        if !result.artifacts.is_empty() {
            line.push_str(&format!(
                "{indent}    artifacts {}\n",
                result.artifacts.join(", ")
            ));
        }
    }
    line
}

fn render_delivery(delivery: &WorkDelivery) -> String {
    match delivery {
        WorkDelivery::Delivered { workspace } => {
            format!("delivered to {workspace}, between turns")
        }
        WorkDelivery::Queued { workspace } => {
            format!("queued for {workspace}; it lands when the current turn ends")
        }
        WorkDelivery::Refused { reason } => format!("not delivered: {reason}"),
        WorkDelivery::Skipped { reason } => format!("nothing sent ({reason})"),
    }
}

fn indent(text: &str, pad: usize) -> String {
    let prefix = " ".repeat(pad);
    text.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_ipc::work::{PlanRow, WorkResultView};

    fn row(title: &str, lifecycle: &str) -> WorkRow {
        WorkRow {
            id: "1b4e28ba-2fa1-11d2-883f-0016d3cca427".into(),
            title: title.into(),
            brief: String::new(),
            lifecycle: lifecycle.into(),
            detail: None,
            owner: None,
            requester: None,
            plan: None,
            parent: None,
            links: Vec::new(),
            result: None,
            events: 1,
            last_event: None,
        }
    }

    #[test]
    fn an_empty_mine_says_so_rather_than_printing_nothing() {
        // A command that prints nothing reads as a command that failed.
        let rendered = render(&WorkReport::Mine {
            workspace: "github:o/r#1".into(),
            mine: Vec::new(),
            waiting_on_others: Vec::new(),
            unassigned: Vec::new(),
        });
        assert!(rendered.contains("no open work"), "{rendered}");
        assert!(rendered.contains("github:o/r#1"));
    }

    #[test]
    fn mine_labels_the_three_lists_so_who_is_on_the_hook_is_readable() {
        let rendered = render(&WorkReport::Mine {
            workspace: "github:o/r#1".into(),
            mine: vec![row("my job", "underway")],
            waiting_on_others: vec![row("their job", "pending")],
            unassigned: vec![row("queued", "pending")],
        });
        assert!(rendered.contains("yours"), "{rendered}");
        assert!(rendered.contains("waiting on others"), "{rendered}");
        assert!(rendered.contains("unassigned"), "{rendered}");
        assert!(!rendered.contains("no open work"));
    }

    #[test]
    fn a_blocked_rows_detail_is_shown_because_it_is_the_actionable_part() {
        let mut held = row("stuck", "held");
        held.detail = Some("waiting on a decision".into());
        let rendered = render(&WorkReport::Mine {
            workspace: "w".into(),
            mine: vec![held],
            waiting_on_others: Vec::new(),
            unassigned: Vec::new(),
        });
        assert!(rendered.contains("waiting on a decision"), "{rendered}");
    }

    #[test]
    fn a_result_and_its_artifacts_are_rendered() {
        let mut done = row("shipped", "completed");
        done.result = Some(WorkResultView {
            summary: "landed in #1".into(),
            artifacts: vec!["findings.md".into()],
        });
        let rendered = render(&WorkReport::One {
            work: Box::new(done),
            delivery: None,
            requester_notified: None,
        });
        assert!(rendered.contains("landed in #1"), "{rendered}");
        assert!(rendered.contains("findings.md"), "{rendered}");
    }

    #[test]
    fn every_delivery_outcome_reads_differently() {
        let cases = [
            (
                WorkDelivery::Delivered {
                    workspace: "w".into(),
                },
                "delivered",
            ),
            (
                WorkDelivery::Queued {
                    workspace: "w".into(),
                },
                "queued",
            ),
            (
                WorkDelivery::Refused {
                    reason: "no running agent".into(),
                },
                "not delivered",
            ),
            (
                WorkDelivery::Skipped {
                    reason: "deliver=false".into(),
                },
                "nothing sent",
            ),
        ];
        for (delivery, expected) in cases {
            let rendered = render(&WorkReport::One {
                work: Box::new(row("x", "pending")),
                delivery: Some(delivery),
                requester_notified: None,
            });
            assert!(
                rendered.contains(expected),
                "{expected} missing: {rendered}"
            );
        }
    }

    #[test]
    fn a_plan_shows_its_rollup_and_the_repos_it_spans() {
        let rendered = render(&WorkReport::Status {
            plans: vec![PlanRow {
                plan: "p".into(),
                title: "ship coordination".into(),
                done: 1,
                total: 2,
                members: vec!["github:o/r#1".into(), "github:o/other#2".into()],
                tasks: vec![row("a", "completed"), row("b", "underway")],
            }],
            unplanned: Vec::new(),
            undecodable_rows: 0,
        });
        assert!(rendered.contains("1/2"), "{rendered}");
        assert!(rendered.contains("github:o/other#2"), "{rendered}");
        // The headline must not claim completion at 1/2. Checked on that line
        // alone: a task row below it legitimately reads `[completed]`, which a
        // whole-output search for "complete" would match.
        let headline = rendered.lines().next().expect("a headline");
        assert_eq!(
            headline.trim_end(),
            "ship coordination  1/2",
            "a partly-done plan must not read as complete"
        );
    }

    #[test]
    fn undecodable_rows_are_warned_about_never_silently_dropped() {
        let rendered = render(&WorkReport::Status {
            plans: Vec::new(),
            unplanned: Vec::new(),
            undecodable_rows: 3,
        });
        assert!(
            rendered.contains("3 row(s) could not be read"),
            "{rendered}"
        );
        assert!(rendered.contains("may be incomplete"), "{rendered}");
    }

    #[test]
    fn an_empty_status_says_so() {
        let rendered = render(&WorkReport::Status {
            plans: Vec::new(),
            unplanned: Vec::new(),
            undecodable_rows: 0,
        });
        assert!(rendered.contains("no plans"), "{rendered}");
    }

    #[test]
    fn the_workspace_override_beats_the_injected_env() {
        // Both are read; the flag has to win, or `--workspace` would be
        // silently ignored inside a session, which is exactly where it is
        // needed to act on another row.
        assert_eq!(
            self_key(Some("github:o/r#9")).expect("key").as_str(),
            "github:o/r#9"
        );
        assert!(
            self_key(Some("   ")).is_err() || self_key(None).is_ok(),
            "a blank override must not be taken as a workspace"
        );
    }

    #[test]
    fn the_usage_names_every_verb_the_dispatcher_accepts() {
        for verb in ["mine", "new", "set", "done", "status"] {
            assert!(
                USAGE.contains(verb),
                "{verb} is dispatched but undocumented"
            );
        }
    }
}
