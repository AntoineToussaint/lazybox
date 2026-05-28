//! `pilot slack prune` — archive stale per-(session, agent) Slack
//! channels.
//!
//! Walks `conversations.list`, classifies each channel via
//! [`pilot_slack::prune`], and either prints the plan (`--dry-run`)
//! or prompts the user before archiving. Lives in `pilot-tui` (not
//! the slack-provider crate) because it ties together three things:
//! the Slack HTTP client, the daemon's persistent store (to see
//! which terminals are still live), and stdin/stdout. The pure
//! classification logic stays in `pilot-slack` so it can be tested
//! without any of those.
//!
//! Acceptance for #6:
//! - `--dry-run` lists what would be archived
//! - Without `--dry-run`, prompts before archiving each batch
//! - Channels not matching pilot's naming pattern are skipped
//! - Channels for still-live terminals are skipped regardless of age

use std::collections::HashSet;
use std::io::{BufRead, Write};

use pilot_config::Config;
use pilot_core::Workspace;
use pilot_slack::api::{Client as SlackApi, channel_name_for_terminal, sluggify};
use pilot_slack::prune::{ChannelInfo, Filter, parse_duration, plan as build_plan};
use pilot_store::Store;

/// Outcome variant for the binary's exit code. `pilot slack prune`
/// exits 0 on `Done`, 1 on `Failed`, 2 on `BadArgs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruneOutcome {
    /// Plan computed, archive step finished (or skipped via --dry-run
    /// / user declined the prompt). Includes a count for the trailing
    /// summary line.
    Done { archived: usize, skipped: usize },
    /// Slack or store error. The wizard already printed the reason.
    Failed,
    /// Argument parse error. Prints usage and exits non-zero.
    BadArgs,
}

/// Parsed argv for `pilot slack prune`. Construction lives in the
/// `parse_args` helper below so it's unit-testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneArgs {
    pub dry_run: bool,
    pub assume_yes: bool,
    pub older_than: Option<String>,
    pub workspace: Option<String>,
    /// `--help` / `-h` — the caller prints usage and exits 0.
    pub show_help: bool,
}

/// Parse `pilot slack prune`'s flags. Returns `Err` with a usage
/// string for unrecognized input so the caller can print and exit 2.
pub fn parse_args(args: &[String]) -> Result<PruneArgs, String> {
    let mut out = PruneArgs::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--help" | "-h" => out.show_help = true,
            "--dry-run" => out.dry_run = true,
            "--yes" | "-y" => out.assume_yes = true,
            "--older-than" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "--older-than requires a value (e.g. `7d`)".to_string())?;
                out.older_than = Some(v.clone());
            }
            "--workspace" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "--workspace requires a value".to_string())?;
                out.workspace = Some(v.clone());
            }
            _ if a.starts_with("--older-than=") => {
                out.older_than = Some(a["--older-than=".len()..].to_string());
            }
            _ if a.starts_with("--workspace=") => {
                out.workspace = Some(a["--workspace=".len()..].to_string());
            }
            _ => return Err(format!("unknown argument: `{a}`")),
        }
        i += 1;
    }
    Ok(out)
}

/// Console-friendly summary of the plan. Returns a string instead of
/// printing so the tests can assert on the output without redirecting
/// stdout.
pub fn render_plan(
    archive: &[pilot_slack::prune::Classified],
    skipped_not_managed: usize,
    skipped_live: usize,
    skipped_filter: usize,
) -> String {
    let mut s = String::new();
    if archive.is_empty() {
        s.push_str("Nothing to archive.\n");
    } else {
        s.push_str(&format!("Would archive {} channel(s):\n", archive.len()));
        for c in archive {
            if let Some(p) = &c.parts {
                s.push_str(&format!(
                    "  #{}  (ws={} agent={})\n",
                    c.channel.name, p.workspace_slug, p.agent
                ));
            } else {
                s.push_str(&format!("  #{}\n", c.channel.name));
            }
        }
    }
    s.push_str(&format!(
        "Skipped: {} not pilot-managed, {} live session(s), {} filtered.\n",
        skipped_not_managed, skipped_live, skipped_filter
    ));
    s
}

