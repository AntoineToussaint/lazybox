#[derive(Debug, Clone, Copy)]
struct HourReplay {
    rest_requests: u32,
    graphql_requests: u32,
    graphql_points: u32,
    response_bytes: u64,
    request_p95_ms: u64,
    notification_freshness_p95_secs: u64,
    reconcile_max_age_secs: u64,
}

fn captured_current_main(changed_targets: u32) -> HourReplay {
    HourReplay {
        rest_requests: 60,
        graphql_requests: 84 + changed_targets,
        graphql_points: 84 + changed_targets,
        response_bytes: 960_000 + u64::from(changed_targets) * 12_000,
        request_p95_ms: 13_800,
        notification_freshness_p95_secs: 60,
        reconcile_max_age_secs: 3600,
    }
}

fn execute_graphql(
    budget: &mut lazybox_gh::RateBudget,
    class: &str,
    remaining: &mut u32,
    used: &mut u32,
    reset_at: chrono::DateTime<chrono::Utc>,
    observed_at: std::time::Instant,
) -> Option<(u64, u64)> {
    budget
        .admit(
            lazybox_gh::ApiResource::Graphql,
            class,
            lazybox_gh::RequestPriority::Recent,
            1,
        )
        .ok()?;
    *remaining = remaining.saturating_sub(1);
    *used = used.saturating_add(1);
    let (bytes, duration) = match class {
        "PR search" => (150_000, std::time::Duration::from_millis(1_800)),
        "single-PR notification deep-fetch" => (12_000, std::time::Duration::from_millis(700)),
        // Windowed `author:USER` search: a near-empty single page most ticks.
        "authored-PR probe" => (2_000, std::time::Duration::from_millis(400)),
        _ => (1_000, std::time::Duration::from_millis(350)),
    };
    budget.observe_graphql_response(
        class,
        lazybox_gh::RemoteRateLimit {
            remaining: *remaining,
            limit: 5000,
            reset_at,
            observed_at,
        },
        *used,
        1,
        200,
        bytes as usize,
        duration,
    );
    Some((bytes, duration.as_millis() as u64))
}

fn governed(changed_targets: u32, external_pressure: bool) -> HourReplay {
    use chrono::Utc;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    let wall_start = Utc::now();
    let mono_start = Instant::now();
    let reset_at = wall_start + chrono::Duration::hours(1);
    let mut budget = lazybox_gh::RateBudget::default_for_lazybox();
    budget.observe(lazybox_gh::RemoteRateLimit {
        remaining: 5000,
        limit: 5000,
        reset_at,
        observed_at: mono_start,
    });
    let client = lazybox_gh::GhClient::stub_with_rate_limit_for_tests(
        "report", "report", 5000, 5000, reset_at,
    )
    .expect("stub client");
    let forecast = client.background_sweep_forecast(true, true);
    let mut rotation = lazybox_server::polling::RoundRobinState::default();
    for index in 0..30 {
        rotation.record_sync(
            &format!("owner/repo-{index:02}"),
            mono_start - Duration::from_secs((30 - index) * 60),
        );
    }
    let sessioned: HashSet<String> = (0..10)
        .map(|index| format!("owner/repo-{index:02}"))
        .collect();
    let tick_interval =
        lazybox_server::polling::background_tick_interval(Duration::from_secs(60), 0);
    let sweep_minutes = lazybox_gh::GhClient::FULL_SWEEP_INTERVAL.as_secs() / 60;
    let mut remaining = 5000u32;
    let mut used = 0u32;
    let mut response_bytes = 0u64;
    let mut changes_left = changed_targets;

    for minute in 0..60u64 {
        let wall_now = wall_start + chrono::Duration::minutes(minute as i64);
        let mono_now = mono_start + Duration::from_secs(minute * 60);
        if external_pressure && minute == 1 {
            remaining = 2200;
            used = 2800;
            budget.observe(lazybox_gh::RemoteRateLimit {
                remaining,
                limit: 5000,
                reset_at,
                observed_at: mono_now,
            });
        }
        let plan = budget.begin_background_tick(tick_interval, wall_now, mono_now);
        let sweep_due = minute.is_multiple_of(sweep_minutes);
        let global_due = lazybox_server::polling::will_run_global(
            rotation.cursor.len(),
            rotation.tick,
            lazybox_server::polling::DEFAULT_ROUND_ROBIN_N,
        );
        let required = forecast.required_points(global_due, true);
        let admitted = sweep_due && plan.admits_complete_graphql_unit(false, required);
        let max_repos = forecast.repo_capacity(
            plan.graphql_points,
            global_due,
            lazybox_server::polling::DEFAULT_ROUND_ROBIN_N,
        );
        let pick = lazybox_server::polling::plan_round_robin_tick_budgeted(
            &mut rotation,
            &sessioned,
            admitted,
            lazybox_server::polling::DEFAULT_ROUND_ROBIN_N,
            max_repos,
            mono_now,
        );

        let mut operations = Vec::new();
        if admitted {
            if pick.run_global {
                operations.push("PR search");
            } else {
                operations.extend(std::iter::repeat_n("round-robin-repo", pick.repos.len()));
            }
            operations.extend(["review-requested", "merged-sweep", "issues search"]);
        }
        if changes_left > 0 {
            operations.push("single-PR notification deep-fetch");
            changes_left -= 1;
        }
        // Discovery probe: a cheap `author:USER` search on non-sweep ticks so
        // a self/agent-created PR (which has no notification) surfaces within
        // a tick or two instead of waiting out the 30-min full sweep. Bounded
        // to ~one small near-empty page per `AUTHORED_PROBE_INTERVAL`.
        if !admitted && minute % 2 == 0 {
            operations.push("authored-PR probe");
        }
        for class in operations {
            if let Some((bytes, _)) = execute_graphql(
                &mut budget,
                class,
                &mut remaining,
                &mut used,
                reset_at,
                mono_now,
            ) {
                response_bytes += bytes;
            }
        }
    }

    let snapshot = budget.snapshot();
    if external_pressure {
        assert_eq!(
            snapshot
                .resources
                .iter()
                .find(|resource| resource.resource == "graphql")
                .map(|resource| resource.allowance),
            Some(0),
            "the production governor must stop scheduled work after the external drain"
        );
    }

    HourReplay {
        rest_requests: 60,
        graphql_requests: snapshot.total.requests as u32,
        graphql_points: snapshot.total.graphql_points as u32,
        response_bytes,
        request_p95_ms: snapshot.request_p95_ms.unwrap_or(0),
        notification_freshness_p95_secs: tick_interval.as_secs(),
        reconcile_max_age_secs: lazybox_gh::GhClient::FULL_RECONCILE_INTERVAL.as_secs()
            + u64::from(external_pressure) * tick_interval.as_secs(),
    }
}

