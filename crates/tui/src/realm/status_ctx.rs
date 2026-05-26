//! `StatusCtx` — transient UI status: the footer notice and the
//! first-poll spinner. Extracted from `Model` so the three related
//! fields (and the tick / fade helpers that manage them) live in
//! one place instead of being scattered across the orchestrator.

use crate::realm::components::footer::{Notice, NoticeSeverity};
use crate::realm::components::polling::Polling;
use crate::realm::model::Msg;
use std::time::{Duration, Instant};

/// How long retryable notices stay visible before fading. Permanent
/// + auth notices ignore this — they stay until dismissed.
const RETRYABLE_FADE: Duration = Duration::from_secs(5);
/// How long info notices ("Spawning shell…", "Setup saved", etc.)
/// stay visible if their triggering event never lands. Longer than
/// retryable so a slow worktree creation doesn't fade mid-flight.
const INFO_FADE: Duration = Duration::from_secs(15);
/// One-shot hints (e.g. "scroll: this view manages its own scrollback")
/// fade quickly so they don't follow the user around the UI.
const HINT_FADE: Duration = Duration::from_secs(3);
/// Heartbeat interval for the polling-modal spinner. Cheap, keeps
/// the spinner glyph advancing at ~12 fps.
const POLLING_TICK_INTERVAL: Duration = Duration::from_millis(80);

/// Spinner frames shared with the initial-poll modal. Subset is fine
/// — the background indicator only renders the current frame.
const BG_SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// How long a background poll keeps its footer indicator after the
/// last progress event arrived. Acts as the "fall back to idle"
/// timeout when a source finishes without an explicit PollCompleted
/// — keeps the spinner from spinning forever if the daemon drops
/// the completion event.
///
/// Bumped from 20s → 90s. The previous 20s cap was clearing the
/// spinner on real, in-flight polls — GitHub's PR-details fan-out
/// runs ~50 GraphQL calls per cycle, and a small handful of those
/// can sit on a 15s socket-level retry without emitting a
/// `PollProgress`. The user perception was "sync stuck at 20s"
/// even though the underlying poll was healthy.
const BG_POLL_IDLE_GUARD: Duration = Duration::from_secs(90);

/// Past this elapsed time without a PollCompleted, the footer
/// label switches from `syncing github · Ns` to
/// `syncing github · Ns · still working`. Visual signal that
/// pilot hasn't given up — useful on slow networks where a
/// 60+ second poll is normal.
const STILL_WORKING_HINT_AFTER: Duration = Duration::from_secs(30);

/// Lightweight footer indicator for poll cycles AFTER the initial
/// blocking modal has been dismissed. Lit up on any `PollProgress`,
/// cleared on `PollCompleted` or after the guard timeout.
pub(crate) struct BackgroundPoll {
    pub source: String,
    pub message: String,
    pub spinner_idx: usize,
    pub last_event: Instant,
    pub started_at: Instant,
}

impl BackgroundPoll {
    pub fn spinner_glyph(&self) -> &'static str {
        BG_SPINNER_FRAMES[self.spinner_idx % BG_SPINNER_FRAMES.len()]
    }
    /// Footer label — `syncing github · 2s` once a cycle has been in
    /// flight long enough to be worth surfacing the elapsed time.
    /// Below ~1s we skip the suffix so the indicator doesn't flicker
    /// `0s → 1s` on fast polls. Past `STILL_WORKING_HINT_AFTER`
    /// the label gains a `· still working` suffix so a slow but
    /// healthy poll reads as "in progress" instead of "stuck."
    pub fn label(&self) -> String {
        let elapsed = self.started_at.elapsed();
        let secs = elapsed.as_secs();
        if secs < 1 {
            return format!("syncing {}", self.source);
        }
        if elapsed >= STILL_WORKING_HINT_AFTER {
            format!("syncing {} · {secs}s · still working", self.source)
        } else {
            format!("syncing {} · {secs}s", self.source)
        }
    }
}

pub(crate) struct StatusCtx {
    /// Most recent footer notice — error, warning, or info. Replaces
    /// the modal-on-every-error UX. Retryable severities auto-fade
    /// after `RETRYABLE_FADE`; permanent + auth stay until cleared.
    pub notice: Option<Notice>,
    /// First-poll progress modal. Set by the on-setup-complete hook
    /// (and the returning-user kickoff path) so users see "Pulling
    /// from github + linear…" instead of an empty sidebar while the
    /// initial poll cycle runs. Cleared on first `WorkspaceUpserted`,
    /// timeout, or any-key dismiss.
    pub polling: Option<Polling>,
    /// Last `tick_direct` instant — drives spinner cadence + timeout
    /// checks at ~50ms granularity from the run loop.
    pub polling_last_tick: Instant,
    /// Background-poll spinner shown in the footer for every poll
    /// cycle after the initial one. Provides the "is pilot polling
    /// right now?" feedback users were missing — pre-fix, the footer
    /// went silent after the first cycle and an assignment made on
    /// GitHub gave no signal until the row appeared (or didn't).
    pub bg_poll: Option<BackgroundPoll>,
}

