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

/// The `BaseRefStalePersistent` note, if the sink recorded one.
fn persistent_stale_note(seen: &PhaseLog) -> Option<String> {
    seen.lock().unwrap().iter().find_map(|p| match p {
        CheckoutPhase::BaseRefStalePersistent(note) => Some(note.clone()),
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
        .arg("-c")
        .arg("commit.gpgsign=false")
        .arg("-c")
        .arg("tag.gpgsign=false")
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

// NOTE: this fixture's bare clone is a legacy FULL clone (blobs local),
// which is what makes a fully offline provision possible. A blobless
// clone's checkout still needs origin for file contents — that contract
// is pinned by `blobless_checkout_fails_clearly_when_origin_unreachable`
// in tests/bare_clone.rs.
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
    // naming the branch, the fallback commit, and why the fetch failed.
    let note = stale_note(&seen).expect("BaseRefStale reported on fetch failure");
    assert!(note.contains("could not refresh main"), "note: {note}");
    assert!(note.contains(&short), "note names the fallback sha: {note}");
    assert!(
        note.contains("fetch failed:"),
        "note carries the cause: {note}"
    );
}

#[tokio::test]
async fn token_source_is_consulted_and_leaves_non_github_origins_alone() {
    // The HTTPS-rewrite env only touches github.com URLs — a local /
    // enterprise origin must keep working exactly as before even when
    // a token resolves.
    let (upstream, base, _bare) = setup("acme", "tokenized");

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let source: lazybox_git_ops::GithubTokenSource = {
        let calls = Arc::clone(&calls);
        Arc::new(move || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Some("test-token".to_string()) })
        })
    };
    let (seen, sink) = recording_sink();
    let wm = WorktreeManager::new(base.path().to_path_buf())
        .with_github_token(source)
        .with_progress(sink);
    let wt_path = base.path().join("token-wt");
    wm.checkout_new_branch_at(&wt_path, "acme", "tokenized", "feature/t", "main")
        .await
        .expect("checkout with a token attached still works on a local origin");

    assert!(
        calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the token source must be consulted for the base-ref fetch"
    );
    assert!(
        stale_note(&seen).is_none() && persistent_stale_note(&seen).is_none(),
        "the auth env must not break a non-github fetch"
    );

    drop(upstream);
}

#[tokio::test]
async fn long_broken_refresh_escalates_to_persistent_stale() {
    let (upstream, base, bare) = setup("acme", "olde");
    drop(upstream);

    // Age the clone's last-contact marker past the 24h escalation
    // threshold: no fetch ever succeeded here, so HEAD's clone-time
    // mtime is the marker.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(4 * 24 * 3600);
    std::fs::File::options()
        .write(true)
        .open(bare.join("HEAD"))
        .expect("open HEAD")
        .set_modified(old)
        .expect("backdate HEAD");

    let (seen, sink) = recording_sink();
    let wm = WorktreeManager::new(base.path().to_path_buf()).with_progress(sink);
    let wt_path = base.path().join("olde-wt");
    wm.checkout_new_branch_at(&wt_path, "acme", "olde", "feature/old", "main")
        .await
        .expect("checkout still succeeds offline");

    assert!(
        stale_note(&seen).is_none(),
        "a days-old failure must escalate, not re-report the one-off note"
    );
    let note = persistent_stale_note(&seen).expect("persistent staleness reported");
    assert!(note.contains("could not refresh main"), "note: {note}");
    assert!(
        note.contains("origin has not refreshed in 4 days"),
        "note quantifies the staleness: {note}"
    );
}

#[tokio::test]
async fn recent_successful_fetch_keeps_a_new_failure_a_one_off() {
    // The last-contact marker must record fetch SUCCESS, not attempts
    // (git's own FETCH_HEAD is truncated + touched even by a failed
    // fetch, which would make days of failures look fresh forever).
    let (upstream, base, bare) = setup("acme", "recent");

    // One healthy checkout writes the success stamp …
    let wm = WorktreeManager::new(base.path().to_path_buf());
    wm.checkout_new_branch_at(
        &base.path().join("ok-wt"),
        "acme",
        "recent",
        "feature/ok",
        "main",
    )
    .await
    .expect("healthy checkout");

    // … so even with an ancient clone-time HEAD, a fresh failure right
    // after a success is a blip, not a persistent degradation.
    drop(upstream);
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 3600);
    std::fs::File::options()
        .write(true)
        .open(bare.join("HEAD"))
        .expect("open HEAD")
        .set_modified(old)
        .expect("backdate HEAD");

    let (seen, sink) = recording_sink();
    let wm = WorktreeManager::new(base.path().to_path_buf()).with_progress(sink);
    wm.checkout_new_branch_at(
        &base.path().join("blip-wt"),
        "acme",
        "recent",
        "feature/blip",
        "main",
    )
    .await
    .expect("checkout still succeeds offline");

    assert!(
        persistent_stale_note(&seen).is_none(),
        "a failure minutes after a successful fetch must not escalate"
    );
    assert!(
        stale_note(&seen).is_some(),
        "the one-off degradation is still surfaced"
    );
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
        .checkout_at(&wt_path, "acme", "attach", "main", None)
        .await
        .expect("checkout_at succeeds offline against the local ref");
    assert_eq!(wt.branch, "main");

    let note = stale_note(&seen).expect("BaseRefStale reported for checkout_at");
    assert!(note.contains("could not refresh main"), "note: {note}");
    assert!(note.contains(&short), "note names the fallback sha: {note}");
}

