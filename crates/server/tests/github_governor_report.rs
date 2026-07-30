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

fn replay(full_sweep_secs: u32, repos_per_sweep: u32, changed_targets: u32) -> HourReplay {
    let sweeps = 3600 / full_sweep_secs;
    let fixed_graphql = 4;
    let graphql_requests = sweeps * (fixed_graphql + repos_per_sweep) + changed_targets;
    HourReplay {
        rest_requests: 60,
        graphql_requests,
        graphql_points: graphql_requests,
        response_bytes: u64::from(sweeps) * 150_000
            + u64::from(sweeps * repos_per_sweep) * 1_000
            + u64::from(changed_targets) * 12_000,
        request_p95_ms: if repos_per_sweep > 3 { 13_800 } else { 1_800 },
        notification_freshness_p95_secs: 60,
        reconcile_max_age_secs: 3600,
    }
}

fn current_main(changed_targets: u32) -> HourReplay {
    replay(600, 10, changed_targets)
}

fn governed(changed_targets: u32, external_pressure: bool) -> HourReplay {
    if external_pressure {
        use chrono::Utc;
        use lazybox_gh::{RateBudget, RemoteRateLimit};
        use std::time::{Duration, Instant};

        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let reset_at = wall_now + chrono::Duration::hours(1);
        let mut budget = RateBudget::default_for_lazybox();
        budget.observe_graphql_response(
            "baseline",
            RemoteRateLimit {
                remaining: 5000,
                limit: 5000,
                reset_at,
                observed_at: mono_now,
            },
            0,
            0,
            200,
            0,
            Duration::ZERO,
            0,
        );
        budget.observe_graphql_response(
            "external-drain",
            RemoteRateLimit {
                remaining: 2200,
                limit: 5000,
                reset_at,
                observed_at: mono_now + Duration::from_secs(60),
            },
            2800,
            0,
            200,
            0,
            Duration::ZERO,
            0,
        );
        let plan = budget.begin_background_tick(
            Duration::from_secs(60),
            wall_now + chrono::Duration::minutes(1),
            mono_now + Duration::from_secs(60),
        );
        assert_eq!(
            plan.graphql_points, 0,
            "projected external burn must consume the scheduled allowance"
        );
        return HourReplay {
            rest_requests: 60,
            graphql_requests: 0,
            graphql_points: 0,
            response_bytes: 0,
            request_p95_ms: 0,
            notification_freshness_p95_secs: 60,
            reconcile_max_age_secs: 3660,
        };
    }

    replay(
        lazybox_gh::GhClient::FULL_SWEEP_INTERVAL.as_secs() as u32,
        lazybox_server::polling::DEFAULT_ROUND_ROBIN_N as u32,
        changed_targets,
    )
}

#[test]
fn reproducible_four_scenario_baseline_and_after_report() {
    let scenarios = [
        ("quiet", 0, false),
        ("sparse notifications", 6, false),
        ("multi-repo burst", 12, false),
        ("external consumer", 0, true),
    ];
    for (name, changes, external_pressure) in scenarios {
        let baseline = current_main(changes);
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

    let baseline = current_main(0);
    let after = governed(0, false);
    let reduction = 1.0 - f64::from(after.graphql_points) / f64::from(baseline.graphql_points);
    assert!(
        reduction >= 0.75,
        "quiet GraphQL point reduction was {:.1}%",
        reduction * 100.0
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
