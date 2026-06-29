use chrono::{DateTime, Utc};

/// Human-readable relative time ("2h ago", "3d ago", "just now").
///
/// Reads the system clock directly — intended for display-side code (UI
/// render). Business logic should prefer `time_ago_at` with an injected `now`.
pub fn time_ago(dt: &DateTime<Utc>) -> String {
    time_ago_at(dt, Utc::now())
}

/// `time_ago` with an explicit reference point. Pure — used by callers that
/// already have a `now` available (reducer, tests).
pub fn time_ago_at(dt: &DateTime<Utc>, now: DateTime<Utc>) -> String {
    let diff = now.signed_duration_since(dt);

    let secs = diff.num_seconds();
    if secs < 60 {
        return "just now".to_string();
    }

    let mins = diff.num_minutes();
    if mins < 60 {
        return format!("{mins}m ago");
    }

    let hours = diff.num_hours();
    if hours < 24 {
        return format!("{hours}h ago");
    }

    let days = diff.num_days();
    if days < 30 {
        return format!("{days}d ago");
    }

    // 30-day months don't tile a 365-day year (12 × 30 = 360), so gate
    // on `days` rather than `months`: without this, ages of 360–364 days
    // skip the months branch (`months == 12`) yet still floor to 0 years.
    let months = days / 30;
    if days < 365 {
        return format!("{months}mo ago");
    }

    let years = days / 365;
    format!("{years}y ago")
}

/// Staleness indicator for PRs that have been open too long or idle.
pub enum Staleness {
    /// Fresh — updated recently.
    Fresh,
    /// Getting stale — no activity for a while.
    Stale { idle_days: i64 },
    /// Very stale — been open a long time with no activity.
    Abandoned { open_days: i64, idle_days: i64 },
}

pub fn staleness(
    created_at: &DateTime<Utc>,
    updated_at: &DateTime<Utc>,
    now: DateTime<Utc>,
) -> Staleness {
    let open_days = now.signed_duration_since(created_at).num_days();
    let idle_days = now.signed_duration_since(updated_at).num_days();

    if idle_days > 14 && open_days > 30 {
        Staleness::Abandoned {
            open_days,
            idle_days,
        }
    } else if idle_days > 3 {
        Staleness::Stale { idle_days }
    } else {
        Staleness::Fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ago(now: DateTime<Utc>, days: i64, secs: i64) -> String {
        let dt = now - chrono::Duration::days(days) - chrono::Duration::seconds(secs);
        time_ago_at(&dt, now)
    }

    #[test]
    fn buckets() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(ago(now, 0, 30), "just now");
        assert_eq!(ago(now, 0, 90), "1m ago");
        assert_eq!(ago(now, 0, 3 * 3600), "3h ago");
        assert_eq!(ago(now, 5, 0), "5d ago");
        assert_eq!(ago(now, 60, 0), "2mo ago");
    }

    #[test]
    fn month_to_year_boundary_has_no_zero_year_gap() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        // The 360–364 day window used to floor to "0y ago" because the
        // /30-month bucket ended (months == 12) before the /365-year
        // bucket began.
        assert_eq!(ago(now, 360, 0), "12mo ago");
        assert_eq!(ago(now, 364, 0), "12mo ago");
        assert_eq!(ago(now, 365, 0), "1y ago");
        assert_eq!(ago(now, 800, 0), "2y ago");
    }

    #[test]
    fn staleness_tiers() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let fresh = now - chrono::Duration::days(1);
        let old = now - chrono::Duration::days(40);
        let idle = now - chrono::Duration::days(20);
        assert!(matches!(staleness(&fresh, &fresh, now), Staleness::Fresh));
        assert!(matches!(
            staleness(&now, &(now - chrono::Duration::days(5)), now),
            Staleness::Stale { .. }
        ));
        assert!(matches!(
            staleness(&old, &idle, now),
            Staleness::Abandoned { .. }
        ));
    }
}