// The following exercise issue #550: `checkout_at` must provision a PR
// whose head branch isn't a plain branch on `origin` by falling back to
// GitHub's `refs/pull/<N>/head`, which the base repo exposes for every PR.

#[tokio::test]
async fn checkout_at_resolves_fork_pr_via_pull_head() {
    // A fork PR's head lives on the contributor's fork, so `origin` has no
    // `refs/heads/<branch>` for it — only `refs/pull/<N>/head`. Simulate
    // that: build the head commit, publish it as the PR ref, then delete
    // the branch so nothing but the pull ref reaches it.
    let (upstream, base, _bare) = setup("acme", "forked");
    git(
        upstream.path(),
        &["checkout", "-b", "contrib/feature", "-q"],
    );
    std::fs::write(upstream.path().join("f.txt"), "fork work").unwrap();
    git(upstream.path(), &["add", "f.txt"]);
    git(upstream.path(), &["commit", "-m", "fork PR head", "-q"]);
    let head = git_out(upstream.path(), &["rev-parse", "HEAD"]);
    git(upstream.path(), &["update-ref", "refs/pull/7/head", &head]);
    git(upstream.path(), &["checkout", "main", "-q"]);
    git(upstream.path(), &["branch", "-D", "contrib/feature"]);

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt = wm
        .checkout_at(
            &base.path().join("fork-wt"),
            "acme",
            "forked",
            "contrib/feature",
            Some(7),
        )
        .await
        .expect("fork PR provisions via refs/pull/<N>/head");

    assert_eq!(
        git_out(&wt.path, &["rev-parse", "HEAD"]),
        head,
        "worktree HEAD is the fork PR's head commit"
    );
    assert_eq!(wt.branch, "contrib/feature");
    drop(upstream);
}

#[tokio::test]
async fn checkout_at_prefers_pull_head_over_stale_local_ref() {
    // A bot/CI PR whose head branch was deleted after the head advanced.
    // The bare clone still holds a stale local `refs/heads/<branch>` that is
    // an ancestor of the new head (a clean fast-forward, nothing to lose),
    // so the pull-head fallback must win and land the worktree on the fresh
    // head rather than the leftover commit.
    let (upstream, base, bare) = setup("acme", "botpr");
    git(upstream.path(), &["checkout", "-b", "ci/bot", "-q"]);
    std::fs::write(upstream.path().join("v1.txt"), "v1").unwrap();
    git(upstream.path(), &["add", "v1.txt"]);
    git(upstream.path(), &["commit", "-m", "v1", "-q"]);
    let stale = git_out(upstream.path(), &["rev-parse", "HEAD"]);
    // Seed the bare clone's local `refs/heads/ci/bot` at the stale commit.
    git(
        &bare,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/heads/ci/bot:refs/heads/ci/bot",
        ],
    );
    assert_eq!(bare_ref(&bare, "refs/heads/ci/bot"), stale);

    // The PR head advances, then the branch is deleted from origin.
    std::fs::write(upstream.path().join("v2.txt"), "v2").unwrap();
    git(upstream.path(), &["add", "v2.txt"]);
    git(upstream.path(), &["commit", "-m", "v2", "-q"]);
    let head = git_out(upstream.path(), &["rev-parse", "HEAD"]);
    git(upstream.path(), &["update-ref", "refs/pull/9/head", &head]);
    git(upstream.path(), &["checkout", "main", "-q"]);
    git(upstream.path(), &["branch", "-D", "ci/bot"]);

    let (seen, sink) = recording_sink();
    let wm = WorktreeManager::new(base.path().to_path_buf()).with_progress(sink);
    let wt = wm
        .checkout_at(
            &base.path().join("bot-wt"),
            "acme",
            "botpr",
            "ci/bot",
            Some(9),
        )
        .await
        .expect("deleted-branch PR provisions via refs/pull/<N>/head");

    assert_eq!(
        git_out(&wt.path, &["rev-parse", "HEAD"]),
        head,
        "worktree HEAD is the fresh PR head, not the stale local ref"
    );
    assert_ne!(git_out(&wt.path, &["rev-parse", "HEAD"]), stale);
    // We branched from the fresh pull head, so there is nothing stale to
    // warn about — the origin-refresh failure must not surface a
    // BaseRefStale note naming the (superseded) local ref.
    assert!(
        stale_note(&seen).is_none(),
        "no false stale-base warning when the pull head resolves the checkout"
    );
    drop(upstream);
}

