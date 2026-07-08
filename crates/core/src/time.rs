use chrono::{DateTime, Utc};

/// Days-per-month used by the relative-time formatter: the boundary at
/// which an age stops reading in days (`29d`) and starts reading in
/// months (`1mo`), and the divisor for the month count. A calendar
/// approximation — good enough for a staleness glance, not for exact
/// arithmetic.
pub const DAYS_PER_MONTH: i64 = 30;

/// Age, in days, past which a task is treated as "stale" and
/// de-emphasized in the sidebar (issue #274). Defined as one month so
/// "reads as `Nmo`" and "counts as stale" flip at the same point —
/// [`is_stale_at`] and [`time_ago_at`]'s months boundary stay in lockstep
/// by construction. Changing the staleness window (e.g. to two months)
/// only moves the fade; the formatter keeps switching to months at
/// [`DAYS_PER_MONTH`] regardless, so no day/month display gap can open.
pub const STALE_AGE_DAYS: i64 = DAYS_PER_MONTH;

/// Whether `then` is at least [`STALE_AGE_DAYS`] old relative to `now`.
pub fn is_stale_at(then: &DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now.signed_duration_since(then).num_days() >= STALE_AGE_DAYS
}

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
    if days < DAYS_PER_MONTH {
        return format!("{days}d ago");
    }

    // 30-day months don't tile a 365-day year (12 × 30 = 360), so gate
    // on `days` rather than `months`: without this, ages of 360–364 days
    // skip the months branch (`months == 12`) yet still floor to 0 years.
    let months = days / DAYS_PER_MONTH;
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
    fn is_stale_at_flips_at_the_months_boundary() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let fresh = now - chrono::Duration::days(STALE_AGE_DAYS - 1);
        let boundary = now - chrono::Duration::days(STALE_AGE_DAYS);
        assert!(!is_stale_at(&fresh, now));
        assert!(is_stale_at(&boundary, now));
    }

    /// Lockstep invariant (issue #274): the first day an item counts as
    /// stale is the first day it reads in months. A fade on a row still
    /// showing `29d`, or a `2mo` row that isn't faded, would look broken —
    /// this pins that it can't happen. Guards against decoupling
    /// `STALE_AGE_DAYS` from the formatter's `DAYS_PER_MONTH` boundary.
    #[test]
    fn staleness_and_months_display_stay_in_lockstep() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let last_fresh_day = STALE_AGE_DAYS - 1;
        assert!(!is_stale_at(
            &(now - chrono::Duration::days(last_fresh_day)),
            now
        ));
        assert_eq!(
            ago(now, last_fresh_day, 0),
            format!("{last_fresh_day}d ago")
        );
        assert!(is_stale_at(
            &(now - chrono::Duration::days(STALE_AGE_DAYS)),
            now
        ));
        assert_eq!(ago(now, STALE_AGE_DAYS, 0), "1mo ago");
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
