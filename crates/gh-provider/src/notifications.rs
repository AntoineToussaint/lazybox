//! GitHub Notifications API client.
//!
//! ## Why this exists
//!
//! GraphQL search (the workhorse of [`crate::client::GhClient::fetch_all_prs`])
//! is expensive — every poll cycle deserializes ~85 sub-objects per PR
//! whether anything changed or not. GitHub's REST `/notifications`
//! endpoint is the right primitive for "did anything actually change":
//! it supports `If-Modified-Since` and returns `304 Not Modified` with
//! an empty body when nothing's new, which costs a single round-trip
//! and zero parsing.
//!
//! The notifications API is rate-budgeted SEPARATELY from GraphQL
//! (REST has its own 5000-req/hr quota), so the heartbeat doesn't
//! compete with the search budget — that's why we add it as a fast
//! tier on top of the existing slow full-sweep.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::sync::Mutex;

/// Outcome of one `GET /notifications` call.
///
/// `NotModified` is the steady-state "nothing happened" answer; the
/// polling driver returns immediately without paying the GraphQL
/// cost of a deep-fetch. `Modified` carries the entries to fan out
/// targeted single-PR / single-issue fetches against.
#[derive(Debug)]
pub enum NotificationsPoll {
    /// `304 Not Modified` — nothing changed since the last `If-Modified-Since`.
    /// The polling driver can return immediately; the tick is a no-op.
    NotModified,
    /// `200 OK` with a (possibly empty) list of notification entries.
    /// The response's `Last-Modified` header is captured into
    /// `NotificationsState` (crate-private) for the next call's `If-Modified-Since`;
    /// callers don't need it directly.
    Modified { entries: Vec<NotificationEntry> },
}

/// Subset of the notification fields we actually use.
///
/// `repository.full_name` (e.g. `"acme/widget"`) plus `subject.url`
/// (e.g. `"https://api.github.com/repos/acme/widget/pulls/186"`) give
/// us everything needed for the targeted deep-fetch. Extra fields are
/// ignored — staying small keeps the deserializer robust to non-breaking
/// upstream additions.
///
/// `#[serde(default)]` everywhere because the notifications endpoint
/// has a long-standing reputation for returning partially-populated
/// rows (e.g. notifications for repos the token can no longer see still
/// arrive with a `null` subject.url). A `serde::Deserialize` failure on
/// one bad row would otherwise kill the whole tick.
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationEntry {
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub subject: NotificationSubject,
    #[serde(default)]
    pub repository: Option<NotificationRepo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NotificationSubject {
    #[serde(default)]
    pub title: String,
    /// e.g. `"https://api.github.com/repos/acme/widget/pulls/186"` for a
    /// PR, `".../issues/186"` for an issue. Optional because GitHub
    /// occasionally returns nulls for deleted/inaccessible subjects.
    #[serde(default)]
    pub url: Option<String>,
    /// `PullRequest`, `Issue`, `Discussion`, `Release`, ...
    /// `Discussion` and `Release` are out of scope; we just skip them.
    #[serde(default, rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NotificationRepo {
    /// `"owner/repo"`. Always present in practice.
    #[serde(default)]
    pub full_name: String,
}

/// Resolved `(owner, repo, number, kind)` derived from a notification
/// subject. Parsing centralizes here so the polling layer doesn't
/// have to know about the GitHub REST URL shape.
/// `Ord` is derived so callers can dedup notifications via a
/// `BTreeSet<NotificationTarget>` — GitHub fires multiple notifications
/// per PR within a single window (one per comment, one per CI status
/// change) and the targeted-fetch fan-out would otherwise issue
/// redundant single-PR queries.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NotificationTarget {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub kind: NotificationTargetKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NotificationTargetKind {
    PullRequest,
    Issue,
}

impl NotificationEntry {
    /// Pull the `(owner, repo, number, kind)` tuple needed for a
    /// targeted deep-fetch. Returns `None` for notifications we don't
    /// know how to act on (Discussions, Releases, malformed subjects).
    pub fn target(&self) -> Option<NotificationTarget> {
        let url = self.subject.url.as_deref()?;
        parse_subject_url(url)
    }
}

