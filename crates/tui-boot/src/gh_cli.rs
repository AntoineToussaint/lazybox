//! `lazybox gh` — the shim lazybox puts on a session's PATH as `gh` (#1801).
//!
//! A session that reaches for `gh` gets this instead of the real binary. It
//! runs real `gh` for everything; what it adds is three round trips to the
//! daemon around that call:
//!
//! - before: [`Command::GhAdmit`], which may answer an identical read from the
//!   fleet's cache ([`GhVerdict::Cached`]) or pace this session
//!   ([`GhVerdict::Throttle`]);
//! - after: [`Command::GhCompleted`], which files the output for the next
//!   session and tells the daemon what a mutation changed, so the row flips
//!   without a sweep and without budget.
//!
//! Everything here degrades toward running `gh` unchanged. No daemon, a
//! refused connection, a slow answer, an unrecognised subcommand, `gh` itself
//! missing — each one falls through to exactly what the agent typed. The shim
//! is a coordination point, never a gate on the session's work: the one thing
//! it may not do is make `gh` stop working.

use lazybox_ipc::gh_shim::{
    GhCallKind, GhChangeKind, GhRecordChange, GhReply, GhVerdict, SHIM_DIR_ENV, SHIM_OPT_OUT_ENV,
};
use lazybox_ipc::{Command, Event};
use lazybox_server::lifecycle;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// How long to wait for the daemon's admission. Generous — the daemon may be
/// mid-sweep — but bounded, because expiring means running `gh` unadmitted,
/// which is strictly better than stalling the agent behind a wedged daemon.
const ADMIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Total time the shim will spend waiting out throttles before giving up and
/// telling the agent why. A quota throttle resolves in seconds, so this is
/// really the bound on a *reserve* throttle, which can otherwise be "come back
/// at the top of the hour" — a wait no agent should sit through silently.
const MAX_TOTAL_THROTTLE: Duration = Duration::from_secs(90);

/// Largest stdout the shim files for reuse. `Command::GhCompleted` travels the
/// daemon's 256 KiB command-frame channel, and a cache is worth having only
/// for answers small enough to be cheap to hold.
const MAX_CACHEABLE_STDOUT: usize = 128 * 1024;

/// Run one `gh` invocation. Never returns: the process exits with `gh`'s own
/// status so a caller cannot tell the shim from the real thing.
pub async fn gh_subcommand(args: &[String]) -> ! {
    // The spawn sets this; falling back to the installed location matters for
    // a hand-run `lazybox gh`, whose PATH may still hold the shim — resolving
    // `gh` back to ourselves would spawn this process forever.
    let shim_dir = std::env::var_os(SHIM_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(lazybox_server::gh_shim::shim_dir);
    let Some(real) = lazybox_server::gh_shim::real_gh(&shim_dir, None) else {
        eprintln!("lazybox gh: no `gh` on PATH to run");
        std::process::exit(127);
    };

    if opted_out() {
        exec_inherit(&real, args);
    }

    let mut call = classify(args);
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        // Capturing stdout to file it for the fleet costs `gh` its terminal:
        // no colour, no pager. An agent's stdout is already a pipe, so this
        // only ever opts out the human typing in a shell pane — who is not
        // the fan-out this cache exists for.
        call.read_key = None;
    }
    if call.kind.is_none() {
        // Local-only: `gh auth status`, `gh --version`. Nothing to admit,
        // nothing to cache, and no reason to make it depend on the daemon.
        exec_inherit(&real, args);
    }
    let kind = call.kind.unwrap_or(GhCallKind::Other);

    let mut client = connect().await;
    if let Some(client) = client.as_mut() {
        match admit(client, kind, call.read_key.as_deref()).await {
            Admission::Cached(stdout) => {
                // Flushed explicitly: `process::exit` runs no destructors, and
                // a block-buffered stdout (which is every piped agent call)
                // would otherwise drop the answer on the floor.
                use std::io::Write as _;
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(stdout.as_bytes());
                let _ = out.flush();
                std::process::exit(0);
            }
            Admission::Refused(reason) => {
                eprintln!(
                    "lazybox gh: {reason}. Re-run later, or bypass lazybox with `gh.real` / \
                     {SHIM_OPT_OUT_ENV}=0."
                );
                std::process::exit(1);
            }
            Admission::Allowed => {}
        }
    }

    // Only a read is captured. A mutation and an unrecognised invocation keep
    // `gh`'s stdio exactly as the agent would have had it — prompts, pagers,
    // progress and colour included — because the shim's promise is that an
    // invocation it does not serve is an invocation it does not change.
    let (code, stdout) = if call.read_key.is_some() {
        run_capturing(&real, args)
    } else {
        (run_inherit(&real, args), None)
    };

    if let Some(client) = client.as_mut() {
        report(client, code, &call, stdout.as_deref()).await;
    }
    std::process::exit(code);
}

