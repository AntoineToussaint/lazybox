//! Live trace-capture harness for the PR-sync path (issue #12).
//!
//! Ignored by default — it hits the real GitHub GraphQL API with the
//! caller's token, so it never runs in CI. It exists as the
//! reproducible way to capture the per-branch / per-phase breakdown
//! documented in `docs/sync-performance.md`.
//!
//! Run it against your own account:
//!
//! ```sh
//! # token comes from $GH_TOKEN, $GITHUB_TOKEN, or `gh auth token`
//! LAZYBOX_WATCH=owner/repo-a,owner/repo-b \
//!   cargo test -p lazybox-gh --test sync_trace -- --ignored --nocapture
//! ```
//!
//! `gh_sync_metrics` is forced to DEBUG so the per-branch
//! `branch fetch complete` lines and the `fetch_all_prs union
//! breakdown` line are emitted regardless of the ambient `RUST_LOG`.

use lazybox_auth::Credential;
use lazybox_gh::GhClient;

fn token() -> String {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(var)
            && !t.trim().is_empty()
        {
            return t.trim().to_string();
        }
    }
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .expect("no $GH_TOKEN/$GITHUB_TOKEN and `gh auth token` failed to spawn");
    assert!(
        out.status.success(),
        "`gh auth token` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("token is utf-8")
        .trim()
        .to_string()
}

fn watch_repos() -> Vec<String> {
    std::env::var("LAZYBOX_WATCH")
        .ok()
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
#[ignore = "hits the live GitHub API; run manually to capture a sync trace"]
async fn capture_fetch_all_prs_trace() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
                .add_directive("gh_sync_metrics=debug".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    let client = GhClient::from_credential(Credential::new(token(), "sync_trace"))
        .await
        .expect("client builds from token")
        .with_watch_repos(watch_repos());

    // `None` = unwindowed reconcile sweep, the heavy path this trace
    // exists to measure. Pass a recent `Some(updated_since)` instead to
    // profile a steady-state windowed sweep (issue #14).
    let started = std::time::Instant::now();
    let tasks = client
        .fetch_all_prs(None)
        .await
        .expect("fetch_all_prs succeeds");
    eprintln!(
        "\n=== fetch_all_prs returned {} PRs in {}ms ===",
        tasks.len(),
        started.elapsed().as_millis()
    );
}