/// Parse a notification `subject.url` into `(owner, repo, number, kind)`.
///
/// Accepted shapes (anything else returns `None` so the caller can skip
/// gracefully):
/// - `https://api.github.com/repos/<owner>/<repo>/pulls/<N>` → PR
/// - `https://api.github.com/repos/<owner>/<repo>/issues/<N>` → Issue
///
/// Tolerates trailing slashes and query strings. Does NOT try to
/// dereference `latest_comment_url` — that points to a comment, not the
/// underlying PR/issue, and the single-PR deep-fetch only needs the
/// canonical number anyway.
///
/// `pub(crate)` instead of `pub`: production callers go through
/// [`NotificationEntry::target`]; the only out-of-module consumer is
/// the test suite below. Two parallel public entry points would invite
/// drift between them.
pub(crate) fn parse_subject_url(url: &str) -> Option<NotificationTarget> {
    let stripped = url.split_once("/repos/").map(|(_, rest)| rest)?;
    // Drop any trailing `?query` / `#fragment` — keeps the segment
    // parser focused on the path-only shape we expect. `str::split`
    // always yields at least one element, so the `expect` is a
    // contract reminder, not a fallible branch.
    let path = stripped
        .split(['?', '#'])
        .next()
        .expect("str::split yields at least one element")
        .trim_end_matches('/');
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let repo = segments.next()?;
    let kind = match segments.next()? {
        "pulls" => NotificationTargetKind::PullRequest,
        "issues" => NotificationTargetKind::Issue,
        _ => return None,
    };
    let number: u64 = segments.next()?.parse().ok()?;
    Some(NotificationTarget {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
        kind,
    })
}

/// Persistent state for the notifications heartbeat that needs to
/// outlive a single tick.
///
/// `last_modified` is GitHub's `Last-Modified` header from the most
/// recent successful 200 — we echo it back as `If-Modified-Since` so
/// the subsequent calls can answer 304 cheaply. `last_full_sweep_at`
/// drives the slow-sweep cadence (see [`super::client::GhClient::should_full_sweep`]).
///
/// Wrapped in `Mutex` because `GhClient` is cheap-cloneable across
/// ticks via `Arc`; one bucket, many handles.
#[derive(Debug, Default)]
pub(crate) struct NotificationsState {
    /// HTTP date string from GitHub's `Last-Modified` header (e.g.
    /// `"Sun, 06 Nov 2026 08:49:37 GMT"`). Storing the exact server
    /// value (rather than locally-formatted `now`) keeps the round-trip
    /// byte-for-byte consistent — GitHub matches on the literal value
    /// when deciding 304 vs 200.
    pub(crate) last_modified: Option<String>,
    /// When the last full sweep completed. `None` means "never" — the
    /// first tick after daemon start always runs a full sweep to
    /// bootstrap the inbox.
    pub(crate) last_full_sweep_at: Option<std::time::Instant>,
    /// "Skip the cheap heartbeat round-trip and go straight to full
    /// sweep" deadline. Armed by `note_heartbeat_failed` when the
    /// notifications endpoint errors (auth scope missing, REST
    /// rate-limit, 5xx) — without this, a chronically-broken
    /// heartbeat would pay BOTH the failed REST round-trip AND the
    /// full GraphQL sweep on every tick. Cleared by
    /// `note_heartbeat_succeeded` once the endpoint recovers.
    pub(crate) heartbeat_back_off_until: Option<std::time::Instant>,
    /// One-shot "force the next sweep" flag. Set by a manual
    /// `Command::Refresh` so the next tick runs a full `involves:USER`
    /// search regardless of where we are in the `FULL_SWEEP_INTERVAL`
    /// window — a freshly created issue produces no notification for
    /// its creator, so the incremental path would never surface it
    /// (issue #180). Cleared once the sweep completes
    /// (`mark_full_sweep_done`).
    pub(crate) force_full_sweep: bool,
}

impl NotificationsState {
    /// Is the slow-sweep cadence due?
    ///
    /// True in any of four cases:
    ///   - a manual refresh forced the next sweep (`force_full_sweep`),
    ///   - no sweep has run yet (bootstrap, first tick after daemon start),
    ///   - the configured `threshold` has elapsed since the last sweep,
    ///   - the notifications heartbeat is currently in a back-off window
    ///     (a chronic heartbeat failure → skip the round-trip and just
    ///     do the sweep until it recovers).
    ///
    /// Centralized here (rather than inlined in `GhClient`) so the
    /// timer arithmetic can be unit-tested without spinning up a real
    /// client.
    pub fn is_full_sweep_due(&self, threshold: std::time::Duration) -> bool {
        if self.force_full_sweep {
            return true;
        }
        if self.heartbeat_backed_off() {
            return true;
        }
        match self.last_full_sweep_at {
            None => true,
            Some(t) => t.elapsed() >= threshold,
        }
    }

    /// True when a previous heartbeat failure has armed a back-off
    /// window that hasn't yet expired. Public-to-crate because the
    /// `should_full_sweep` path consults it; also surfaced via
    /// `NotificationsSnapshot` for the status indicator.
    pub(crate) fn heartbeat_backed_off(&self) -> bool {
        self.heartbeat_back_off_until
            .is_some_and(|deadline| std::time::Instant::now() < deadline)
    }