/// Build the set of channel names belonging to live terminals so
/// `pilot_slack::prune::classify` can short-circuit them.
///
/// Walks every workspace in the store and every agent session
/// inside it, formatting the channel name with the same helper the
/// chat layer uses. Non-agent sessions (Shell, Compare, LogTail)
/// don't get slack channels in the first place (see
/// `chat.rs:TerminalSpawned` handler — it bails when the terminal
/// isn't `Agent`-kind) so they're skipped here too.
pub fn live_channel_names_from_store(store: &dyn Store, channel_prefix: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let workspaces = match store.list_workspaces() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("slack prune: list_workspaces failed: {e}");
            return out;
        }
    };
    for rec in workspaces {
        let Some(json) = rec.workspace_json.as_deref() else {
            continue;
        };
        let ws: Workspace = match serde_json::from_str(json) {
            Ok(w) => w,
            Err(e) => {
                tracing::debug!("slack prune: skip malformed workspace `{}`: {e}", rec.key);
                continue;
            }
        };
        for session in &ws.sessions {
            let agent_id = match &session.kind {
                pilot_core::SessionKind::Agent { agent_id } => agent_id.clone(),
                _ => continue,
            };
            let name = channel_name_for_terminal(
                ws.key.as_str(),
                &session.id.to_string(),
                &agent_id,
                channel_prefix,
            );
            out.insert(name);
        }
    }
    out
}

/// Stdin / stdout abstraction. Same shape as `slack_init::PromptIo`
/// but a separate trait so the two wizards don't share a brittle
/// import chain. The split keeps a future "rename prompt label"
/// change in init from accidentally breaking prune tests.
pub trait PrunePromptIo {
    fn prompt(&mut self, label: &str) -> std::io::Result<String>;
    fn print(&mut self, line: &str) -> std::io::Result<()>;
}

pub struct StdioPrompt;

impl PrunePromptIo for StdioPrompt {
    fn prompt(&mut self, label: &str) -> std::io::Result<String> {
        let mut out = std::io::stdout().lock();
        out.write_all(label.as_bytes())?;
        out.flush()?;
        drop(out);
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        Ok(line)
    }
    fn print(&mut self, line: &str) -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{line}")?;
        Ok(())
    }
}

/// Slack-side calls the prune wizard needs. Trait-shaped so tests
/// drive scripted fake-network responses without touching reqwest.
pub trait PruneSlack {
    /// Walk every channel via paginated `conversations.list`.
    fn list_all_channels<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<ChannelInfo>, String>> + Send + 'a>,
    >;
    /// Archive one channel by id. Returns the slack `error` code on
    /// failure so the caller can recognize `already_archived` /
    /// `channel_not_found`.
    fn archive<'a>(
        &'a self,
        channel_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
}

/// Live wiring over `pilot_slack::api::Client`.
pub struct LiveSlack {
    api: SlackApi,
}

impl LiveSlack {
    pub fn new(bot_token: String) -> Self {
        Self {
            api: SlackApi::new(bot_token),
        }
    }
}

impl PruneSlack for LiveSlack {
    fn list_all_channels<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<ChannelInfo>, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            // Pages of 200 — Slack caps page size at 1000 but 200 is a
            // friendlier-on-rate-limit default that still finishes a
            // 9000-channel workspace in well under a minute.
            let mut out = Vec::new();
            let mut cursor = String::new();
            loop {
                let resp = self
                    .api
                    .conversations_list_page(200, &cursor, false)
                    .await
                    .map_err(|e| format!("conversations.list: {e}"))?;
                for c in resp.channels {
                    out.push(ChannelInfo {
                        id: c.id,
                        name: c.name,
                        created_unix: c.created,
                    });
                }
                if resp.response_metadata.next_cursor.is_empty() {
                    break;
                }
                cursor = resp.response_metadata.next_cursor;
            }
            Ok(out)
        })
    }

    fn archive<'a>(
        &'a self,
        channel_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            match self.api.conversations_archive(channel_id).await {
                Ok(()) => Ok(()),
                Err(pilot_slack::SlackError::Api(ref e)) if e == "already_archived" => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        })
    }
}

