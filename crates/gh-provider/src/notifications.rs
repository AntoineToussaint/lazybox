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
use parking_lot::Mutex;
use serde::Deserialize;

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
    ///
    /// `last_modified` carries the response's `Last-Modified` header
    /// (when GitHub sent one) as the *pending* cursor for the next
    /// call's `If-Modified-Since`. It is deliberately NOT committed to
    /// `NotificationsState` at parse time: the cursor advance is coupled
    /// to work completion (#512). The polling layer fans out the
    /// per-entry deep-fetches and only calls
    /// [`super::client::GhClient::commit_notifications_cursor`] once every
    /// entry resolved — a transient deep-fetch `Err` holds the cursor so
    /// the un-fetched entry re-lists on the next heartbeat instead of
    /// being skipped forever by a premature 304.
    Modified {
        entries: Vec<NotificationEntry>,
        last_modified: Option<String>,
    },
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
    /// Server-directed minimum cadence from `X-Poll-Interval`.
    pub(crate) poll_interval: Option<std::time::Duration>,
    pub(crate) last_poll_at: Option<std::time::Instant>,
    /// When the last full sweep completed. `None` means "never" — the
    /// first tick after daemon start always runs a full sweep to
    /// bootstrap the inbox.
    pub(crate) last_full_sweep_at: Option<DateTime<Utc>>,
    /// "Skip the cheap heartbeat round-trip and go straight to full
    /// sweep" deadline. Armed by `note_heartbeat_failed` when the
    /// notifications endpoint errors (auth scope missing, REST
    /// rate-limit, 5xx) — without this, a chronically-broken
    /// heartbeat would pay BOTH the failed REST round-trip AND the
    /// full GraphQL sweep on every tick. Cleared by
    /// `note_heartbeat_succeeded` once the endpoint recovers.
    pub(crate) heartbeat_back_off_until: Option<std::time::Instant>,
    /// One catch-up full sweep owed because the heartbeat just failed
    /// (#1218): without the notifications channel we may have missed
    /// changes, so the NEXT tick sweeps — but only once per backed-off
    /// episode. Ticks after that stay hot-only until the normal sweep
    /// cadence elapses; the old behavior (every backed-off tick a full
    /// sweep, for the whole 10-minute window) burned the most quota
    /// exactly when the bucket was already exhausted.
    pub(crate) backoff_catchup_sweep_due: bool,
    /// One-shot "force the next sweep" flag. Set by a manual
    /// `Command::Refresh` so the next tick runs a full `involves:USER`
    /// search regardless of where we are in the `FULL_SWEEP_INTERVAL`
    /// window — a freshly created issue produces no notification for
    /// its creator, so the incremental path would never surface it
    /// (issue #180). Cleared once the sweep completes
    /// (`mark_full_sweep_done`).
    pub(crate) force_full_sweep: bool,
    /// Start time of the most recent GLOBAL `involves:` PR sweep, used
    /// as the `updated:>=` floor for the next windowed sweep (issue
    /// #14). Captured before the fetch so PRs touched mid-sweep are
    /// caught next time. `None` until the first global sweep completes.
    pub(crate) last_pr_sweep_at_utc: Option<DateTime<Utc>>,
    /// Start time of the most recent GLOBAL merged-sweep that actually
    /// SUCCEEDED, used as the `updated:>=` floor for the next windowed
    /// merged sweep (issue #530). Tracked separately from
    /// `last_pr_sweep_at_utc` because the merged branch is best-effort:
    /// the main branch can succeed (advancing that floor) while the
    /// merged branch fails, and reusing the main floor would then window
    /// past a merge the failed branch never reported, stranding the PR
    /// on `OPEN` until the next reconcile. `None` until the first
    /// successful global merged sweep.
    pub(crate) last_merged_sweep_at_utc: Option<DateTime<Utc>>,
    /// When the last UNWINDOWED reconcile sweep completed. `None` means
    /// "never" — the first global sweep is always a reconcile. Drives
    /// [`is_full_reconcile_due`](Self::is_full_reconcile_due): most
    /// global sweeps narrow to `updated:>=last_pr_sweep_at_utc`, but a
    /// windowed search can't see PRs that left the window without an
    /// update (silently closed, un-involved, transferred), so one
    /// sweep per `FULL_RECONCILE_INTERVAL` drops the window.
    pub(crate) last_full_reconcile_at: Option<DateTime<Utc>>,
}