    /// Construct a fresh `Arc<Mutex<NotificationsState>>` — the shape
    /// `GhClient` keeps so `with_filters` clones can share one bucket
    /// of conditional-GET state. `pub(crate)` because this is an
    /// implementation detail of the client, not part of the public
    /// notifications API.
    pub(crate) fn shared() -> SharedNotificationsState {
        std::sync::Arc::new(Mutex::new(Self::default()))
    }
}

/// Thread-safe handle on shared notifications state. `pub(crate)`
/// because only `GhClient` and its module construct or hold one;
/// callers observe state via [`NotificationsSnapshot`].
pub(crate) type SharedNotificationsState = std::sync::Arc<Mutex<NotificationsState>>;

/// Read-only view of the notifications heartbeat state, returned by
/// [`super::client::GhClient::notifications_snapshot`]. Tests and a
/// future status indicator observe whether the slow-sweep timer is
/// armed without holding the lock or learning the storage shape.
#[derive(Debug, Clone)]
pub struct NotificationsSnapshot {
    pub has_last_modified: bool,
    pub last_full_sweep_elapsed: Option<std::time::Duration>,
    /// True when the heartbeat is in a back-off window (chronic
    /// failure). The polling layer is currently skipping the cheap
    /// notifications round-trip; full sweeps still run.
    pub heartbeat_backed_off: bool,
}

