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
//! # token comes from $LAZYBOX_GITHUB_TOKEN, $GH_TOKEN, $GITHUB_TOKEN, or `gh auth token`
//! LAZYBOX_WATCH=owner/repo-a,owner/repo-b \
//!   cargo test -p lazybox-gh --test sync_trace -- --ignored --nocapture
//! ```
//!
//! `gh_sync_metrics` is forced to DEBUG so the per-branch
//! `branch fetch complete` lines and the `fetch_all_prs union
//! breakdown` line are emitted regardless of the ambient `RUST_LOG`.

use lazybox_auth::Credential;
use lazybox_core::{CiStatus, ReviewStatus, Task};
use lazybox_gh::GhClient;

/// Install the metrics subscriber once for the test binary. Both
/// traces share a process when `cargo test` runs them together, so
/// `try_init` (not `init`) — a second call must be a no-op, not a
/// panic.
fn init_metrics_subscriber() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
                .add_directive("gh_sync_metrics=debug".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .try_init();
}

fn token() -> String {
    for var in ["LAZYBOX_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(var)
            && !t.trim().is_empty()
        {
            return t.trim().to_string();
        }
    }
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .expect(
            "no $LAZYBOX_GITHUB_TOKEN/$GH_TOKEN/$GITHUB_TOKEN and \
             `gh auth token` failed to spawn",
        );
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
    init_metrics_subscriber();

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

/// Prefetch scoring, mirrored from `prefetch_top_pr_details` in
/// `crates/server/src/polling/handlers.rs`. Kept in sync by hand —
/// this harness exists to measure that handler's per-tick cost, so
/// it must pick the same PRs the handler would.
fn prefetch_score(task: &Task) -> i32 {
    let mut score = 1;
    if matches!(task.ci, CiStatus::Failure | CiStatus::Mixed) {
        score += 100;
    }
    if matches!(
        task.review,
        ReviewStatus::ChangesRequested | ReviewStatus::Pending
    ) {
        score += 50;
    }
    score += (task.unread_count.min(5) as i32) * 10;
    score
}

/// Capture the per-tick detail-prefetch cost (issue #16).
///
/// The `fetch_all_prs` trace above measures only the main poll. The
/// server's poll tick also runs `prefetch_top_pr_details`, which
/// scores the just-polled PRs and fires up to 5 `fetch_pr_details`
/// calls (3 concurrent) — each emitting a `branch="pr-details"`
/// metrics line. This reproduces that second phase against a real
/// inbox so its elapsed/cost/bytes share of the tick is measurable.
#[tokio::test]
#[ignore = "hits the live GitHub API; run manually to capture a sync trace"]
async fn capture_prefetch_trace() {
    use futures::stream::{self, StreamExt};

    const PREFETCH_TOP_N: usize = 5;
    const PREFETCH_CONCURRENCY: usize = 3;

    init_metrics_subscriber();

    let client = GhClient::from_credential(Credential::new(token(), "sync_trace"))
        .await
        .expect("client builds from token")
        .with_watch_repos(watch_repos());

    // `None` = unwindowed reconcile sweep (issue #14); this trace
    // measures the prefetch phase, not the windowed-poll path.
    let tasks = client
        .fetch_all_prs(None)
        .await
        .expect("fetch_all_prs succeeds");

    // Same selection the handler makes: score, drop the 0-score
    // (score == 1, no actionable signal) rows, highest first, take N.
    let mut scored: Vec<(i32, String)> = tasks
        .iter()
        .filter_map(|t| {
            let node_id = t.node_id.clone()?;
            let score = prefetch_score(t);
            (score > 1).then_some((score, node_id))
        })
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.truncate(PREFETCH_TOP_N);

    let total = scored.len();
    eprintln!(
        "\n=== prefetch: {total} of {} PRs scored above threshold ===",
        tasks.len(),
    );
    if total == 0 {
        eprintln!("no PRs cleared the prefetch threshold; nothing to measure");
        return;
    }

    let started = std::time::Instant::now();
    let fetched: usize = stream::iter(scored)
        .map(|(_score, node_id)| {
            let client = client.clone();
            async move {
                match client.fetch_pr_details(&node_id).await {
                    Ok(Some(_)) => 1,
                    Ok(None) => 0,
                    Err(e) => {
                        eprintln!("fetch_pr_details({node_id}) failed: {e}");
                        0
                    }
                }
            }
        })
        .buffer_unordered(PREFETCH_CONCURRENCY)
        .collect::<Vec<usize>>()
        .await
        .into_iter()
        .sum();
    eprintln!(
        "\n=== prefetch phase: {fetched}/{total} PR details fetched in {}ms (concurrency {PREFETCH_CONCURRENCY}) ===",
        started.elapsed().as_millis()
    );
}