/// Real-mode entry point. Reads tokens via the same env-wins-over-
/// YAML rule as `pilot slack doctor`, opens the daemon's persistent
/// store read-only, runs the wizard.
pub async fn run(args: &[String]) -> std::io::Result<PruneOutcome> {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(msg) => {
            println!("error: {msg}");
            print_usage();
            return Ok(PruneOutcome::BadArgs);
        }
    };
    if parsed.show_help {
        print_usage();
        return Ok(PruneOutcome::Done {
            archived: 0,
            skipped: 0,
        });
    }
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            println!("✗ load ~/.pilot/config.yaml: {e}");
            return Ok(PruneOutcome::Failed);
        }
    };
    let Some(bot_token) = resolve_token(cfg.slack.bot_token.as_deref(), "SLACK_BOT_TOKEN") else {
        println!("✗ bot_token not set (config.slack.bot_token or $SLACK_BOT_TOKEN)");
        println!("  Run `pilot slack init` to configure interactively.");
        return Ok(PruneOutcome::Failed);
    };
    let Some(store) = pilot_server::open_store() else {
        println!("✗ open ~/.pilot/v2/state.db — can't tell which sessions are live.");
        return Ok(PruneOutcome::Failed);
    };
    let slack = LiveSlack::new(bot_token);
    let mut io = StdioPrompt;
    run_with(&parsed, &slack, &mut io, &*store, &cfg.slack.channel_prefix).await
}

/// Core wizard, parameterized over IO + Slack probe + store. The
/// `now_unix` value is read from the system clock here; tests supply
/// their own to `run_with_clock` below.
pub async fn run_with<S: PruneSlack + ?Sized, I: PrunePromptIo + ?Sized>(
    args: &PruneArgs,
    slack: &S,
    io: &mut I,
    store: &dyn Store,
    channel_prefix: &str,
) -> std::io::Result<PruneOutcome> {
    let now = chrono::Utc::now().timestamp();
    run_with_clock(args, slack, io, store, channel_prefix, now).await
}