/// `LAZYBOX_GH_SHIM` set to a falsey value — the documented way to take
/// lazybox out of the path entirely for one session or one command.
fn opted_out() -> bool {
    std::env::var(SHIM_OPT_OUT_ENV)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(false)
}

/// What the shim made of one invocation.
#[derive(Debug, Default, PartialEq, Eq)]
struct Call {
    /// `None` for an invocation that never reaches GitHub, which the daemon is
    /// not asked about at all.
    kind: Option<GhCallKind>,
    /// Canonical spelling of a cacheable read, or `None` when the answer must
    /// not be replayed for anyone else.
    read_key: Option<String>,
    change: Option<GhRecordChange>,
}

/// Subcommands that never touch the GitHub API.
const LOCAL: &[&str] = &[
    "auth",
    "alias",
    "completion",
    "config",
    "extension",
    "extensions",
    "help",
    "version",
];

/// `<group> <verb>` pairs the shim recognises as reads whose stdout is safe to
/// serve to an identical invocation from another session.
const READS: &[(&str, &str)] = &[
    ("issue", "view"),
    ("issue", "list"),
    ("issue", "status"),
    ("pr", "view"),
    ("pr", "list"),
    ("pr", "status"),
    ("pr", "diff"),
    ("pr", "checks"),
    ("release", "view"),
    ("release", "list"),
    ("repo", "view"),
    ("run", "view"),
    ("run", "list"),
    ("label", "list"),
    ("workflow", "list"),
];

/// `<group> <verb>` pairs the shim recognises as changing a record, and what
/// the daemon can conclude about the record's state from each.
const MUTATIONS: &[(&str, &str, GhChangeKind)] = &[
    ("issue", "close", GhChangeKind::Closed),
    ("issue", "reopen", GhChangeKind::Reopened),
    ("issue", "comment", GhChangeKind::Touched),
    ("issue", "edit", GhChangeKind::Touched),
    ("issue", "create", GhChangeKind::Touched),
    ("issue", "transfer", GhChangeKind::Touched),
    ("pr", "merge", GhChangeKind::Merged),
    ("pr", "close", GhChangeKind::Closed),
    ("pr", "reopen", GhChangeKind::Reopened),
    ("pr", "comment", GhChangeKind::Touched),
    ("pr", "edit", GhChangeKind::Touched),
    ("pr", "create", GhChangeKind::Touched),
    ("pr", "ready", GhChangeKind::Touched),
    ("pr", "review", GhChangeKind::Touched),
];

/// The invocation's operands — `["issue", "close", "12"]` — with flags *and
/// their values* removed.
///
/// Dropping a token because the one before it was a flag is what keeps a
/// flag's value from being read as the record: in `gh pr comment --body 42 7`
/// the operand is `7`, and treating `42` as the record would write a state flip
/// onto a different issue. A boolean flag immediately before the record
/// (`gh pr close --delete-branch 7`) costs the shim the reference instead,
/// which is the safe direction — the poll sweep still reconciles it.
fn operands(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut after_flag = false;
    for arg in args {
        let is_flag = arg.starts_with('-');
        if !is_flag && !after_flag {
            out.push(arg.as_str());
        }
        after_flag = is_flag && !arg.contains('=');
    }
    out
}