#[tokio::test]
async fn reproducible_four_scenario_baseline_and_after_report() {
    let scenarios = [
        ("quiet", 0, false),
        ("sparse notifications", 6, false),
        ("multi-repo burst", 12, false),
        ("external consumer", 0, true),
    ];
    for (name, changes, external_pressure) in scenarios {
        let baseline = captured_current_main(changes);
        let after = governed(changes, external_pressure);
        eprintln!("{name}: baseline={baseline:?} after={after:?}");

        assert_eq!(after.rest_requests, baseline.rest_requests);
        assert!(after.graphql_requests <= baseline.graphql_requests);
        assert!(after.graphql_points <= baseline.graphql_points);
        assert!(after.response_bytes <= baseline.response_bytes);
        assert!(after.request_p95_ms <= baseline.request_p95_ms);
        assert_eq!(
            after.notification_freshness_p95_secs,
            baseline.notification_freshness_p95_secs
        );
        if external_pressure {
            assert_eq!(after.reconcile_max_age_secs, 3660);
        } else {
            assert_eq!(after.reconcile_max_age_secs, 3600);
        }
    }

    let baseline = captured_current_main(0);
    let after = governed(0, false);
    let reduction = 1.0 - f64::from(after.graphql_points) / f64::from(baseline.graphql_points);
    // The governor still roughly halves the quiet scheduled load versus old
    // main. The threshold is 0.50 rather than 0.75 because we deliberately
    // spend part of that headroom on the `author:USER` discovery probe: a
    // small near-empty search every couple of ticks that surfaces
    // self/agent-created PRs (which have no notification) in ~one tick
    // instead of waiting out the 30-min sweep. Even with the probe the quiet
    // hour costs ~38 GraphQL points — under 1% of GitHub's 5000/hr budget —
    // so the absolute floor stays trivially small; this assertion guards the
    // reduction from silently eroding further.
    assert!(
        reduction >= 0.50,
        "quiet GraphQL point reduction was {:.1}%",
        reduction * 100.0
    );
    // Absolute ceiling: the whole quiet hour must stay a tiny fraction of the
    // GraphQL budget even after the probe, so the probe can never grow into a
    // rate-limit hazard without tripping this.
    assert!(
        after.graphql_points < 250,
        "quiet GraphQL points {} exceeded the absolute safety ceiling",
        after.graphql_points
    );
}

#[test]
fn sync_cursors_survive_a_real_sqlite_restart() {
    use chrono::{TimeZone, Utc};
    use lazybox_store::{SqliteStore, Store};

    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("state.db");
    let key = "github:sync-cursors:v1:viewer";
    let expected = lazybox_gh::SyncCursors {
        last_modified: Some("Thu, 30 Jul 2026 10:15:00 GMT".into()),
        last_full_sweep_at: Some(Utc.with_ymd_and_hms(2026, 7, 30, 10, 0, 0).unwrap()),
        last_pr_sweep_at: Some(Utc.with_ymd_and_hms(2026, 7, 30, 10, 1, 0).unwrap()),
        last_merged_sweep_at: Some(Utc.with_ymd_and_hms(2026, 7, 30, 10, 2, 0).unwrap()),
        last_full_reconcile_at: Some(Utc.with_ymd_and_hms(2026, 7, 30, 10, 0, 0).unwrap()),
    };

    {
        let store = SqliteStore::open(&db).expect("open sqlite store");
        store
            .set_kv(key, &serde_json::to_string(&expected).unwrap())
            .expect("persist cursors");
    }

    let reopened = SqliteStore::open(&db).expect("reopen sqlite store");
    let payload = reopened
        .get_kv(key)
        .expect("load cursors")
        .expect("cursor value");
    let restored: lazybox_gh::SyncCursors =
        serde_json::from_str(&payload).expect("deserialize cursors");
    assert_eq!(restored, expected);
}