pub async fn run_with_clock<S: PruneSlack + ?Sized, I: PrunePromptIo + ?Sized>(
    args: &PruneArgs,
    slack: &S,
    io: &mut I,
    store: &dyn Store,
    channel_prefix: &str,
    now_unix: i64,
) -> std::io::Result<PruneOutcome> {
    io.print("pilot slack prune — archive stale per-(session, agent) channels")?;
    io.print("")?;

    // ── Translate args → Filter ─────────────────────────────────────
    let older_than_secs = match &args.older_than {
        Some(s) => match parse_duration(s) {
            Ok(secs) => Some(secs),
            Err(msg) => {
                io.print(&format!("✗ --older-than: {msg}"))?;
                return Ok(PruneOutcome::BadArgs);
            }
        },
        None => None,
    };
    let workspace_slug = args.workspace.as_deref().map(sluggify);
    let filter = Filter {
        older_than_secs,
        workspace_slug,
    };

    // ── Pull listing + live set ─────────────────────────────────────
    io.print("• Listing Slack channels…")?;
    let channels = match slack.list_all_channels().await {
        Ok(v) => v,
        Err(e) => {
            io.print(&format!("✗ {e}"))?;
            return Ok(PruneOutcome::Failed);
        }
    };
    io.print(&format!("• Found {} channel(s)", channels.len()))?;

    let live = live_channel_names_from_store(store, channel_prefix);
    io.print(&format!("• {} live pilot terminal(s) in store", live.len()))?;

    // ── Plan ────────────────────────────────────────────────────────
    let plan = build_plan(&channels, channel_prefix, &live, &filter, now_unix);
    io.print("")?;
    io.print(&render_plan(
        &plan.archive,
        plan.skipped_not_managed,
        plan.skipped_live,
        plan.skipped_filter,
    ))?;

    if args.dry_run {
        return Ok(PruneOutcome::Done {
            archived: 0,
            skipped: plan.skipped_not_managed + plan.skipped_live + plan.skipped_filter,
        });
    }

    if plan.archive.is_empty() {
        return Ok(PruneOutcome::Done {
            archived: 0,
            skipped: plan.skipped_not_managed + plan.skipped_live + plan.skipped_filter,
        });
    }

    // ── Confirm ─────────────────────────────────────────────────────
    if !args.assume_yes {
        let answer = io.prompt(&format!(
            "Archive {} channel(s)? [y/N]: ",
            plan.archive.len()
        ))?;
        let answer = answer.trim().to_lowercase();
        if !(answer == "y" || answer == "yes") {
            io.print("Aborted.")?;
            return Ok(PruneOutcome::Done {
                archived: 0,
                skipped: plan.skipped_not_managed + plan.skipped_live + plan.skipped_filter,
            });
        }
    }

    // ── Archive ─────────────────────────────────────────────────────
    let mut archived = 0;
    let mut errors = 0;
    for c in &plan.archive {
        match slack.archive(&c.channel.id).await {
            Ok(()) => {
                archived += 1;
                io.print(&format!("✓ archived #{}", c.channel.name))?;
            }
            Err(e) => {
                errors += 1;
                io.print(&format!("✗ archive #{}: {e}", c.channel.name))?;
            }
        }
    }
    if errors > 0 {
        io.print(&format!(
            "Done with {archived} archived, {errors} error(s)."
        ))?;
        return Ok(PruneOutcome::Failed);
    }
    io.print(&format!("Done. Archived {archived} channel(s)."))?;
    Ok(PruneOutcome::Done {
        archived,
        skipped: plan.skipped_not_managed + plan.skipped_live + plan.skipped_filter,
    })
}

fn print_usage() {
    println!("usage: pilot slack prune [--dry-run] [--yes] [--older-than DUR] [--workspace KEY]");
    println!();
    println!("  --dry-run         List candidates without archiving");
    println!("  --yes, -y         Don't prompt before archiving");
    println!("  --older-than DUR  Only consider channels older than DUR (e.g. 7d, 12h)");
    println!("  --workspace KEY   Only consider channels whose workspace contains KEY");
}