impl NotificationsState {
    /// Is the slow-sweep cadence due?
    ///
    /// True in any of four cases:
    ///   - a manual refresh forced the next sweep (`force_full_sweep`),
    ///   - no sweep has run yet (bootstrap, first tick after daemon start),
    ///   - the configured `threshold` has elapsed since the last sweep,
    ///   - the heartbeat just failed and its one catch-up sweep hasn't
    ///     run yet (`backoff_catchup_sweep_due`) — after that, a
    ///     backed-off heartbeat leaves ticks hot-only until the normal
    ///     cadence elapses (#1218: promoting every backed-off tick to a
    ///     full sweep burned quota precisely during exhaustion).
    ///
    /// Centralized here (rather than inlined in `GhClient`) so the
    /// timer arithmetic can be unit-tested without spinning up a real
    /// client.
    pub fn is_full_sweep_due(&self, threshold: std::time::Duration) -> bool {
        self.is_full_sweep_due_at(threshold, Utc::now())
    }

    fn is_full_sweep_due_at(&self, threshold: std::time::Duration, now: DateTime<Utc>) -> bool {
        if self.force_full_sweep {
            return true;
        }
        if self.backoff_catchup_sweep_due {
            return true;
        }
        match self.last_full_sweep_at {
            None => true,
            Some(t) if t > now => true,
            Some(t) => now
                .signed_duration_since(t)
                .to_std()
                .is_ok_and(|elapsed| elapsed >= threshold),
        }
    }

    pub fn heartbeat_due(&self, now: std::time::Instant) -> bool {
        match (self.last_poll_at, self.poll_interval) {
            (Some(last), Some(interval)) => now
                .checked_duration_since(last)
                .is_none_or(|elapsed| elapsed >= interval),
            _ => true,
        }
    }

    /// Is an UNWINDOWED reconcile sweep due? Distinct from
    /// [`is_full_sweep_due`](Self::is_full_sweep_due): that routes
    /// full-vs-notifications every tick; this decides, once we're
    /// already running a global sweep, whether to drop the
    /// `updated:>=` window and re-reconcile the whole inbox (issue
    /// #14).
    ///
    /// True when a manual refresh forced it, when no reconcile has run
    /// yet (bootstrap), or when `threshold` has elapsed since the last
    /// one. A heartbeat back-off is deliberately NOT a trigger: a
    /// windowed sweep still catches every changed *open* PR, and a
    /// broken heartbeat shouldn't force the heavy full download every
    /// cycle.
    pub fn is_full_reconcile_due(&self, threshold: std::time::Duration) -> bool {
        self.is_full_reconcile_due_at(threshold, Utc::now())
    }

