//! Daemon-side sync cost: how long one poll `tick` spends turning a
//! batch of tasks into workspace upserts + broadcasts. Measures the
//! work the live `#[ignore]` GraphQL harness in `docs/sync-performance.md`
//! can't — it timed wall-clock against the GitHub API, not the local
//! upsert/serialize/short-circuit path that runs every 60s.
//!
//! Two regimes matter. `all_new` runs against a cold store, so every
//! task is a fresh insert + broadcast. `steady_state` runs against a warm
//! store where every task is byte-identical to what's stored, so
//! `commit_upsert`'s no-change short-circuit should skip the write +
//! broadcast — but each task still pays the contextual reads, which this
//! bench pins.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use lazybox_core::{ProviderError, Task, TaskId, TaskRole, TaskState};
use lazybox_server::ServerConfig;
use lazybox_server::polling::{TaskSource, TickState, tick_with_state};
use lazybox_store::MemoryStore;

/// A `TaskSource` that just hands back a fixed batch. Defaults on the
/// trait keep the tick on the non-destructive path (no rescope deletes).
struct FixtureSource {
    tasks: Vec<Task>,
}

impl TaskSource for FixtureSource {
    fn name(&self) -> &str {
        lazybox_gh::SOURCE
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, ProviderError>> + Send + 'a>> {
        let tasks = self.tasks.clone();
        Box::pin(async move { Ok(tasks) })
    }
}

fn synthetic_issue(i: usize) -> Task {
    Task {
        author: String::new(),
        id: TaskId {
            source: "github".into(),
            key: format!("o/r#{i}"),
        },
        title: format!("synthetic task {i}"),
        body: Some(format!("body for {i}")),
        state: TaskState::Open,
        role: TaskRole::Author,
        ci: lazybox_core::CiStatus::None,
        review: lazybox_core::ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: format!("https://github.com/o/r/issues/{i}"),
        repo: Some("o/r".into()),
        branch: None,
        base_branch: None,
        updated_at: chrono::Utc::now(),
        created_at: None,
        closed_at: None,
        labels: vec![],
        reviewers: vec![],
        reviews: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: lazybox_core::Mergeable::Unknown,
        is_behind_base: false,
        merge_blocked: false,
        node_id: None,
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
        kind: None,
        closes_issues: vec![],
        linked_tasks: vec![],
        priority: None,
        state_label: None,
    }
}

fn sources_with(n: usize) -> Vec<Box<dyn TaskSource>> {
    let tasks = (0..n).map(synthetic_issue).collect();
    vec![Box::new(FixtureSource { tasks })]
}

fn bench_tick(c: &mut Criterion) {
    // `tick` arms a per-task `tokio::time::timeout`, so the runtime
    // needs timers enabled.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("tick");
    for &n in &[10usize, 50, 200] {
        // Cold store every iteration: pure insert + broadcast cost.
        group.bench_with_input(BenchmarkId::new("all_new", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let config = ServerConfig::with_store(Arc::new(MemoryStore::new()));
                    (config, sources_with(n), TickState::default())
                },
                |(config, sources, mut state)| {
                    rt.block_on(tick_with_state(&config, &sources, &mut state))
                },
                criterion::BatchSize::SmallInput,
            );
        });

        // Warm store: identical tasks already persisted, so every upsert
        // should hit the no-change short-circuit.
        group.bench_with_input(BenchmarkId::new("steady_state", n), &n, |b, &n| {
            let config = ServerConfig::with_store(Arc::new(MemoryStore::new()));
            let sources = sources_with(n);
            // Prime the store so the timed ticks all see "unchanged".
            rt.block_on(tick_with_state(
                &config,
                &sources,
                &mut TickState::default(),
            ));
            b.iter_batched(
                TickState::default,
                |mut state| rt.block_on(tick_with_state(&config, &sources, &mut state)),
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_tick);
criterion_main!(benches);