fn resolve_token(yaml: Option<&str>, env_key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(env_key) {
        if !v.trim().is_empty() {
            return Some(v);
        }
    }
    yaml.filter(|s| !s.trim().is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilot_core::{SessionKind, WorkspaceKey, WorkspaceSession as Session};
    use pilot_store::mock::MemoryStore;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // ── parse_args ──────────────────────────────────────────────

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_args_defaults_to_false_everywhere() {
        let a = parse_args(&[]).unwrap();
        assert!(!a.dry_run);
        assert!(!a.assume_yes);
        assert!(a.older_than.is_none());
        assert!(a.workspace.is_none());
    }

    #[test]
    fn parse_args_accepts_long_form() {
        let a = parse_args(&argv(&[
            "--dry-run",
            "--older-than",
            "7d",
            "--workspace",
            "acme/widget",
        ]))
        .unwrap();
        assert!(a.dry_run);
        assert_eq!(a.older_than.as_deref(), Some("7d"));
        assert_eq!(a.workspace.as_deref(), Some("acme/widget"));
    }

    #[test]
    fn parse_args_accepts_equals_form() {
        let a = parse_args(&argv(&["--older-than=12h", "--workspace=foo"])).unwrap();
        assert_eq!(a.older_than.as_deref(), Some("12h"));
        assert_eq!(a.workspace.as_deref(), Some("foo"));
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        assert!(parse_args(&argv(&["--nope"])).is_err());
    }

    #[test]
    fn parse_args_rejects_value_flag_without_value() {
        assert!(parse_args(&argv(&["--older-than"])).is_err());
        assert!(parse_args(&argv(&["--workspace"])).is_err());
    }

    // ── live_channel_names_from_store ───────────────────────────

    fn seed_store_with_workspace(store: &MemoryStore, key: &str, sessions: Vec<Session>) {
        let mut ws = Workspace::empty(WorkspaceKey::new(key), "main", chrono::Utc::now());
        for s in sessions {
            ws.add_session(s);
        }
        let rec = pilot_store::WorkspaceRecord {
            key: key.to_string(),
            created_at: chrono::Utc::now(),
            workspace_json: Some(serde_json::to_string(&ws).unwrap()),
        };
        store.save_workspace(&rec).unwrap();
    }

    fn agent_session(workspace: &str, agent: &str) -> Session {
        Session::new(
            WorkspaceKey::new(workspace),
            SessionKind::Agent {
                agent_id: agent.into(),
            },
            std::path::PathBuf::from("/tmp/fake"),
            chrono::Utc::now(),
        )
    }

    fn shell_session(workspace: &str) -> Session {
        Session::new(
            WorkspaceKey::new(workspace),
            SessionKind::Shell,
            std::path::PathBuf::from("/tmp/fake"),
            chrono::Utc::now(),
        )
    }

    #[test]
    fn live_channel_names_includes_only_agent_sessions() {
        let store = MemoryStore::new();
        let a = agent_session("github-acme-widget-186", "claude");
        let s = shell_session("github-acme-widget-186");
        seed_store_with_workspace(&store, "github-acme-widget-186", vec![a.clone(), s]);

        let live = live_channel_names_from_store(&store, "");
        let expected =
            channel_name_for_terminal("github-acme-widget-186", &a.id.to_string(), "claude", "");
        assert!(live.contains(&expected));
        // Shell sessions don't get slack channels, so no entry for the
        // shell session's id.
        assert_eq!(live.len(), 1);
    }

    #[test]
    fn live_channel_names_honors_prefix() {
        let store = MemoryStore::new();
        let a = agent_session("github-foo-1", "codex");
        seed_store_with_workspace(&store, "github-foo-1", vec![a.clone()]);

        let live = live_channel_names_from_store(&store, "pr-");
        let expected = channel_name_for_terminal("github-foo-1", &a.id.to_string(), "codex", "pr-");
        assert!(live.contains(&expected));
    }

    // ── run_with_clock ──────────────────────────────────────────

    struct ScriptedIo {
        answers: VecDeque<String>,
        out: Vec<String>,
    }

    impl ScriptedIo {
        fn new(answers: &[&str]) -> Self {
            Self {
                answers: answers.iter().map(|s| s.to_string()).collect(),
                out: Vec::new(),
            }
        }
        fn joined(&self) -> String {
            self.out.join("\n")
        }
    }

    impl PrunePromptIo for ScriptedIo {
        fn prompt(&mut self, label: &str) -> std::io::Result<String> {
            self.out.push(label.trim_end().to_string());
            Ok(self
                .answers
                .pop_front()
                .expect("test ran out of scripted answers"))
        }
        fn print(&mut self, line: &str) -> std::io::Result<()> {
            self.out.push(line.to_string());
            Ok(())
        }
    }

    struct FakeSlack {
        channels: Vec<ChannelInfo>,
        archived: Mutex<Vec<String>>,
        fail_archive: Option<String>,
    }

    impl FakeSlack {
        fn new(channels: Vec<ChannelInfo>) -> Self {
            Self {
                channels,
                archived: Mutex::new(Vec::new()),
                fail_archive: None,
            }
        }
    }

    impl PruneSlack for FakeSlack {
        fn list_all_channels<'a>(
            &'a self,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<ChannelInfo>, String>> + Send + 'a>,
        > {
            let chans = self.channels.clone();
            Box::pin(async move { Ok(chans) })
        }
        fn archive<'a>(
            &'a self,
            channel_id: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async move {
                if let Some(err) = &self.fail_archive {
                    return Err(err.clone());
                }
                self.archived.lock().unwrap().push(channel_id.to_string());
                Ok(())
            })
        }
    }

    fn ci(name: &str, created: i64) -> ChannelInfo {
        ChannelInfo {
            id: format!("C-{name}"),
            name: name.into(),
            created_unix: created,
        }
    }

    #[tokio::test]
    async fn dry_run_lists_without_archiving() {
        let slack = FakeSlack::new(vec![
            ci("general", 0),            // unmanaged
            ci("ws-deadbeef-claude", 0), // stale
        ]);
        let store = MemoryStore::new();
        let mut io = ScriptedIo::new(&[]);
        let args = PruneArgs {
            dry_run: true,
            ..Default::default()
        };
        let outcome = run_with_clock(&args, &slack, &mut io, &store, "", 1_000_000)
            .await
            .unwrap();
        // Dry-run never archives.
        assert!(matches!(outcome, PruneOutcome::Done { archived: 0, .. }));
        assert!(slack.archived.lock().unwrap().is_empty());
        let log = io.joined();
        assert!(log.contains("Would archive 1 channel"));
        assert!(log.contains("ws-deadbeef-claude"));
    }

    #[tokio::test]
    async fn prompts_before_archiving_and_archives_on_yes() {
        let slack = FakeSlack::new(vec![ci("ws-deadbeef-claude", 0)]);
        let store = MemoryStore::new();
        let mut io = ScriptedIo::new(&["y"]);
        let args = PruneArgs::default();
        let outcome = run_with_clock(&args, &slack, &mut io, &store, "", 1_000_000)
            .await
            .unwrap();
        assert!(matches!(outcome, PruneOutcome::Done { archived: 1, .. }));
        let archived = slack.archived.lock().unwrap().clone();
        assert_eq!(archived, vec!["C-ws-deadbeef-claude".to_string()]);
    }

    #[tokio::test]
    async fn declining_prompt_archives_nothing() {
        let slack = FakeSlack::new(vec![ci("ws-deadbeef-claude", 0)]);
        let store = MemoryStore::new();
        let mut io = ScriptedIo::new(&["n"]);
        let args = PruneArgs::default();
        let outcome = run_with_clock(&args, &slack, &mut io, &store, "", 1_000_000)
            .await
            .unwrap();
        assert!(matches!(outcome, PruneOutcome::Done { archived: 0, .. }));
        assert!(slack.archived.lock().unwrap().is_empty());
        assert!(io.joined().contains("Aborted"));
    }

    #[tokio::test]
    async fn assume_yes_skips_prompt() {
        let slack = FakeSlack::new(vec![ci("ws-deadbeef-claude", 0)]);
        let store = MemoryStore::new();
        // No scripted answers — must not prompt.
        let mut io = ScriptedIo::new(&[]);
        let args = PruneArgs {
            assume_yes: true,
            ..Default::default()
        };
        let outcome = run_with_clock(&args, &slack, &mut io, &store, "", 1_000_000)
            .await
            .unwrap();
        assert!(matches!(outcome, PruneOutcome::Done { archived: 1, .. }));
    }

    #[tokio::test]
    async fn live_session_channel_is_skipped() {
        // Workspace has one live agent session. Its channel name
        // must not be archived even though it matches the pattern.
        let store = MemoryStore::new();
        let a = agent_session("ws", "claude");
        let live_name = channel_name_for_terminal("ws", &a.id.to_string(), "claude", "");
        seed_store_with_workspace(&store, "ws", vec![a]);

        let slack = FakeSlack::new(vec![ChannelInfo {
            id: format!("C-{live_name}"),
            name: live_name.clone(),
            created_unix: 0,
        }]);
        let mut io = ScriptedIo::new(&[]);
        let args = PruneArgs {
            dry_run: true,
            ..Default::default()
        };
        let outcome = run_with_clock(&args, &slack, &mut io, &store, "", 1_000_000)
            .await
            .unwrap();
        assert!(matches!(outcome, PruneOutcome::Done { archived: 0, .. }));
        let log = io.joined();
        assert!(log.contains("Nothing to archive"));
        assert!(log.contains("1 live session"));
    }

    #[tokio::test]
    async fn unmanaged_channels_are_skipped() {
        // A channel like #general must never appear in the prune
        // candidate list even with no filters.
        let slack = FakeSlack::new(vec![ci("general", 0), ci("random", 0)]);
        let store = MemoryStore::new();
        let mut io = ScriptedIo::new(&[]);
        let args = PruneArgs {
            dry_run: true,
            ..Default::default()
        };
        let outcome = run_with_clock(&args, &slack, &mut io, &store, "", 1_000_000)
            .await
            .unwrap();
        assert!(matches!(outcome, PruneOutcome::Done { archived: 0, .. }));
        let log = io.joined();
        assert!(log.contains("Nothing to archive"));
        assert!(log.contains("2 not pilot-managed"));
    }

    #[tokio::test]
    async fn older_than_filter_excludes_young_channels() {
        let now = 1_000_000;
        let slack = FakeSlack::new(vec![
            ci("ws-deadbeef-claude", now - 100),    // 100s old
            ci("ws-cafef00d-codex", now - 800_000), // ~9d old
        ]);
        let store = MemoryStore::new();
        let mut io = ScriptedIo::new(&["y"]);
        let args = PruneArgs {
            older_than: Some("7d".into()),
            ..Default::default()
        };
        let outcome = run_with_clock(&args, &slack, &mut io, &store, "", now)
            .await
            .unwrap();
        assert!(matches!(outcome, PruneOutcome::Done { archived: 1, .. }));
        let archived = slack.archived.lock().unwrap().clone();
        assert_eq!(archived, vec!["C-ws-cafef00d-codex".to_string()]);
    }

    #[tokio::test]
    async fn workspace_filter_uses_sluggified_input() {
        // User types `acme/widget` — gets slugged to `acme-widget`
        // before matching, so the github-acme-widget channel
        // matches and a globex one does not.
        let slack = FakeSlack::new(vec![
            ci("github-acme-widget-186-deadbeef-claude", 0),
            ci("github-globex-99-cafef00d-claude", 0),
        ]);
        let store = MemoryStore::new();
        let mut io = ScriptedIo::new(&[]);
        let args = PruneArgs {
            dry_run: true,
            workspace: Some("acme/widget".into()),
            ..Default::default()
        };
        let outcome = run_with_clock(&args, &slack, &mut io, &store, "", 1_000_000)
            .await
            .unwrap();
        assert!(matches!(outcome, PruneOutcome::Done { .. }));
        let log = io.joined();
        assert!(log.contains("Would archive 1 channel"));
        assert!(log.contains("github-acme-widget-186-deadbeef-claude"));
    }

    #[tokio::test]
    async fn invalid_older_than_is_a_bad_args_failure() {
        let slack = FakeSlack::new(vec![]);
        let store = MemoryStore::new();
        let mut io = ScriptedIo::new(&[]);
        let args = PruneArgs {
            older_than: Some("forever".into()),
            ..Default::default()
        };
        let outcome = run_with_clock(&args, &slack, &mut io, &store, "", 1_000_000)
            .await
            .unwrap();
        assert_eq!(outcome, PruneOutcome::BadArgs);
    }
}
