//! Tests for worktree-base freshness — issue #35.
//!
//! `checkout_new_branch_at` must fetch the base branch from origin
//! and fast-forward the bare clone's local ref before branching, so
//! new worktrees don't start on a stale base. When the fetch fails
//! (offline / auth) the call must still succeed against the local
//! ref rather than blocking.

use lazybox_git_ops::{CheckoutPhase, WorktreeManager};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// Shared log of the phases a recording sink observed.
type PhaseLog = Arc<Mutex<Vec<CheckoutPhase>>>;

/// A progress sink that records every reported [`CheckoutPhase`] so a
/// test can assert on the boundaries the manager emitted (e.g. that a
/// degraded fetch surfaced a `BaseRefStale`).
fn recording_sink() -> (PhaseLog, Arc<lazybox_git_ops::ProgressSink>) {
    let seen: PhaseLog = Arc::new(Mutex::new(Vec::new()));
    let sink: Arc<lazybox_git_ops::ProgressSink> = {
        let seen = Arc::clone(&seen);
        Arc::new(move |phase: CheckoutPhase| seen.lock().unwrap().push(phase))
    };
    (seen, sink)
}

/// The `BaseRefStale` note, if the sink recorded one.
fn stale_note(seen: &PhaseLog) -> Option<String> {
    seen.lock().unwrap().iter().find_map(|p| match p {
        CheckoutPhase::BaseRefStale(note) => Some(note.clone()),
        _ => None,
    })
}

/// Build a Command for git in `cwd`, with GIT_DIR / GIT_WORK_TREE
/// scrubbed. `cargo test` inherits env vars from the surrounding
/// lazybox worktree (its `.git` is a gitfile pointing into the main
/// repo's gitdir), and any leaked `GIT_*` var would override
/// `current_dir(cwd)` and run git against the wrong repo.
fn git_cmd(cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR");
    cmd
}