fn classify(args: &[String]) -> Call {
    let positional = operands(args);
    let Some(&group) = positional.first() else {
        // Bare `gh` prints its own help.
        return Call::default();
    };
    if LOCAL.contains(&group) {
        return Call::default();
    }
    let verb = positional.get(1).copied().unwrap_or("");

    if let Some((_, _, kind)) = MUTATIONS.iter().find(|(g, v, _)| *g == group && *v == verb) {
        return Call {
            kind: Some(GhCallKind::Mutation),
            read_key: None,
            change: change_for(args, &positional, *kind),
        };
    }

    let cacheable = READS.iter().any(|(g, v)| *g == group && *v == verb)
        || group == "search"
        || (group == "api" && is_api_read(args));
    if !cacheable {
        return Call {
            kind: Some(GhCallKind::Other),
            read_key: None,
            change: None,
        };
    }
    Call {
        kind: Some(GhCallKind::Read),
        read_key: read_key(args, &positional, group),
        change: None,
    }
}

/// Whether a `gh api` invocation only reads. `gh` itself infers POST from any
/// field flag, so the absence of those plus a GET (or absent) `--method` is
/// exactly the condition under which the call is safe to replay.
fn is_api_read(args: &[String]) -> bool {
    let mut expect_method = false;
    for arg in args {
        if expect_method {
            if !arg.eq_ignore_ascii_case("GET") {
                return false;
            }
            expect_method = false;
            continue;
        }
        match arg.as_str() {
            "-X" | "--method" => expect_method = true,
            "-f" | "-F" | "--field" | "--raw-field" | "--input" => return false,
            other => {
                if let Some(value) = other
                    .strip_prefix("--method=")
                    .or_else(|| other.strip_prefix("-X="))
                    && !value.eq_ignore_ascii_case("GET")
                {
                    return false;
                }
                if other.starts_with("-f") && other.len() > 2 {
                    return false;
                }
            }
        }
    }
    true
}

/// The cache key: the repo scope plus the argv, so two sessions asking the
/// same question of the same repo share one upstream call and nothing else
/// can collide.
///
/// `None` means "do not cache". That is the answer whenever the scope cannot
/// be named unambiguously — several git remotes and no explicit `--repo` — so
/// a key can never silently span two repositories.
fn read_key(args: &[String], positional: &[&str], group: &str) -> Option<String> {
    let scope = repo_scope(args)?;
    let mut key = String::from(&scope);
    // A `pr` read with no explicit target resolves against the current
    // branch, so the branch is part of the question being asked.
    if group == "pr" && !positional.get(2).is_some_and(|arg| looks_like_target(arg)) {
        key.push('\u{1f}');
        key.push_str(&current_branch().unwrap_or_default());
    }
    for arg in args {
        key.push('\u{1f}');
        key.push_str(arg);
    }
    Some(key)
}

fn looks_like_target(arg: &str) -> bool {
    arg.parse::<u64>().is_ok() || arg.starts_with("http://") || arg.starts_with("https://")
}