    fn is_full_reconcile_due_at(&self, threshold: std::time::Duration, now: DateTime<Utc>) -> bool {
        if self.force_full_sweep {
            return true;
        }
        match self.last_full_reconcile_at {
            None => true,
            Some(t) if t > now => true,
            Some(t) => now
                .signed_duration_since(t)
                .to_std()
                .is_ok_and(|elapsed| elapsed >= threshold),
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

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SyncCursors {
    pub last_modified: Option<String>,
    pub last_full_sweep_at: Option<DateTime<Utc>>,
    pub last_pr_sweep_at: Option<DateTime<Utc>>,
    pub last_merged_sweep_at: Option<DateTime<Utc>>,
    pub last_full_reconcile_at: Option<DateTime<Utc>>,
}

impl NotificationsState {
    pub(crate) fn cursors(&self) -> SyncCursors {
        SyncCursors {
            last_modified: self.last_modified.clone(),
            last_full_sweep_at: self.last_full_sweep_at,
            last_pr_sweep_at: self.last_pr_sweep_at_utc,
            last_merged_sweep_at: self.last_merged_sweep_at_utc,
            last_full_reconcile_at: self.last_full_reconcile_at,
        }
    }

    pub(crate) fn restore_cursors(&mut self, cursors: SyncCursors, now: DateTime<Utc>) {
        let valid = |wall: Option<DateTime<Utc>>| wall.filter(|value| *value <= now);
        self.last_modified = cursors.last_modified;
        self.last_full_sweep_at = valid(cursors.last_full_sweep_at);
        self.last_pr_sweep_at_utc = valid(cursors.last_pr_sweep_at);
        self.last_merged_sweep_at_utc = valid(cursors.last_merged_sweep_at);
        self.last_full_reconcile_at = valid(cursors.last_full_reconcile_at);
    }
}

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
    fn server_poll_interval_is_a_hard_minimum() {
        let now = std::time::Instant::now();
        let state = NotificationsState {
            poll_interval: Some(std::time::Duration::from_secs(60)),
            last_poll_at: Some(now),
            ..NotificationsState::default()
        };
        assert!(!state.heartbeat_due(now + std::time::Duration::from_secs(59)));
        assert!(state.heartbeat_due(now + std::time::Duration::from_secs(60)));
    }

    #[test]
    fn successful_branch_cursors_survive_restart_round_trip() {
        let now = Utc::now();
        let state = NotificationsState {
            last_modified: Some("Wed, 30 Jul 2026 10:00:00 GMT".to_string()),
            last_full_sweep_at: Some(now - chrono::Duration::seconds(120)),
            last_pr_sweep_at_utc: Some(now - chrono::Duration::minutes(2)),
            last_merged_sweep_at_utc: Some(now - chrono::Duration::minutes(3)),
            last_full_reconcile_at: Some(now - chrono::Duration::seconds(1800)),
            ..NotificationsState::default()
        };
        let encoded = serde_json::to_string(&state.cursors()).expect("serialize cursors");
        let cursors: SyncCursors = serde_json::from_str(&encoded).expect("deserialize cursors");
        let mut restored = NotificationsState::default();
        restored.restore_cursors(cursors, now);

        assert_eq!(restored.last_modified, state.last_modified);
        assert_eq!(restored.last_pr_sweep_at_utc, state.last_pr_sweep_at_utc);
        assert_eq!(
            restored.last_merged_sweep_at_utc,
            state.last_merged_sweep_at_utc
        );
        assert!(restored.last_full_sweep_at.is_some());
        assert!(restored.last_full_reconcile_at.is_some());
    }

    #[test]
    fn restart_preserves_elapsed_age_and_rejects_future_cursors() {
        let now = Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap();
        let mut old = NotificationsState::default();
        old.restore_cursors(
            SyncCursors {
                last_full_sweep_at: Some(now - chrono::Duration::hours(2)),
                last_full_reconcile_at: Some(now - chrono::Duration::hours(2)),
                ..SyncCursors::default()
            },
            now,
        );
        assert!(old.is_full_sweep_due_at(std::time::Duration::from_secs(600), now));
        assert!(old.is_full_reconcile_due_at(std::time::Duration::from_secs(3600), now));

        let mut future = NotificationsState::default();
        future.restore_cursors(
            SyncCursors {
                last_full_sweep_at: Some(now + chrono::Duration::hours(1)),
                last_pr_sweep_at: Some(now + chrono::Duration::hours(1)),
                last_merged_sweep_at: Some(now + chrono::Duration::hours(1)),
                last_full_reconcile_at: Some(now + chrono::Duration::hours(1)),
                ..SyncCursors::default()
            },
            now,
        );
        assert!(future.is_full_sweep_due_at(std::time::Duration::from_secs(600), now));
        assert!(future.is_full_reconcile_due_at(std::time::Duration::from_secs(3600), now));
        assert!(future.last_pr_sweep_at_utc.is_none());
        assert!(future.last_merged_sweep_at_utc.is_none());
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
        let mut s = NotificationsState {
            // Sweep happened RIGHT now — timer alone wouldn't promote.
            last_full_sweep_at: Some(Utc::now()),
            // Back-off armed 100ms into the future, catch-up owed.
            heartbeat_back_off_until: Some(
                std::time::Instant::now() + std::time::Duration::from_millis(100),
            ),
            backoff_catchup_sweep_due: true,
            ..Default::default()
        };
        assert!(s.heartbeat_backed_off());
        assert!(
            s.is_full_sweep_due(std::time::Duration::from_secs(600)),
            "the owed catch-up sweep promotes exactly one tick",
        );
        // Catch-up consumed (mark_full_sweep_done clears it): the still-
        // backed-off heartbeat no longer promotes every tick — hot-only
        // until the normal cadence elapses (#1218).
        s.backoff_catchup_sweep_due = false;
        assert!(s.heartbeat_backed_off());
        assert!(
            !s.is_full_sweep_due(std::time::Duration::from_secs(600)),
            "a backed-off heartbeat alone must not promote full sweeps",
        );
    }

    #[test]
    fn expired_heartbeat_back_off_does_not_force_full_sweep() {
        // Back-off in the past = expired. Once it expires, the timer
        // alone gates the decision again — a fresh sweep means the
        // heartbeat resumes on the next tick.
        let past = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let s = NotificationsState {
            last_full_sweep_at: Some(Utc::now()),
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
            last_full_sweep_at: Some(Utc::now()),
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
            last_full_sweep_at: Some(Utc::now()),
            force_full_sweep: true,
            ..Default::default()
        };
        assert!(s.is_full_sweep_due(std::time::Duration::from_secs(600)));
    }

    #[test]
    fn fresh_state_demands_reconcile() {
        // Bootstrap: never reconciled → the first global sweep must run
        // unwindowed so it seeds the whole inbox (issue #14).
        let s = NotificationsState::default();
        assert!(s.is_full_reconcile_due(std::time::Duration::from_secs(3600)));
    }

    #[test]
    fn recent_reconcile_allows_windowed_sweep() {
        // A reconcile just ran → the next global sweep can narrow to
        // `updated:>=` instead of re-downloading every PR.
        let s = NotificationsState {
            last_full_reconcile_at: Some(Utc::now()),
            ..Default::default()
        };
        assert!(!s.is_full_reconcile_due(std::time::Duration::from_secs(3600)));
        // Degenerate zero threshold = always reconcile.
        assert!(s.is_full_reconcile_due(std::time::Duration::from_secs(0)));
    }

    #[test]
    fn forced_sweep_overrides_recent_reconcile_timer() {
        // A manual refresh must reconcile (drop the window) so a row
        // that silently left the search — closed with no comment,
        // un-involved — is reconciled immediately, not at the next
        // scheduled reconcile up to an hour away.
        let s = NotificationsState {
            last_full_reconcile_at: Some(Utc::now()),
            force_full_sweep: true,
            ..Default::default()
        };
        assert!(s.is_full_reconcile_due(std::time::Duration::from_secs(3600)));
    }

    #[test]
    fn heartbeat_back_off_does_not_force_reconcile() {
        // Unlike `is_full_sweep_due`, a broken heartbeat must NOT force
        // the heavy unwindowed download every cycle — a windowed sweep
        // still catches every changed open PR.
        let s = NotificationsState {
            last_full_reconcile_at: Some(Utc::now()),
            heartbeat_back_off_until: Some(
                std::time::Instant::now() + std::time::Duration::from_secs(60),
            ),
            ..Default::default()
        };
        assert!(s.heartbeat_backed_off());
        assert!(!s.is_full_reconcile_due(std::time::Duration::from_secs(3600)));
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