impl StatusCtx {
    pub fn new() -> Self {
        Self {
            notice: None,
            polling: None,
            polling_last_tick: Instant::now(),
            bg_poll: None,
        }
    }

    /// Record a poll-progress event from the daemon. Lights up the
    /// background spinner if the initial modal is gone. Updates
    /// `last_event` so the idle-guard timer resets — a slow poll
    /// that takes 8s of progress still keeps the spinner lit.
    pub fn note_poll_progress(&mut self, source: &str, message: &str) {
        let now = Instant::now();
        match &mut self.bg_poll {
            Some(bg) if bg.source == source => {
                bg.message = message.into();
                bg.spinner_idx = bg.spinner_idx.wrapping_add(1);
                bg.last_event = now;
            }
            _ => {
                self.bg_poll = Some(BackgroundPoll {
                    source: source.into(),
                    message: message.into(),
                    spinner_idx: 0,
                    last_event: now,
                    started_at: now,
                });
            }
        }
    }

    /// Clear the background spinner. Called on `PollCompleted` /
    /// `PollSucceeded` so the footer goes idle as soon as the
    /// cycle finishes (instead of spinning until the guard timeout).
    pub fn note_poll_completed(&mut self, source: &str) {
        if let Some(bg) = &self.bg_poll
            && bg.source == source
        {
            self.bg_poll = None;
        }
    }

    /// Background-poll guard: if the last `note_poll_progress` is
    /// older than `BG_POLL_IDLE_GUARD`, clear the spinner. Defends
    /// against a dropped PollCompleted leaving the indicator
    /// spinning forever.
    pub fn tick_bg_poll(&mut self) -> bool {
        let stale = self
            .bg_poll
            .as_ref()
            .map(|bg| bg.last_event.elapsed() >= BG_POLL_IDLE_GUARD)
            .unwrap_or(false);
        if stale {
            self.bg_poll = None;
            true
        } else {
            false
        }
    }

    /// Clear an in-flight "Spawning…" notice when the matching spawn
    /// event lands. Other notice messages aren't disturbed.
    pub fn clear_spawning_notice(&mut self) {
        if let Some(n) = &self.notice
            && n.message.starts_with("Spawning")
        {
            self.notice = None;
        }
    }

    /// Auto-fade transient notices. Returns `true` if the notice was
    /// cleared so the caller can redraw. Severity decides the timeout:
    /// - Retryable: 5s. Hiccups self-heal, no need to linger.
    /// - Info: 15s. Long enough for a slow spawn; short enough that
    ///   a stuck notice doesn't follow the user around forever.
    /// - Permanent / Auth: stay until dismissed.
    pub fn tick_notice(&mut self) -> bool {
        let Some(n) = &self.notice else { return false };
        let timeout = match n.severity {
            NoticeSeverity::Retryable => Some(RETRYABLE_FADE),
            NoticeSeverity::Info => Some(INFO_FADE),
            NoticeSeverity::Hint => Some(HINT_FADE),
            NoticeSeverity::Auth | NoticeSeverity::Permanent => None,
        };
        if let Some(t) = timeout
            && n.set_at.elapsed() >= t
        {
            self.notice = None;
            return true;
        }
        false
    }

    /// Drive the polling spinner + termination check from the run
    /// loop. Cheap; called every iteration. Returns Some(msg) when
    /// the polling modal wants to be torn down. Caller redraws when
    /// the inner state actually changed.
    pub fn polling_tick(&mut self) -> Option<Msg> {
        if self.polling_last_tick.elapsed() < POLLING_TICK_INTERVAL {
            return None;
        }
        self.polling_last_tick = Instant::now();
        // Background-spinner cadence too — PollProgress events are
        // bursty (zero between steps for several hundred ms), so a
        // pure event-driven advance freezes the glyph between
        // updates. Advancing here gives the smooth ~12 fps motion
        // the initial-poll spinner already has.
        if let Some(bg) = self.bg_poll.as_mut() {
            bg.spinner_idx = bg.spinner_idx.wrapping_add(1);
        }
        // Also gc a stale bg_poll (dropped PollCompleted).
        let _ = self.tick_bg_poll();
        let polling = self.polling.as_mut()?;
        polling.tick_direct()
    }

    /// Tear down the polling modal. Returns true if there was one to
    /// dismiss (so the caller can redraw).
    pub fn dismiss_polling(&mut self) -> bool {
        self.polling.take().is_some()
    }