/// Format a `DateTime<Utc>` as an HTTP-date (RFC 7231 IMF-fixdate),
/// e.g. `"Sun, 06 Nov 2026 08:49:37 GMT"`. `pub(crate)` because the
/// only producer in non-test code is `GhClient::mark_full_sweep_done`.
pub(crate) fn format_http_date(dt: DateTime<Utc>) -> String {
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Parse an HTTP-date string back to `DateTime<Utc>`. Tolerant of the
/// IMF-fixdate form GitHub uses; returns `None` on anything weirder.
/// Only used in tests today.
#[cfg(test)]
pub fn parse_http_date(s: &str) -> Option<DateTime<Utc>> {
    use chrono::{NaiveDateTime, TimeZone};
    NaiveDateTime::parse_from_str(s, "%a, %d %b %Y %H:%M:%S GMT")
        .ok()
        .and_then(|naive| Utc.from_local_datetime(&naive).single())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parse_pull_request_subject() {
        let t = parse_subject_url("https://api.github.com/repos/acme/widget/pulls/186").unwrap();
        assert_eq!(t.owner, "acme");
        assert_eq!(t.repo, "widget");
        assert_eq!(t.number, 186);
        assert_eq!(t.kind, NotificationTargetKind::PullRequest);
    }

    #[test]
    fn parse_issue_subject() {
        let t = parse_subject_url("https://api.github.com/repos/acme/widget/issues/12").unwrap();
        assert_eq!(t.owner, "acme");
        assert_eq!(t.repo, "widget");
        assert_eq!(t.number, 12);
        assert_eq!(t.kind, NotificationTargetKind::Issue);
    }

    #[test]
    fn parse_handles_trailing_slash() {
        let t = parse_subject_url("https://api.github.com/repos/o/r/pulls/1/").unwrap();
        assert_eq!(t.number, 1);
    }

    #[test]
    fn parse_handles_query_string() {
        // GitHub sometimes appends a query — strip before segmenting
        // so the number parse doesn't trip over `1?foo=bar`.
        let t = parse_subject_url("https://api.github.com/repos/o/r/pulls/1?foo=bar").unwrap();
        assert_eq!(t.number, 1);
    }

    #[test]
    fn parse_rejects_discussion_subject() {
        // Discussions and Releases use `/discussions/` and `/releases/`
        // — out of scope; return None so the caller can `continue`.
        assert!(parse_subject_url("https://api.github.com/repos/o/r/discussions/1").is_none());
        assert!(parse_subject_url("https://api.github.com/repos/o/r/releases/1").is_none());
    }

    #[test]
    fn parse_rejects_malformed_url() {
        assert!(parse_subject_url("not-a-url").is_none());
        assert!(parse_subject_url("https://api.github.com/repos/o/r/pulls/notanumber").is_none());
        assert!(parse_subject_url("").is_none());
    }

    #[test]
    fn notification_entry_target_round_trip() {
        // Plumbed through the actual Deserialize path because that's
        // what the wire data flows through — a unit-only test on
        // parse_subject_url wouldn't catch a missed `#[serde(default)]`.
        let json = r#"{
            "reason": "review_requested",
            "updated_at": "2026-05-28T12:00:00Z",
            "subject": {
                "title": "Add foo",
                "url": "https://api.github.com/repos/acme/widget/pulls/186",
                "type": "PullRequest"
            },
            "repository": { "full_name": "acme/widget" }
        }"#;
        let entry: NotificationEntry = serde_json::from_str(json).unwrap();
        let t = entry.target().unwrap();
        assert_eq!(t.owner, "acme");
        assert_eq!(t.number, 186);
        assert_eq!(t.kind, NotificationTargetKind::PullRequest);
    }

    #[test]
    fn notification_entry_tolerates_missing_subject_url() {
        // GitHub sometimes ships notifications with a null subject.url
        // (e.g. when the underlying repo is gone). Must NOT crash the
        // deserializer; `target()` returns None and the caller skips.
        let json = r#"{
            "reason": "subscribed",
            "subject": { "title": "x", "type": "PullRequest" }
        }"#;
        let entry: NotificationEntry = serde_json::from_str(json).unwrap();
        assert!(entry.target().is_none());
    }

    #[test]
    fn fresh_state_demands_full_sweep() {
        // Bootstrap case — daemon just started, no sweep has run.
        // Must report due so the first tick fans out the heavy
        // `involves:USER` search and seeds the inbox.
        let s = NotificationsState::default();
        assert!(s.is_full_sweep_due(std::time::Duration::from_secs(600)));
    }

    #[test]
    fn heartbeat_back_off_forces_full_sweep_even_if_timer_says_no() {
        // The whole point of the back-off: a chronic heartbeat
        // failure shouldn't pay the round-trip on every tick. Even
        // when the slow-sweep timer thinks "incremental is fine right
        // now," `is_full_sweep_due` must return true while the
        // back-off window is active so the polling layer skips the
        // heartbeat and goes straight to the sweep.
        let s = NotificationsState {
            // Sweep happened RIGHT now — timer alone wouldn't promote.
            last_full_sweep_at: Some(std::time::Instant::now()),
            // Back-off armed 100ms into the future.
            heartbeat_back_off_until: Some(
                std::time::Instant::now() + std::time::Duration::from_millis(100),
            ),
            ..Default::default()
        };
        assert!(s.heartbeat_backed_off());
        assert!(s.is_full_sweep_due(std::time::Duration::from_secs(600)));
    }

    #[test]
    fn expired_heartbeat_back_off_does_not_force_full_sweep() {
        // Back-off in the past = expired. Once it expires, the timer
        // alone gates the decision again — a fresh sweep means the
        // heartbeat resumes on the next tick.
        let past = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let s = NotificationsState {
            last_full_sweep_at: Some(std::time::Instant::now()),
            heartbeat_back_off_until: Some(past),
            ..Default::default()
        };
        assert!(!s.heartbeat_backed_off());
        assert!(!s.is_full_sweep_due(std::time::Duration::from_secs(600)));
    }

    #[test]
    fn recent_sweep_blocks_full_sweep_until_threshold_elapses() {
        // The slow-sweep cadence is what makes notifications-driven
        // sync cheap: between sweeps, every tick takes the cheap
        // notifications-heartbeat path. Verify the timer does its job.
        let s = NotificationsState {
            last_full_sweep_at: Some(std::time::Instant::now()),
            ..Default::default()
        };
        assert!(!s.is_full_sweep_due(std::time::Duration::from_secs(600)));
        // A zero threshold means "always sweep" — degenerate but the
        // arithmetic should still hold.
        assert!(s.is_full_sweep_due(std::time::Duration::from_secs(0)));
    }

    #[test]
    fn forced_sweep_overrides_recent_timer() {
        // Issue #180: a manual `Command::Refresh` arms `force_full_sweep`
        // so the next tick runs the heavy `involves:USER` search even
        // when the timer just ran a sweep — otherwise a freshly created
        // issue (no self-notification) wouldn't surface until the next
        // scheduled sweep, up to 10 min away.
        let s = NotificationsState {
            last_full_sweep_at: Some(std::time::Instant::now()),
            force_full_sweep: true,
            ..Default::default()
        };
        assert!(s.is_full_sweep_due(std::time::Duration::from_secs(600)));
    }

    #[test]
    fn http_date_round_trip() {
        // The header value we read from GitHub gets echoed back on the
        // next request — make sure our local synthesis path produces
        // a string the parse path agrees with, so a tester can swap
        // either side without confusion.
        let dt = Utc.with_ymd_and_hms(2026, 5, 28, 12, 34, 56).unwrap();
        let s = format_http_date(dt);
        assert_eq!(s, "Thu, 28 May 2026 12:34:56 GMT");
        assert_eq!(parse_http_date(&s).unwrap(), dt);
    }
}