/// The value of the first of `flags` present in `args`, in either the
/// `--flag value` or `--flag=value` spelling.
fn flag_value(args: &[String], flags: &[&str]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if flags.contains(&arg.as_str()) {
            return iter.next().cloned().filter(|value| !value.is_empty());
        }
        for flag in flags {
            if let Some(value) = arg.strip_prefix(&format!("{flag}="))
                && !value.is_empty()
            {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// `owner/repo` for the invocation, resolved without spending a GitHub call.
///
/// `GH_REPO` and `--repo` are what `gh` itself obeys first. Otherwise the
/// worktree's single git remote names the repo; **several** remotes mean the
/// shim cannot know which one `gh` would pick (`gh repo set-default` is not
/// readable from here), and guessing would let two repositories share a cache
/// key — so that case resolves to `None` and simply is not cached.
fn repo_scope(args: &[String]) -> Option<String> {
    if let Some(explicit) = flag_value(args, &["-R", "--repo"]) {
        return Some(explicit);
    }

    if let Ok(env) = std::env::var("GH_REPO")
        && !env.trim().is_empty()
    {
        return Some(env);
    }
    let remotes = git(&["remote"])?;
    let mut names = remotes.lines().filter(|line| !line.trim().is_empty());
    let only = names.next()?;
    if names.next().is_some() {
        return None;
    }
    let url = git(&["remote", "get-url", only])?;
    Some(normalize_remote(url.trim()))
}

/// `owner/repo` from a remote URL in any of git's spellings.
fn normalize_remote(url: &str) -> String {
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    let tail = trimmed
        .rsplit_once(':')
        .map(|(_, tail)| tail)
        .filter(|tail| !tail.starts_with("//"))
        .unwrap_or(trimmed);
    let parts: Vec<&str> = tail.trim_start_matches('/').split('/').collect();
    if parts.len() >= 2 {
        return format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1]);
    }
    trimmed.to_string()
}

fn current_branch() -> Option<String> {
    git(&["rev-parse", "--abbrev-ref", "HEAD"]).map(|branch| branch.trim().to_string())
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The record a mutation names, so the daemon can find the row to update.
/// A record the shim cannot name is reported as no change at all rather than
/// as a guess — the poll sweep is the backstop for exactly that case.
fn change_for(args: &[String], positional: &[&str], kind: GhChangeKind) -> Option<GhRecordChange> {
    // Strictly the argument in the position `gh` reads the record from. A
    // scan for "the first thing that looks like a number" would happily pick
    // up a flag's value (`gh pr comment --body 42 7`) and write a state flip
    // onto the wrong row. Failing to name the record is safe — the poll sweep
    // is the backstop — so this fails closed on any shape it does not know.
    let reference = positional
        .get(2)
        .filter(|arg| looks_like_target(arg))
        .map(|arg| (*arg).to_string())?;
    Some(GhRecordChange {
        reference,
        repo: repo_scope(args),
        kind,
    })
}

enum Admission {
    Allowed,
    Cached(String),
    Refused(String),
}

async fn connect() -> Option<lazybox_ipc::Client> {
    let (client, _peer) = lazybox_ipc::socket::connect(&lifecycle::socket_path())
        .await
        .ok()?;
    Some(client)
}

async fn admit(
    client: &mut lazybox_ipc::Client,
    kind: GhCallKind,
    read_key: Option<&str>,
) -> Admission {
    let session_key = session_key();
    let deadline = tokio::time::Instant::now() + MAX_TOTAL_THROTTLE;
    loop {
        let client_request_id = uuid::Uuid::new_v4().to_string();
        if client
            .send(Command::GhAdmit {
                session_key: session_key.clone(),
                kind,
                read_key: read_key.map(str::to_string),
                client_request_id: client_request_id.clone(),
            })
            .is_err()
        {
            return Admission::Allowed;
        }
        match await_reply(client, &client_request_id).await {
            Some(GhReply::Admission(GhVerdict::Allow)) | Some(GhReply::Recorded) | None => {
                return Admission::Allowed;
            }
            Some(GhReply::Admission(GhVerdict::Cached { stdout })) => {
                return Admission::Cached(stdout);
            }
            Some(GhReply::Admission(GhVerdict::Throttle { wait_secs, reason })) => {
                let wait = Duration::from_secs(wait_secs);
                if tokio::time::Instant::now() + wait > deadline {
                    return Admission::Refused(reason);
                }
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// The daemon's correlated answer, or `None` when it did not answer in time —
/// which the caller treats as permission, never as refusal.
async fn await_reply(client: &mut lazybox_ipc::Client, client_request_id: &str) -> Option<GhReply> {
    let deadline = tokio::time::Instant::now() + ADMIT_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, client.recv()).await {
            Ok(Some(Event::GhShimReply {
                client_request_id: id,
                reply,
            })) if id == client_request_id => return Some(reply),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return None,
        }
    }
}

/// File the outcome with the daemon. Best-effort by construction: `gh` has
/// already run and its output is already on the terminal, so a send that fails
/// costs the fleet a cache entry, never the agent its command.
async fn report(client: &mut lazybox_ipc::Client, code: i32, call: &Call, stdout: Option<&str>) {
    if code != 0 {
        // A failed read must not be filed as the answer, and a failed mutation
        // changed nothing to report.
        return;
    }
    let cacheable = stdout
        .filter(|out| out.len() <= MAX_CACHEABLE_STDOUT)
        .map(str::to_string);
    if cacheable.is_none() && call.change.is_none() {
        return;
    }
    let client_request_id = uuid::Uuid::new_v4().to_string();
    if client
        .send(Command::GhCompleted {
            session_key: session_key(),
            read_key: cacheable.as_ref().and(call.read_key.clone()),
            stdout: cacheable,
            change: call.change.clone(),
            client_request_id: client_request_id.clone(),
        })
        .is_err()
    {
        return;
    }
    // Waiting on the ack is what makes the report reliable: the process exits
    // as soon as this returns, and the socket's writer runs in a background
    // task that process teardown does not drain.
    await_reply(client, &client_request_id).await;
}

/// This session's key, when `gh` is running inside one.
fn session_key() -> Option<lazybox_core::SessionKey> {
    std::env::var("LAZYBOX_SESSION_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
        .map(lazybox_core::SessionKey::from)
}

fn run_inherit(real: &Path, args: &[String]) -> i32 {
    std::process::Command::new(real)
        .args(args)
        .status()
        .map(|status| status.code().unwrap_or(1))
        .unwrap_or_else(|error| {
            eprintln!("lazybox gh: could not run {}: {error}", real.display());
            127
        })
}

/// Run `gh` with stdout captured so it can be filed for the fleet, then print
/// it verbatim. stderr stays inherited, so `gh`'s own diagnostics reach the
/// agent as they always did.
fn run_capturing(real: &Path, args: &[String]) -> (i32, Option<String>) {
    let output = std::process::Command::new(real)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output();
    match output {
        Ok(output) => {
            let code = output.status.code().unwrap_or(1);
            // Written as bytes, so `gh pr diff` on a binary patch reaches the
            // agent byte-for-byte. Only valid UTF-8 is filed for reuse — the
            // wire type is a `String`, and lossy-decoding into it would cache
            // an answer that is not the one `gh` printed.
            use std::io::Write as _;
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(&output.stdout);
            let _ = stdout.flush();
            (code, String::from_utf8(output.stdout).ok())
        }
        Err(error) => {
            eprintln!("lazybox gh: could not run {}: {error}", real.display());
            (127, None)
        }
    }
}

/// Run real `gh` and exit with its status — the shim contributing nothing at
/// all, which is what the opt-out and the local subcommands want.
fn exec_inherit(real: &Path, args: &[String]) -> ! {
    std::process::exit(run_inherit(real, args));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &str) -> Vec<String> {
        raw.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn local_subcommands_never_reach_the_daemon() {
        for raw in ["auth status", "config get editor", "--version", "version"] {
            assert_eq!(classify(&args(raw)).kind, None, "{raw}");
        }
        assert_eq!(classify(&[]).kind, None);
    }

    #[test]
    fn an_unrecognised_subcommand_passes_through_uncached() {
        let call = classify(&args("gist create hello.txt"));
        assert_eq!(call.kind, Some(GhCallKind::Other));
        assert_eq!(call.read_key, None);
        assert_eq!(call.change, None);
    }

    #[test]
    fn reads_are_cacheable_and_scoped_to_the_repo() {
        let call = classify(&args("issue view 12 --repo acme/widget"));
        assert_eq!(call.kind, Some(GhCallKind::Read));
        let key = call.read_key.expect("an explicit --repo names the scope");
        assert!(key.starts_with("acme/widget"), "{key}");
        assert!(key.contains("12"), "{key}");
        // The same question from another session must produce the same key.
        assert_eq!(
            Some(key),
            classify(&args("issue view 12 --repo acme/widget")).read_key
        );
        // A different question must not.
        assert_ne!(
            classify(&args("issue view 13 --repo acme/widget")).read_key,
            classify(&args("issue view 12 --repo acme/widget")).read_key
        );
    }

    #[test]
    fn operands_drop_a_flags_value_along_with_the_flag() {
        assert_eq!(
            operands(&args("issue close 12 --reason completed")),
            vec!["issue", "close", "12"]
        );
        assert_eq!(
            operands(&args("issue close --reason completed 12")),
            vec!["issue", "close", "12"]
        );
        assert_eq!(
            operands(&args("--repo o/r issue close 12")),
            vec!["issue", "close", "12"]
        );
        assert_eq!(
            operands(&args("pr comment --body 42 7")),
            vec!["pr", "comment", "7"]
        );
        assert_eq!(
            operands(&args("issue view 12 --repo=o/r")),
            vec!["issue", "view", "12"]
        );
        assert_eq!(operands(&args("--version")), Vec::<&str>::new());
    }

    #[test]
    fn a_pr_read_without_a_target_is_keyed_by_branch() {
        // `gh pr view` with no number resolves against the checked-out branch,
        // so two sessions on different branches are asking different questions
        // and must not share one answer.
        let positional = vec!["pr", "view"];
        let with_branch = read_key(&args("pr view --repo o/r"), &positional, "pr").unwrap();
        let fields: Vec<&str> = with_branch.split('\u{1f}').collect();
        assert_eq!(fields[0], "o/r");
        assert_eq!(fields[1], current_branch().unwrap_or_default());

        // An explicitly numbered PR asks the same question from any branch, so
        // the branch must not narrow the key and cost the fleet the dedupe.
        let positional_target = vec!["pr", "view", "7"];
        let with_target =
            read_key(&args("pr view 7 --repo o/r"), &positional_target, "pr").unwrap();
        assert_eq!(
            with_target.split('\u{1f}').collect::<Vec<_>>(),
            vec!["o/r", "pr", "view", "7", "--repo", "o/r"],
        );
    }

    #[test]
    fn mutations_carry_what_the_daemon_can_conclude() {
        let call = classify(&args("issue close 12 --repo acme/widget"));
        assert_eq!(call.kind, Some(GhCallKind::Mutation));
        assert_eq!(call.read_key, None, "a mutation is never replayed");
        let change = call.change.expect("a numbered close names its record");
        assert_eq!(change.reference, "12");
        assert_eq!(change.repo.as_deref(), Some("acme/widget"));
        assert_eq!(change.kind, GhChangeKind::Closed);

        assert_eq!(
            classify(&args("pr merge 3 --repo o/r"))
                .change
                .unwrap()
                .kind,
            GhChangeKind::Merged
        );
        assert_eq!(
            classify(&args("issue comment 3 --repo o/r"))
                .change
                .unwrap()
                .kind,
            GhChangeKind::Touched
        );
    }

    #[test]
    fn a_mutation_that_names_no_record_reports_no_change() {
        // `gh pr merge` on the current branch, and `gh issue create`: real
        // mutations the shim cannot address, so it claims nothing.
        assert_eq!(classify(&args("pr merge --squash")).change, None);
        assert_eq!(classify(&args("issue create --title x")).change, None);
        assert_eq!(
            classify(&args("pr merge --squash")).kind,
            Some(GhCallKind::Mutation),
            "still quota-counted, and still wakes the poller"
        );
        // A flag value that happens to be a number must never be mistaken for
        // the record — writing Closed onto the wrong row is worse than
        // learning about the close on the next sweep.
        assert_eq!(
            classify(&args("pr comment --body 42 7"))
                .change
                .map(|change| change.reference),
            Some("7".into()),
        );
        // A boolean flag right before the record costs the shim the reference
        // rather than letting it guess: the sweep is the backstop.
        assert_eq!(classify(&args("pr close --delete-branch 7")).change, None);
    }

    #[test]
    fn only_a_reading_api_call_is_cacheable() {
        assert!(is_api_read(&args("api repos/o/r/issues/1")));
        assert!(is_api_read(&args("api -X GET repos/o/r")));
        assert!(!is_api_read(&args("api -X PATCH repos/o/r")));
        assert!(!is_api_read(&args("api --method=DELETE repos/o/r")));
        assert!(!is_api_read(&args("api repos/o/r/issues -f title=x")));
        assert!(!is_api_read(&args(
            "api repos/o/r/issues --input body.json"
        )));
        assert_eq!(
            classify(&args("api -X POST repos/o/r/issues")).kind,
            Some(GhCallKind::Other),
            "a writing api call is passed through, never replayed"
        );
    }

    #[test]
    fn remote_urls_normalize_to_owner_repo() {
        for url in [
            "git@github.com:acme/widget.git",
            "https://github.com/acme/widget.git",
            "https://github.com/acme/widget",
            "ssh://git@github.com/acme/widget.git",
        ] {
            assert_eq!(normalize_remote(url), "acme/widget", "{url}");
        }
    }
}