    /// Spin up the first-poll progress modal. `sources` is the list
    /// of provider IDs the daemon is about to poll.
    pub fn show_polling(&mut self, sources: Vec<String>) {
        self.polling = Some(Polling::new(sources));
        self.polling_last_tick = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(severity: NoticeSeverity, age: Duration) -> Notice {
        Notice {
            message: "x".into(),
            severity,
            set_at: Instant::now() - age,
        }
    }

    #[test]
    fn empty_status_is_noop_on_tick() {
        let mut s = StatusCtx::new();
        assert!(!s.tick_notice());
        assert!(s.polling_tick().is_none());
        assert!(!s.dismiss_polling());
    }

    #[test]
    fn tick_clears_a_faded_retryable_notice() {
        let mut s = StatusCtx::new();
        s.notice = Some(notice(NoticeSeverity::Retryable, Duration::from_secs(10)));
        assert!(s.tick_notice());
        assert!(s.notice.is_none());
    }

    #[test]
    fn tick_leaves_a_fresh_retryable_alone() {
        let mut s = StatusCtx::new();
        s.notice = Some(notice(NoticeSeverity::Retryable, Duration::from_secs(1)));
        assert!(!s.tick_notice());
        assert!(s.notice.is_some());
    }

    #[test]
    fn tick_never_fades_permanent_or_auth() {
        for sev in [NoticeSeverity::Auth, NoticeSeverity::Permanent] {
            let mut s = StatusCtx::new();
            // Even ancient — should not fade.
            s.notice = Some(notice(sev, Duration::from_secs(60 * 60)));
            assert!(!s.tick_notice(), "{sev:?} should not auto-fade");
            assert!(s.notice.is_some());
        }
    }

    #[test]
    fn info_uses_longer_fade_than_retryable() {
        // 7s old: retryable (5s) fades, info (15s) does not.
        let mut retry = StatusCtx::new();
        retry.notice = Some(notice(NoticeSeverity::Retryable, Duration::from_secs(7)));
        assert!(retry.tick_notice());

        let mut info = StatusCtx::new();
        info.notice = Some(notice(NoticeSeverity::Info, Duration::from_secs(7)));
        assert!(!info.tick_notice());
    }

    #[test]
    fn clear_spawning_notice_only_clears_spawn_messages() {
        let mut s = StatusCtx::new();
        s.notice = Some(Notice::new("Saved", NoticeSeverity::Info));
        s.clear_spawning_notice();
        assert!(s.notice.is_some(), "non-spawn notices must be preserved");

        s.notice = Some(Notice::new("Spawning shell…", NoticeSeverity::Info));
        s.clear_spawning_notice();
        assert!(s.notice.is_none());
    }

    #[test]
    fn show_and_dismiss_polling_round_trip() {
        let mut s = StatusCtx::new();
        s.show_polling(vec!["github".into()]);
        assert!(s.polling.is_some());
        assert!(s.dismiss_polling());
        assert!(s.polling.is_none());
        // Second dismiss is a no-op.
        assert!(!s.dismiss_polling());
    }

    #[test]
    fn bg_poll_lights_up_on_progress_and_clears_on_completion() {
        let mut s = StatusCtx::new();
        assert!(s.bg_poll.is_none());

        s.note_poll_progress("github", "PR query: …");
        let bg = s.bg_poll.as_ref().expect("lit up");
        assert_eq!(bg.source, "github");
        assert_eq!(bg.message, "PR query: …");

        s.note_poll_progress("github", "Got 5 PRs, fetching reviews");
        // Same source updates in place, doesn't reset.
        let bg = s.bg_poll.as_ref().unwrap();
        assert_eq!(bg.message, "Got 5 PRs, fetching reviews");

        s.note_poll_completed("github");
        assert!(s.bg_poll.is_none(), "PollCompleted clears the indicator");
    }

    #[test]
    fn bg_poll_ignores_completion_from_other_sources() {
        // Two sources polling concurrently — one finishing must not
        // wipe the other's indicator.
        let mut s = StatusCtx::new();
        s.note_poll_progress("linear", "Fetching issues…");
        s.note_poll_completed("github");
        assert!(
            s.bg_poll.as_ref().map(|b| b.source.as_str()) == Some("linear"),
            "linear's spinner survived github's PollCompleted",
        );
    }

    #[test]
    fn bg_poll_guard_clears_after_long_silence() {
        let mut s = StatusCtx::new();
        s.note_poll_progress("github", "Querying…");
        // Backdate the last_event past the guard.
        if let Some(bg) = s.bg_poll.as_mut() {
            bg.last_event = Instant::now() - Duration::from_secs(30);
        }
        assert!(s.tick_bg_poll(), "stale bg_poll cleared by guard");
        assert!(s.bg_poll.is_none());
    }
}