fn git(cwd: &Path, args: &[&str]) {
    let out = git_cmd(cwd, args).output().unwrap();
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(cwd: &Path, args: &[&str]) -> String {
    let out = git_cmd(cwd, args).output().unwrap();
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Read the SHA of a specific ref in a bare repo. `for-each-ref`
/// is the most direct command for this: returns the empty string
/// for non-existent refs (so assertions get a usable diff) and
/// avoids the argument-parsing quirks of `show-ref --hash <ref>`
/// (which is interpreted as `--hash=<ref>`).
fn bare_ref(bare: &Path, refname: &str) -> String {
    git_out(bare, &["for-each-ref", "--format=%(objectname)", refname])
}

/// Set up a regular "upstream" repo (with one initial commit on
/// `main`) and a bare clone of it placed at the layout that
/// `WorktreeManager` expects (`<base>/repos/<owner>/<repo>.git`),
/// with `origin` rewritten to the local upstream path. This lets
/// the manager's fetch + worktree calls hit local files instead
/// of github.com.
///
/// Returns (upstream TempDir, base TempDir, bare clone path).
fn setup(owner: &str, repo: &str) -> (TempDir, TempDir, PathBuf) {
    let upstream = TempDir::new().unwrap();
    git(upstream.path(), &["init", "-b", "main", "-q"]);
    git(upstream.path(), &["config", "user.email", "t@example.com"]);
    git(upstream.path(), &["config", "user.name", "Tester"]);
    git(
        upstream.path(),
        &["commit", "--allow-empty", "-m", "first", "-q"],
    );

    let base = TempDir::new().unwrap();
    let bare = base
        .path()
        .join("repos")
        .join(owner)
        .join(format!("{repo}.git"));
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    // Route through `git()` (which clears GIT_DIR &c.) for the same
    // env hygiene as everything else in this file. cwd is irrelevant
    // for `git clone --bare A B` — both source and dest are absolute
    // paths and clone creates a new repo.
    git(
        upstream.path(),
        &[
            "clone",
            "--bare",
            "-q",
            &upstream.path().to_string_lossy(),
            &bare.to_string_lossy(),
        ],
    );
    // Wire origin to the upstream local path so subsequent fetches
    // resolve without touching the network.
    git(
        &bare,
        &[
            "remote",
            "set-url",
            "origin",
            &upstream.path().to_string_lossy(),
        ],
    );

    (upstream, base, bare)
}

#[tokio::test]
async fn worktree_branches_off_latest_origin_main() {
    let (upstream, base, bare) = setup("acme", "widgets");

    // The bare clone currently mirrors the first commit on upstream.
    let initial = git_out(upstream.path(), &["rev-parse", "HEAD"]);
    assert_eq!(bare_ref(&bare, "refs/heads/main"), initial);

    // Advance upstream's main beyond the bare clone's view.
    std::fs::write(upstream.path().join("a.txt"), "a").unwrap();
    git(upstream.path(), &["add", "a.txt"]);
    git(upstream.path(), &["commit", "-m", "second", "-q"]);
    let latest = git_out(upstream.path(), &["rev-parse", "HEAD"]);
    assert_ne!(initial, latest);

    // Confirm the bare clone is still stale before we create a worktree.
    assert_eq!(
        bare_ref(&bare, "refs/heads/main"),
        initial,
        "bare clone's local main is stale before checkout"
    );

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt_path = base.path().join("custom-wt");
    let wt = wm
        .checkout_new_branch_at(&wt_path, "acme", "widgets", "feature/x", "main")
        .await
        .expect("checkout_new_branch_at");

    // The new worktree must point at upstream's latest, not the stale
    // commit the bare clone was sitting on.
    assert_eq!(
        git_out(&wt.path, &["rev-parse", "HEAD"]),
        latest,
        "worktree HEAD is at upstream's latest commit"
    );

    // The bare clone's local refs/heads/main must also be fast-forwarded
    // so future offline checkouts start from a recent base too.
    assert_eq!(
        bare_ref(&bare, "refs/heads/main"),
        latest,
        "bare clone's local main is fast-forwarded to origin/main"
    );

    drop(upstream);
}

#[tokio::test]
async fn worktree_creation_succeeds_when_fetch_fails() {
    let (upstream, base, bare) = setup("acme", "gizmo");

    let initial = git_out(upstream.path(), &["rev-parse", "HEAD"]);

    // Simulate offline / unreachable origin: nuke the upstream so the
    // bare clone's `origin` URL no longer resolves.
    drop(upstream);

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt_path = base.path().join("custom-wt");
    let wt = wm
        .checkout_new_branch_at(&wt_path, "acme", "gizmo", "feature/y", "main")
        .await
        .expect("checkout_new_branch_at must succeed with offline origin");

    // Falls back to the local ref — what was the bare clone's main
    // before we tried to refresh it.
    assert_eq!(
        git_out(&wt.path, &["rev-parse", "HEAD"]),
        initial,
        "worktree falls back to the bare clone's local main when fetch fails"
    );
    // Local ref untouched on fetch failure.
    assert_eq!(bare_ref(&bare, "refs/heads/main"), initial);
}

#[tokio::test]
async fn respects_configured_base_branch() {
    // Issue #35 asks the change to "respect the configured default
    // branch (not hardcode main)". The function already takes
    // base_branch by parameter, but exercise that explicitly with a
    // non-main base.
    let (upstream, base, _bare) = setup("acme", "trunk");
    git(upstream.path(), &["checkout", "-b", "develop", "-q"]);
    std::fs::write(upstream.path().join("d.txt"), "d").unwrap();
    git(upstream.path(), &["add", "d.txt"]);
    git(upstream.path(), &["commit", "-m", "on develop", "-q"]);
    let develop_tip = git_out(upstream.path(), &["rev-parse", "HEAD"]);

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt_path = base.path().join("dev-wt");
    let wt = wm
        .checkout_new_branch_at(&wt_path, "acme", "trunk", "feature/dev", "develop")
        .await
        .expect("checkout_new_branch_at off develop");

    assert_eq!(
        git_out(&wt.path, &["rev-parse", "HEAD"]),
        develop_tip,
        "new worktree starts from develop's tip"
    );

    drop(upstream);
}

#[tokio::test]
async fn new_branch_surfaces_stale_base_when_fetch_fails() {
    let (upstream, base, bare) = setup("acme", "stale");

    let initial = git_out(upstream.path(), &["rev-parse", "HEAD"]);
    let short = git_out(upstream.path(), &["rev-parse", "--short", "HEAD"]);
    // Offline / unreachable origin: the fetch will fail and the checkout
    // falls back to the bare clone's local main.
    drop(upstream);

    let (seen, sink) = recording_sink();
    let wm = WorktreeManager::new(base.path().to_path_buf()).with_progress(sink);
    let wt_path = base.path().join("stale-wt");
    let wt = wm
        .checkout_new_branch_at(&wt_path, "acme", "stale", "feature/z", "main")
        .await
        .expect("checkout still succeeds offline");

    // Still branched off the local ref (creation must not block on the
    // network) …
    assert_eq!(git_out(&wt.path, &["rev-parse", "HEAD"]), initial);
    assert_eq!(bare_ref(&bare, "refs/heads/main"), initial);

    // … but the degradation is surfaced, not silent: a stale-base note
    // naming the branch and the fallback commit.
    let note = stale_note(&seen).expect("BaseRefStale reported on fetch failure");
    assert!(note.contains("could not refresh main"), "note: {note}");
    assert!(note.contains(&short), "note names the fallback sha: {note}");
}

#[tokio::test]
async fn new_branch_reports_no_stale_note_when_fetch_succeeds() {
    let (upstream, base, _bare) = setup("acme", "fresh");

    let (seen, sink) = recording_sink();
    let wm = WorktreeManager::new(base.path().to_path_buf()).with_progress(sink);
    let wt_path = base.path().join("fresh-wt");
    wm.checkout_new_branch_at(&wt_path, "acme", "fresh", "feature/ok", "main")
        .await
        .expect("checkout_new_branch_at");

    assert!(
        stale_note(&seen).is_none(),
        "a healthy fetch must not report a stale base"
    );

    drop(upstream);
}

#[tokio::test]
async fn checkout_existing_branch_surfaces_stale_base_when_fetch_fails() {
    // The same treatment for `checkout_at` when attaching to an existing
    // branch (issue #320): an offline fetch must still surface the
    // stale-ref note rather than only logging it.
    let (upstream, base, _bare) = setup("acme", "attach");
    let short = git_out(upstream.path(), &["rev-parse", "--short", "HEAD"]);
    drop(upstream);

    let (seen, sink) = recording_sink();
    let wm = WorktreeManager::new(base.path().to_path_buf()).with_progress(sink);
    let wt_path = base.path().join("attach-wt");
    let wt = wm
        .checkout_at(&wt_path, "acme", "attach", "main")
        .await
        .expect("checkout_at succeeds offline against the local ref");
    assert_eq!(wt.branch, "main");

    let note = stale_note(&seen).expect("BaseRefStale reported for checkout_at");
    assert!(note.contains("could not refresh main"), "note: {note}");
    assert!(note.contains(&short), "note names the fallback sha: {note}");
}