#[tokio::test]
async fn checkout_at_uses_origin_branch_fast_path_even_for_a_pr() {
    // When the PR's head branch IS a plain branch on origin (a lazybox-
    // created PR, or any same-repo PR), the origin fast path stays
    // unchanged and the pull-head fetch never runs — verified by the
    // `refs/lazybox/pr/<N>` tracking ref never appearing.
    let (upstream, base, bare) = setup("acme", "onorigin");
    git(
        upstream.path(),
        &["checkout", "-b", "feature/on-origin", "-q"],
    );
    std::fs::write(upstream.path().join("o.txt"), "o").unwrap();
    git(upstream.path(), &["add", "o.txt"]);
    git(upstream.path(), &["commit", "-m", "on origin", "-q"]);
    let head = git_out(upstream.path(), &["rev-parse", "HEAD"]);
    git(upstream.path(), &["update-ref", "refs/pull/3/head", &head]);
    git(upstream.path(), &["checkout", "main", "-q"]);

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt = wm
        .checkout_at(
            &base.path().join("origin-wt"),
            "acme",
            "onorigin",
            "feature/on-origin",
            Some(3),
        )
        .await
        .expect("origin-branch PR provisions via the fast path");

    assert_eq!(git_out(&wt.path, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        bare_ref(&bare, "refs/lazybox/pr/3"),
        "",
        "the origin fast path must not fetch the pull-head fallback ref"
    );
    drop(upstream);
}

#[tokio::test]
async fn checkout_at_keeps_local_ref_with_unpushed_commits_over_pull_head() {
    // A lazybox-created PR: the local `refs/heads/<branch>` carries a commit
    // that isn't in the PR head yet (unpushed work), and the origin branch
    // is gone. Resetting to `refs/pull/<N>/head` would silently drop that
    // commit, so the checkout must keep the local ref when the two diverge.
    let (upstream, base, bare) = setup("acme", "unpushed");

    // The PR head as GitHub knows it (what `refs/pull/<N>/head` resolves to).
    git(upstream.path(), &["checkout", "-b", "feature/wip", "-q"]);
    std::fs::write(upstream.path().join("pushed.txt"), "pushed").unwrap();
    git(upstream.path(), &["add", "pushed.txt"]);
    git(upstream.path(), &["commit", "-m", "pushed", "-q"]);
    let pr_head = git_out(upstream.path(), &["rev-parse", "HEAD"]);
    git(
        upstream.path(),
        &["update-ref", "refs/pull/5/head", &pr_head],
    );

    // Seed the bare clone's local branch, then advance it with a local-only
    // commit the PR head never sees, and delete the branch from origin.
    git(
        &bare,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/heads/feature/wip:refs/heads/feature/wip",
        ],
    );
    std::fs::write(upstream.path().join("local.txt"), "local").unwrap();
    git(upstream.path(), &["add", "local.txt"]);
    git(upstream.path(), &["commit", "-m", "local only", "-q"]);
    let local_tip = git_out(upstream.path(), &["rev-parse", "HEAD"]);
    // Move the bare's local ref up to the unpushed tip (the shape a session
    // that committed locally but never pushed leaves behind).
    git(
        &bare,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/heads/feature/wip:refs/heads/feature/wip",
        ],
    );
    assert_eq!(bare_ref(&bare, "refs/heads/feature/wip"), local_tip);
    git(upstream.path(), &["checkout", "main", "-q"]);
    git(upstream.path(), &["branch", "-D", "feature/wip"]);

    let (seen, sink) = recording_sink();
    let wm = WorktreeManager::new(base.path().to_path_buf()).with_progress(sink);
    let wt = wm
        .checkout_at(
            &base.path().join("wip-wt"),
            "acme",
            "unpushed",
            "feature/wip",
            Some(5),
        )
        .await
        .expect("diverged local ref still provisions");

    assert_eq!(
        git_out(&wt.path, &["rev-parse", "HEAD"]),
        local_tip,
        "the unpushed local commit is preserved, not reset to the PR head"
    );
    assert_ne!(git_out(&wt.path, &["rev-parse", "HEAD"]), pr_head);
    // Here we genuinely branched from the local ref (origin gone, pull head
    // rejected as divergent), so the stale-base warning is accurate and must
    // still fire — the gate suppresses only the fresh pull-head case.
    assert!(
        stale_note(&seen).is_some(),
        "branching from the local ref must still surface a stale-base note"
    );
    drop(upstream);
}
