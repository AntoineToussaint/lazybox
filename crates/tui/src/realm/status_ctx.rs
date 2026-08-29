//! `StatusCtx` — transient UI status: the footer notice and the
//! first-poll spinner. Extracted from `Model` so the three related
//! fields (and the tick / fade helpers that manage them) live in
//! one place instead of being scattered across the orchestrator.

use crate::realm::components::footer::{Notice, NoticeKey, NoticeSeverity};
use crate::realm::components::polling::Polling;
use crate::realm::model::Msg;
use chrono::{DateTime, Utc};
use lazybox_core::SessionKey;
use lazybox_ipc::TerminalKind;
use lazybox_tui_core::remote::RemoteConnState;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Cap on retained sync-attempt records. Old entries roll off the
/// front; the sync-status window only ever surfaces recent activity.
const SYNC_LOG_CAP: usize = 200;

/// Outcome of one completed sync attempt for a provider source.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SyncOutcome {
    /// Poll finished; `count` tasks matched the source's filter.
    Ok { count: usize },
    /// GitHub deliberately paused until the observed API budget resets.
    RateLimited {
        remaining: u32,
        limit: u32,
        reset_at: DateTime<Utc>,
    },
    /// Poll failed. `kind` is the `ProviderErrorKind` string
    /// ("retryable" / "auth" / "permanent" / ""), `message` the
    /// one-line summary, `detail` the full diagnostic (may be empty).
    Err {
        kind: String,
        message: String,
        detail: String,
    },
}

/// One recorded sync attempt: which source, when, and how it ended.
#[derive(Clone, Debug)]
pub(crate) struct SyncEntry {
    pub source: String,
    pub at: DateTime<Utc>,
    pub outcome: SyncOutcome,
}

impl SyncEntry {
    pub fn is_ok(&self) -> bool {
        matches!(self.outcome, SyncOutcome::Ok { .. })
    }

    pub fn is_rate_limited(&self) -> bool {
        matches!(self.outcome, SyncOutcome::RateLimited { .. })
    }
}

/// Rolling history of sync attempts per provider source. Fed from the
/// daemon's `PollCompleted` / `ProviderError` events; read by the
/// sync-status window. Session-scoped — not persisted, since the
/// daemon itself keeps no sync history to replay on reconnect.
#[derive(Default)]
pub(crate) struct SyncLog {
    entries: VecDeque<SyncEntry>,
}

impl SyncLog {
    /// Record a terminal sync outcome stamped at `at`.
    pub fn record(&mut self, source: &str, at: DateTime<Utc>, outcome: SyncOutcome) {
        self.entries.push_back(SyncEntry {
            source: source.to_string(),
            at,
            outcome,
        });
        while self.entries.len() > SYNC_LOG_CAP {
            self.entries.pop_front();
        }
    }

    pub fn note_completed(&mut self, source: &str, count: usize) {
        self.record(source, Utc::now(), SyncOutcome::Ok { count });
    }

    pub fn note_rate_limited(&mut self, remaining: u32, limit: u32, reset_at: DateTime<Utc>) {
        self.record(
            "github",
            Utc::now(),
            SyncOutcome::RateLimited {
                remaining,
                limit,
                reset_at,
            },
        );
    }

    pub fn note_error(&mut self, source: &str, kind: &str, message: &str, detail: &str) {
        self.record(
            source,
            Utc::now(),
            SyncOutcome::Err {
                kind: kind.to_string(),
                message: message.to_string(),
                detail: detail.to_string(),
            },
        );
    }

    /// Recent attempts, most-recent-first.
    pub fn recent(&self) -> impl Iterator<Item = &SyncEntry> {
        self.entries.iter().rev()
    }

    /// Latest attempt per source, sorted by source name. Drives the
    /// "last sync per provider" summary block — most recent insert
    /// wins because `entries` is in chronological order.
    pub fn latest_per_source(&self) -> Vec<SyncEntry> {
        let mut latest: std::collections::BTreeMap<String, SyncEntry> =
            std::collections::BTreeMap::new();
        for e in &self.entries {
            latest.insert(e.source.clone(), e.clone());
        }
        latest.into_values().collect()
    }
}

/// Cap on retained messages-log records. The log is a bounded ring —
/// the last N notices — so a long session can't grow it without bound.
const MESSAGE_LOG_CAP: usize = 100;

/// One footer notice preserved in the messages log: its text, its
/// severity (for coloring), and when it was raised.
#[derive(Clone, Debug)]
pub(crate) struct MessageEntry {
    pub message: String,
    pub severity: NoticeSeverity,
    pub at: DateTime<Utc>,
    /// Consecutive identical re-fires collapsed into this entry. A
    /// poll sweep re-emitting one error every 15s reads as `✗ … ×12 ·
    /// 3m ago`, not as 12 rows drowning the rest of the log (#1245).
    pub count: u32,
}

/// Durable, bounded history of footer notices. The footer is the
/// transient surface — a notice flashes and fades; this is the durable
/// one, where every non-hint notice accumulates so a missed or
/// already-faded error is still readable via the messages window
/// (`Shift-M`, #309). Session-scoped, not persisted.
#[derive(Default)]
pub(crate) struct MessageLog {
    entries: VecDeque<MessageEntry>,
}

impl MessageLog {
    /// Append a notice, stamped now. A repeat of the newest entry
    /// (same text + severity) collapses into it — count bumped,
    /// timestamp refreshed — instead of stacking duplicate rows.
    /// Oldest entries roll off the front once the ring is full.
    pub fn record(&mut self, message: &str, severity: NoticeSeverity) {
        if let Some(last) = self.entries.back_mut()
            && last.message == message
            && last.severity == severity
        {
            last.count = last.count.saturating_add(1);
            last.at = Utc::now();
            return;
        }
        self.entries.push_back(MessageEntry {
            message: message.to_string(),
            severity,
            at: Utc::now(),
            count: 1,
        });
        while self.entries.len() > MESSAGE_LOG_CAP {
            self.entries.pop_front();
        }
    }

    /// Recorded notices, most-recent-first.
    pub fn recent(&self) -> impl Iterator<Item = &MessageEntry> {
        self.entries.iter().rev()
    }

    /// Drop the whole history (the `c` key in the messages window).
    /// Returns whether anything was cleared.
    pub fn clear(&mut self) -> bool {
        let had = !self.entries.is_empty();
        self.entries.clear();
        had
    }
}

/// How long retryable notices stay visible before fading. Auth notices
/// and the two persistent system banners ignore this — they stay until
/// dismissed or resolved.
const RETRYABLE_FADE: Duration = Duration::from_secs(5);
/// How long info notices ("Spawning shell…", "Setup saved", etc.)
/// stay visible if their triggering event never lands. Longer than
/// retryable so a slow worktree creation doesn't fade mid-flight.
const INFO_FADE: Duration = Duration::from_secs(15);
/// One-shot hints (e.g. "scroll: this view manages its own scrollback")
/// fade quickly so they don't follow the user around the UI.
const HINT_FADE: Duration = Duration::from_secs(3);
/// How long an action-error toast (a merge/close/update/delete failure,
/// i.e. a workspace-tagged Permanent notice — see
/// [`Notice::is_action_toast`]) stays before fading. Long — the failure
/// is worth inspecting and the full text lives in the messages log
/// (`Shift-M`) regardless — but finite, so a toast for an
/// already-resolved condition doesn't own the footer until an explicit
/// `Esc` (#588). `Esc` still dismisses immediately. EVERY Permanent error
/// now uses this fade (tagged action toasts AND plain untagged failures)
/// — the only exceptions are the two event-resolved system banners
/// (daemon disconnected / build mismatch), which stay put (see
/// [`Notice::is_persistent_system_banner`]).
const PERMANENT_FADE: Duration = Duration::from_secs(45);
/// How long an acknowledged error message (Esc'd or fully faded)
/// suppresses identical re-fires from the footer. Long enough to stop
/// a 15s poll sweep from re-shouting a known condition; short enough
/// that a genuinely persistent problem resurfaces within the session.
/// The messages log records every re-fire either way (#1245).
const ACK_SUPPRESS: Duration = Duration::from_secs(10 * 60);
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
/// lazybox hasn't given up — useful on slow networks where a
/// 60+ second poll is normal.
const STILL_WORKING_HINT_AFTER: Duration = Duration::from_secs(30);

/// Last-resort backstop for the spawn spinner. The spinner is primarily
/// a *projection* of live terminal state — it clears the moment a
/// terminal for the spawn's target appears (see
/// `Model::recompute_spawn_spinner`), independent of any single event
/// arriving — and a spawn `ProviderError` clears it on failure. This
/// guard only covers the pathological case where neither ever happens
/// (the spawn vanished without a terminal and without an error). It is
/// deliberately generous — a cold bare-clone + worktree checkout for a
/// large repo can run well past a minute, and the whole point of the
/// spinner is to reassure during exactly that wait, so the guard must
/// outlast a realistic worst-case provision.
const SPAWN_SPINNER_GUARD: Duration = Duration::from_secs(120);

/// How long a spawn must have been in flight before `Esc` treats it as
/// "stuck" and cancels it (#1372). Short enough to be a real escape from
/// a wedged spawn, long enough that an `Esc` during a normal fast spawn
/// (dismissing a notice, clearing a search) doesn't abandon it.
const STUCK_SPAWN_ESC_AFTER: Duration = Duration::from_secs(5);

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

pub(crate) struct GithubRateLimitWait {
    pub remaining: u32,
    pub limit: u32,
    pub reset_at: DateTime<Utc>,
    last_tick: Instant,
}

impl GithubRateLimitWait {
    pub fn label(&self) -> String {
        rate_limit_wait_label(self.remaining, self.limit, self.reset_at, Utc::now())
    }
}

pub(crate) fn rate_limit_wait_label(
    remaining: u32,
    limit: u32,
    reset_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> String {
    format!(
        "GitHub rate-limited · {}",
        rate_limit_wait_detail(remaining, limit, reset_at, now)
    )
}

pub(crate) fn rate_limit_wait_detail(
    remaining: u32,
    limit: u32,
    reset_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> String {
    let budget = if limit == 0 {
        format!("{remaining} left")
    } else {
        format!("{remaining}/{limit} left")
    };
    if reset_at <= now {
        return format!("reset {} · {budget}", reset_at.format("%H:%M UTC"));
    }

    let millis = reset_at
        .signed_duration_since(now)
        .num_milliseconds()
        .max(1) as u64;
    let seconds = millis.div_ceil(1000);
    let wait = if seconds < 60 {
        format!("~{seconds}s")
    } else if seconds < 60 * 60 {
        format!("~{}m", seconds.div_ceil(60))
    } else {
        format!("~{}h", seconds.div_ceil(60 * 60))
    };
    format!("{wait} · {} · {budget}", reset_at.format("%H:%M UTC"))
}

/// Animated "spawning…" indicator shown in the footer between a
/// `Spawn` command leaving the TUI and the matching `TerminalSpawned`
/// arriving. Worktree provisioning (bare clone → per-task worktree)
/// happens on the daemon BEFORE it broadcasts `TerminalSpawned`, and
/// on a cold clone that gap is several seconds with zero feedback —
/// the dogfood complaint was "press w, nothing happens, did it even
/// register?". This replaces the old static `Spawning claude…` Info
/// notice (which neither animated nor outlived its 15s fade) with a
/// glyph that visibly turns until the terminal actually appears.
pub(crate) struct Spawning {
    /// Agent / shell label, e.g. `"claude"`, `"shell"`. Rendered as
    /// `spawning claude · 3s`.
    pub label: String,
    /// Workspace the spawn targets. Together with `kind` and
    /// `baseline_count`, lets the spinner be a projection of the live
    /// terminal set: it clears the instant a matching terminal exists,
    /// regardless of whether the `TerminalSpawned` event was seen (see
    /// `Model::recompute_spawn_spinner`).
    pub session_key: SessionKey,
    /// What was spawned. Singleton kinds (agents) clear when a runner of
    /// the same identity exists in the session; non-singleton kinds
    /// (shells) clear when the session's terminal count rises above
    /// `baseline_count`.
    pub kind: TerminalKind,
    /// Terminals already in the target session when the spawn was sent.
    /// A non-singleton spawn is satisfied once the count exceeds this.
    pub baseline_count: usize,
    pub spinner_idx: usize,
    pub started_at: Instant,
}

impl Spawning {
    pub fn spinner_glyph(&self) -> &'static str {
        BG_SPINNER_FRAMES[self.spinner_idx % BG_SPINNER_FRAMES.len()]
    }
    /// Footer label — `spawning claude` for the first second (no
    /// suffix so a fast spawn doesn't flicker `0s`), then
    /// `spawning claude · Ns` so a slow worktree provision reads as
    /// "still working" rather than stuck.
    pub fn label(&self) -> String {
        let secs = self.started_at.elapsed().as_secs();
        if secs < 1 {
            format!("spawning {}", self.label)
        } else {
            format!("spawning {} · {secs}s", self.label)
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
    /// cycle after the initial one. Provides the "is lazybox polling
    /// right now?" feedback users were missing — pre-fix, the footer
    /// went silent after the first cycle and an assignment made on
    /// GitHub gave no signal until the row appeared (or didn't).
    pub bg_poll: Option<BackgroundPoll>,
    /// GitHub is not syncing or failed: it is deliberately sleeping
    /// until the observed API window resets.
    pub github_rate_limit_wait: Option<GithubRateLimitWait>,
    /// Latest compact budget-governor snapshot received through the
    /// GitHub poll progress stream. Shift-D renders it above history.
    pub github_governor: Option<String>,
    /// Animated spawn indicator — lit when a `Spawn` command is sent,
    /// cleared when the matching `TerminalSpawned` /
    /// `TerminalFocusRequested` lands (or a spawn `ProviderError`, or
    /// the `SPAWN_SPINNER_GUARD` backstop).
    pub spawning: Option<Spawning>,
    /// Rolling per-source history of sync outcomes. Surfaced by the
    /// sync-status window so a silently-failing poll (rate limit,
    /// auth, network) is visible instead of leaving the inbox
    /// quietly stale.
    pub sync: SyncLog,
    /// Durable, bounded history of footer notices. The footer flashes
    /// the latest one transiently; this keeps the last N so a notice
    /// that faded (or that the user missed) is still readable in the
    /// messages window (#309).
    pub messages: MessageLog,
    /// Action-error toasts (merge/close/update/delete failures) the user
    /// acknowledged — via Esc or by letting the 45s fade elapse — keyed
    /// by workspace → the exact message. While an entry stands, an
    /// identical re-fire for that workspace stays quiet instead of
    /// re-shouting on the next poll/attempt — an acknowledged failure
    /// means "I've seen this," not "show it again in 5s" (#832). Cleared
    /// when the reason changes or a superseding success resolves the
    /// condition ([`Self::forget_dismissed_action_error`]).
    ///
    /// Matching is on the exact message, so it relies on the notice text
    /// being a deterministic function of the failure
    /// (`action_failure_notice`). That holds today; if that formatting
    /// ever gains a variable part (timestamp, attempt counter) the match
    /// stops firing — which fails *open* (the banner re-shows), never
    /// silently muting a live error.
    dismissed_action_errors: HashMap<String, String>,
    /// Error notices (Retryable / Auth / Permanent) the user has
    /// acknowledged — Esc'd, or let run their full on-screen fade —
    /// keyed by exact message → when. While an entry is fresh
    /// ([`ACK_SUPPRESS`]), an identical re-fire stays out of the
    /// footer (it still lands in the messages log), so a condition
    /// that re-emits on every poll sweep can't re-shout an already
    /// acknowledged error forever (#1245). Info/Hint never suppress —
    /// "Spawning shell…" must show on every spawn. Matching is on the
    /// exact text, so any variation fails *open* (the banner shows),
    /// never silently muting a new error.
    acknowledged_errors: HashMap<String, Instant>,
    /// Durable state of the connection to the remote box (#1066). Unlike
    /// the transient `notice`, this persists so the user can consult "am I
    /// connected to my box right now?" at any time. `NotConfigured` (the
    /// default) hides the indicator entirely — the local-only path is
    /// unchanged. Fed by the r-spawn worker's `State` notices.
    pub remote_conn: RemoteConnState,
    /// Animation frame for the box-connection indicator while a bring-up
    /// is in flight (creating/waking/connecting). Advanced on the polling
    /// heartbeat so the glyph turns like the other spinners.
    remote_spinner_idx: usize,
}

impl StatusCtx {
    pub fn new() -> Self {
        Self {
            notice: None,
            polling: None,
            polling_last_tick: Instant::now(),
            bg_poll: None,
            github_rate_limit_wait: None,
            github_governor: None,
            spawning: None,
            sync: SyncLog::default(),
            messages: MessageLog::default(),
            dismissed_action_errors: HashMap::new(),
            acknowledged_errors: HashMap::new(),
            remote_conn: RemoteConnState::NotConfigured,
            remote_spinner_idx: 0,
        }
    }

    /// Record a durable box-connection-state transition (#1066).
    pub fn note_remote_state(&mut self, state: RemoteConnState) {
        self.remote_conn = state;
    }

    /// The persistent box indicator's `(glyph, label)` while a bring-up is
    /// in flight — an animated spinner frame plus `creating…`/`waking…`/
    /// `connecting…`. `None` when the box is idle or unconfigured, so the
    /// caller can rank it above the ambient background-poll spinner.
    pub fn remote_status_busy(&self) -> Option<(&'static str, String)> {
        self.remote_conn.is_busy().then(|| {
            (
                BG_SPINNER_FRAMES[self.remote_spinner_idx % BG_SPINNER_FRAMES.len()],
                self.remote_conn.label(),
            )
        })
    }

    /// The persistent box indicator's `(glyph, label)` in a steady state —
    /// connected / disconnected / asleep / error. `None` when a bring-up is
    /// in flight (that's `remote_status_busy`) or no box is configured.
    /// This is the footer's lowest-priority fallback, so the box state is
    /// visible whenever nothing more urgent occupies the status slot.
    pub fn remote_status_steady(&self) -> Option<(&'static str, String)> {
        (self.remote_conn.is_configured() && !self.remote_conn.is_busy())
            .then(|| (self.remote_conn.glyph(), self.remote_conn.label()))
    }

    /// Remember the currently-displayed action-error toast as dismissed,
    /// so an identical re-fire for its workspace stays suppressed (#832).
    /// No-op unless the live notice is a workspace-tagged action toast —
    /// dismissing a plain system banner (daemon disconnected, sync
    /// failed) records nothing, since those have their own recovery.
    pub fn remember_dismissed_action_error(&mut self) {
        if let Some(n) = self.notice.as_ref()
            && let Some(ws) = n.workspace().map(str::to_string)
        {
            self.dismissed_action_errors.insert(ws, n.message.clone());
        }
    }

    /// Retract a live notice whose cause `key` names — the general
    /// "this condition resolved" primitive. Replaces the workspace-only
    /// `clear_action_error` special case: ANY keyed notice (a stale
    /// daemon build, a dropped connection, a workspace action) is cleared
    /// through here when its cause ends, instead of sitting in the footer
    /// until dismissed. For a [`NoticeKey::WorkspaceAction`] it also
    /// forgets the dismissal/ack so a genuinely new failure surfaces
    /// afresh (#832/#1245) — this runs even when no notice is currently
    /// visible. Returns whether a visible notice was cleared.
    pub fn resolve_notice(&mut self, key: &NoticeKey) -> bool {
        if let NoticeKey::WorkspaceAction(ws) = key {
            self.forget_dismissed_action_error(ws);
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|n| n.key.as_ref() == Some(key))
        {
            self.notice = None;
            return true;
        }
        false
    }

    /// Whether an action error for `workspace` carrying this exact
    /// `message` was already dismissed and not since superseded (#832).
    pub fn action_error_dismissed(&self, workspace: &str, message: &str) -> bool {
        self.dismissed_action_errors
            .get(workspace)
            .map(String::as_str)
            == Some(message)
    }

    /// Remember the currently-displayed notice as acknowledged when it
    /// carries an error severity (Retryable / Auth / Permanent), so an
    /// identical re-fire stays out of the footer for a while (#1245).
    /// Called from the Esc dismissal and from a completed on-screen
    /// fade — both count as "I've had the chance to see this".
    pub fn remember_acknowledged_error(&mut self) {
        if let Some(n) = self.notice.as_ref()
            && matches!(
                n.severity,
                NoticeSeverity::Retryable | NoticeSeverity::Auth | NoticeSeverity::Permanent
            )
        {
            self.acknowledged_errors
                .insert(n.message.clone(), Instant::now());
        }
    }

    /// Whether this exact error message was acknowledged recently
    /// enough ([`ACK_SUPPRESS`]) that a re-fire should skip the footer.
    /// Never true for Info/Hint. The messages log records the re-fire
    /// regardless, so nothing is lost — only the re-shout.
    pub fn error_acknowledged(&self, message: &str, severity: NoticeSeverity) -> bool {
        matches!(
            severity,
            NoticeSeverity::Retryable | NoticeSeverity::Auth | NoticeSeverity::Permanent
        ) && self
            .acknowledged_errors
            .get(message)
            .is_some_and(|at| at.elapsed() < ACK_SUPPRESS)
    }

    /// Forget a workspace's dismissed action error — its reason changed
    /// or a success resolved it, so the next failure surfaces afresh
    /// (#832). The general acknowledged-message entry goes with it: a
    /// recurrence after the condition cleared is new information, not a
    /// re-fire of a known one (#1245).
    pub fn forget_dismissed_action_error(&mut self, workspace: &str) {
        if let Some(msg) = self.dismissed_action_errors.remove(workspace) {
            self.acknowledged_errors.remove(&msg);
        }
    }

    /// Light up the animated spawn indicator. `label` is the agent /
    /// shell id (`"claude"`, `"shell"`); `session_key` + `kind` +
    /// `baseline_count` capture the target so the spinner can be cleared
    /// by projection once a matching terminal exists. Replaces any prior
    /// spawn indicator — back-to-back spawns just restart the timer,
    /// which is the right behavior (the latest spawn is the one in
    /// flight).
    pub fn note_spawning(
        &mut self,
        label: impl Into<String>,
        session_key: SessionKey,
        kind: TerminalKind,
        baseline_count: usize,
    ) {
        self.spawning = Some(Spawning {
            label: label.into(),
            session_key,
            kind,
            baseline_count,
            spinner_idx: 0,
            started_at: Instant::now(),
        });
    }

    /// Clear the spawn indicator (matching terminal appeared, focus
    /// was redirected to an existing one, or the spawn failed).
    /// Returns true if there was one to clear so the caller can redraw.
    pub fn clear_spawning(&mut self) -> bool {
        self.spawning.take().is_some()
    }

    /// The workspace of a spawn that has been in flight long enough to be
    /// considered stuck, if any — the target `Esc` cancels (#1372). `None`
    /// while nothing is spawning or a fresh spawn is still plausibly
    /// provisioning, so `Esc` keeps its normal meaning then.
    pub fn stuck_spawn_session(&self) -> Option<&SessionKey> {
        self.spawning
            .as_ref()
            .filter(|sp| sp.started_at.elapsed() >= STUCK_SPAWN_ESC_AFTER)
            .map(|sp| &sp.session_key)
    }

    /// Record a poll-progress event from the daemon. Lights up the
    /// background spinner if the initial modal is gone. Updates
    /// `last_event` so the idle-guard timer resets — a slow poll
    /// that takes 8s of progress still keeps the spinner lit.
    pub fn note_poll_progress(&mut self, source: &str, message: &str) {
        let now = Instant::now();
        if source == "github" {
            self.github_rate_limit_wait = None;
            if let Some(snapshot) = message.strip_prefix("Governor: ") {
                self.github_governor = Some(snapshot.to_string());
            }
        }
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
        if source == "github" {
            self.github_rate_limit_wait = None;
        }
    }

    pub fn note_github_rate_limit_wait(
        &mut self,
        remaining: u32,
        limit: u32,
        reset_at: DateTime<Utc>,
    ) {
        if self
            .bg_poll
            .as_ref()
            .is_some_and(|poll| poll.source == "github")
        {
            self.bg_poll = None;
        }
        self.github_rate_limit_wait = Some(GithubRateLimitWait {
            remaining,
            limit,
            reset_at,
            last_tick: Instant::now(),
        });
    }

    pub fn note_poll_failed(&mut self, source: &str) -> bool {
        let background_active = self
            .bg_poll
            .as_ref()
            .is_some_and(|poll| poll.source == source);
        let wait_active = source == "github" && self.github_rate_limit_wait.is_some();
        if background_active {
            self.bg_poll = None;
        }
        if source == "github" {
            self.github_rate_limit_wait = None;
        }
        background_active || wait_active
    }

    pub fn tick_github_rate_limit_wait(&mut self, now: DateTime<Utc>) -> bool {
        let expired = self
            .github_rate_limit_wait
            .as_ref()
            .is_some_and(|wait| wait.reset_at <= now);
        if expired {
            self.github_rate_limit_wait = None;
        }
        expired
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

    /// Spawn-indicator backstop: clear it if `SPAWN_SPINNER_GUARD` has
    /// elapsed since the spawn started without the matching event
    /// landing. Returns true if it was cleared (caller redraws).
    pub fn tick_spawning(&mut self) -> bool {
        let stale = self
            .spawning
            .as_ref()
            .map(|sp| sp.started_at.elapsed() >= SPAWN_SPINNER_GUARD)
            .unwrap_or(false);
        if stale {
            self.spawning = None;
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
    /// - Permanent (spawn/provider/sync failure, action toast): 45s.
    ///   EVERY such error now auto-fades — a one-off or already-resolved
    ///   failure must not own the footer forever (the "errors that never
    ///   disappear" gripe). The full text stays in the messages log
    ///   (`Shift-M`). The ONLY exceptions are the two genuinely-persistent
    ///   system banners — daemon disconnected / build mismatch — which are
    ///   event-resolved (not poll-re-fired), so fading them would drop a
    ///   live problem indicator with nothing to bring it back.
    /// - Auth: still sticky. It's specifically actionable ("fix your
    ///   token"), rare, and self-clears on the next successful auth, so
    ///   it stays until resolved or the user dismisses it with `Esc`.
    pub fn tick_notice(&mut self) -> bool {
        let Some(n) = &self.notice else { return false };
        let is_action = n.is_action_toast();
        let timeout = match n.severity {
            NoticeSeverity::Retryable => Some(RETRYABLE_FADE),
            NoticeSeverity::Info => Some(INFO_FADE),
            NoticeSeverity::Hint => Some(HINT_FADE),
            NoticeSeverity::Permanent if n.is_persistent_system_banner() => None,
            NoticeSeverity::Permanent => Some(PERMANENT_FADE),
            NoticeSeverity::Auth => None,
        };
        // `n`'s borrow ends here (last read), freeing `&mut self` below.
        if matches!(timeout, Some(t) if n.set_at.elapsed() >= t) {
            // An action toast that fades has been on screen its full 45s
            // — treat that as the same acknowledgment as Esc and record
            // it dismissed, so an identical re-fire on the next
            // poll/attempt stays quiet instead of re-shouting (#832).
            // Without this, only the Esc path suppressed and a user who
            // let the banner fade got the never-disappearing footer back
            // on the next genuine attempt.
            if is_action {
                self.remember_dismissed_action_error();
            }
            // Any error severity that ran its full on-screen time was
            // seen; identical re-fires stay quiet for a while (#1245).
            self.remember_acknowledged_error();
            self.notice = None;
            return true;
        }
        false
    }

    /// Drive the polling spinner + termination check from the run
    /// loop. Cheap; called every iteration. Returns `(msg, advanced)`:
    /// `msg` is Some when the polling modal wants to be torn down;
    /// `advanced` is true when a visible indicator actually changed
    /// this tick (a spinner frame advanced, or a guard cleared one).
    /// The caller uses `advanced` as its redraw gate — the old shape
    /// redrew the whole UI on every ~16ms run-loop heartbeat for the
    /// entire duration of a poll/provision just because an indicator
    /// *existed*, even though the glyph only moves at the 80ms
    /// cadence enforced here.
    pub fn polling_tick(&mut self) -> (Option<Msg>, bool) {
        if self.polling_last_tick.elapsed() < POLLING_TICK_INTERVAL {
            return (None, false);
        }
        self.polling_last_tick = Instant::now();
        let mut advanced = false;
        // Background-spinner cadence too — PollProgress events are
        // bursty (zero between steps for several hundred ms), so a
        // pure event-driven advance freezes the glyph between
        // updates. Advancing here gives the smooth ~12 fps motion
        // the initial-poll spinner already has.
        if let Some(bg) = self.bg_poll.as_mut() {
            bg.spinner_idx = bg.spinner_idx.wrapping_add(1);
            advanced = true;
        }
        // Same cadence for the spawn spinner — there are no progress
        // events during worktree provisioning, so without advancing
        // here the glyph would sit frozen for the whole (multi-second)
        // wait, defeating the "something is happening" reassurance.
        if let Some(sp) = self.spawning.as_mut() {
            sp.spinner_idx = sp.spinner_idx.wrapping_add(1);
            advanced = true;
        }
        // Box-connection indicator: animate only while a bring-up is in
        // flight; a steady state (connected/disconnected) uses a static
        // glyph and needs no per-tick redraw.
        if self.remote_conn.is_busy() {
            self.remote_spinner_idx = self.remote_spinner_idx.wrapping_add(1);
            advanced = true;
        }
        if self.tick_github_rate_limit_wait(Utc::now()) {
            advanced = true;
        } else if let Some(wait) = self.github_rate_limit_wait.as_mut()
            && wait.last_tick.elapsed() >= Duration::from_secs(1)
        {
            wait.last_tick = Instant::now();
            advanced = true;
        }
        // Also gc a stale bg_poll (dropped PollCompleted) + a stale
        // spawn indicator (dropped TerminalSpawned). Clearing an
        // indicator is itself a visible change.
        if self.tick_bg_poll() {
            advanced = true;
        }
        if self.tick_spawning() {
            advanced = true;
        }
        let msg = self.polling.as_mut().and_then(|p| p.tick_direct());
        (msg, advanced)
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
            key: None,
            repeats: 1,
        }
    }

    #[test]
    fn message_log_collapses_consecutive_identical_refires() {
        let mut log = MessageLog::default();
        log.record("⚠ sync failed: boom", NoticeSeverity::Retryable);
        log.record("⚠ sync failed: boom", NoticeSeverity::Retryable);
        log.record("⚠ sync failed: boom", NoticeSeverity::Retryable);
        log.record("· something else", NoticeSeverity::Info);
        log.record("⚠ sync failed: boom", NoticeSeverity::Retryable);

        let entries: Vec<_> = log.recent().collect();
        assert_eq!(entries.len(), 3, "3 rows, not 5");
        assert_eq!(entries[2].count, 3, "the first burst collapsed to ×3");
        assert_eq!(
            entries[0].count, 1,
            "a re-fire after an interleaved message starts a new row",
        );
    }

    #[test]
    fn empty_status_is_noop_on_tick() {
        let mut s = StatusCtx::new();
        assert!(!s.tick_notice());
        let (msg, advanced) = s.polling_tick();
        assert!(msg.is_none());
        assert!(!advanced, "nothing to animate — no redraw request");
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
    fn tick_never_fades_auth() {
        let mut s = StatusCtx::new();
        // Even ancient — an actionable auth error stays until dismissed.
        s.notice = Some(notice(NoticeSeverity::Auth, Duration::from_secs(60 * 60)));
        assert!(!s.tick_notice(), "Auth should not auto-fade");
        assert!(s.notice.is_some());
    }

    /// A workspace-tagged action-error toast (merge/close failed) fades
    /// on a long timeout — bounded, not infinite (#588). Full text still
    /// lives in the messages log.
    #[test]
    fn action_toast_fades_on_a_long_timeout() {
        let action_toast = |age| Notice {
            key: Some(NoticeKey::WorkspaceAction("github:o/r#1".into())),
            ..notice(NoticeSeverity::Permanent, age)
        };

        let mut fresh = StatusCtx::new();
        fresh.notice = Some(action_toast(Duration::from_secs(10)));
        assert!(!fresh.tick_notice(), "a fresh error must stay up");
        assert!(fresh.notice.is_some());

        let mut stale = StatusCtx::new();
        stale.notice = Some(action_toast(Duration::from_secs(120)));
        assert!(stale.tick_notice(), "a long-stale action toast must fade");
        assert!(stale.notice.is_none());
    }

    /// Every hard failure now auto-fades — including an untagged
    /// Permanent banner (revises #588, which kept these sticky forever).
    /// A genuinely-persistent condition re-fires on its next poll after
    /// `ACK_SUPPRESS`, so it pulses back rather than owning the footer.
    #[test]
    fn untagged_permanent_banner_fades_after_its_timeout() {
        // Fresh (10s old) → still up.
        let mut fresh = StatusCtx::new();
        fresh.notice = Some(notice(NoticeSeverity::Permanent, Duration::from_secs(10)));
        assert!(!fresh.tick_notice(), "a fresh error banner stays up");
        assert!(fresh.notice.is_some());

        // Past PERMANENT_FADE (45s) → gone, even without a workspace tag.
        let mut stale = StatusCtx::new();
        stale.notice = Some(notice(NoticeSeverity::Permanent, Duration::from_secs(60)));
        assert!(
            stale.tick_notice(),
            "an untagged Permanent banner auto-fades once its timeout elapses"
        );
        assert!(stale.notice.is_none());
    }

    /// Auth stays sticky: it's actionable and self-clears on success.
    #[test]
    fn auth_banner_stays_sticky() {
        let mut s = StatusCtx::new();
        s.notice = Some(notice(NoticeSeverity::Auth, Duration::from_secs(60 * 60)));
        assert!(!s.tick_notice(), "an Auth banner must not auto-fade");
        assert!(s.notice.is_some());
    }

    /// The general resolve primitive retracts a notice by its typed
    /// cause and leaves a notice with a different (or no) key alone —
    /// what lets a resolved system condition (matching daemon build,
    /// reconnect) clear its banner instead of it sitting until dismissed.
    #[test]
    fn resolve_notice_clears_only_the_matching_key() {
        let mut s = StatusCtx::new();

        // A different key is untouched.
        let mut n = notice(NoticeSeverity::Permanent, Duration::from_secs(0));
        n.key = Some(NoticeKey::DaemonBuildMismatch);
        s.notice = Some(n);
        assert!(!s.resolve_notice(&NoticeKey::DaemonConnection));
        assert!(s.notice.is_some(), "an unrelated cause must not clear it");

        // The matching key clears it.
        assert!(s.resolve_notice(&NoticeKey::DaemonBuildMismatch));
        assert!(s.notice.is_none());

        // Nothing to clear reports false.
        assert!(!s.resolve_notice(&NoticeKey::DaemonBuildMismatch));
    }

    /// Resolving a workspace-action notice also forgets its dismissal, so
    /// a genuinely new failure surfaces afresh after the user dismissed
    /// the earlier one (#832) — and it runs even when no banner is up.
    #[test]
    fn resolve_notice_forgets_workspace_action_dismissal() {
        let mut s = StatusCtx::new();
        let ws = "github:o/r#1";
        let mut n = notice(NoticeSeverity::Permanent, Duration::from_secs(0));
        n.message = "merge failed".into();
        n.key = Some(NoticeKey::WorkspaceAction(ws.into()));
        s.notice = Some(n);
        s.remember_dismissed_action_error();
        assert!(s.action_error_dismissed(ws, "merge failed"));

        // The condition resolved: forget the dismissal even though the
        // banner already faded (notice cleared).
        s.notice = None;
        assert!(!s.resolve_notice(&NoticeKey::WorkspaceAction(ws.into())));
        assert!(
            !s.action_error_dismissed(ws, "merge failed"),
            "a resolved action error re-arms so the next failure shows",
        );
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
    fn governor_progress_is_retained_for_sync_status() {
        let mut s = StatusCtx::new();
        s.note_poll_progress(
            "github",
            "Governor: share=55% · graphql 4300/5000 reserve=2250 allowance=4/9",
        );
        assert_eq!(
            s.github_governor.as_deref(),
            Some("share=55% · graphql 4300/5000 reserve=2250 allowance=4/9")
        );
        s.note_poll_completed("github");
        assert!(
            s.github_governor.is_some(),
            "completion clears the spinner but retains the last budget snapshot"
        );
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
    fn rate_limit_wait_replaces_syncing_and_clears_on_resume() {
        let mut s = StatusCtx::new();
        let reset_at = Utc::now() + chrono::Duration::minutes(7);
        s.note_poll_progress("github", "Fetching issues");
        assert!(s.bg_poll.is_some());

        s.note_github_rate_limit_wait(98, 5000, reset_at);
        assert!(s.bg_poll.is_none());
        assert!(s.github_rate_limit_wait.is_some());

        s.note_poll_progress("github", "Fetching issues");
        assert!(s.github_rate_limit_wait.is_none());
        assert!(s.bg_poll.is_some());
    }

    #[test]
    fn rate_limit_label_surfaces_countdown_reset_and_budget() {
        use chrono::TimeZone;

        let now = Utc.with_ymd_and_hms(2026, 7, 30, 7, 16, 28).unwrap();
        let reset_at = Utc.with_ymd_and_hms(2026, 7, 30, 7, 23, 22).unwrap();

        assert_eq!(
            rate_limit_wait_label(98, 5000, reset_at, now),
            "GitHub rate-limited · ~7m · 07:23 UTC · 98/5000 left"
        );
    }

    #[test]
    fn failed_poll_clears_in_flight_and_waiting_states() {
        let mut s = StatusCtx::new();
        s.note_poll_progress("github", "Fetching issues");
        assert!(s.note_poll_failed("github"));
        assert!(s.bg_poll.is_none());

        s.note_github_rate_limit_wait(98, 5000, Utc::now() + chrono::Duration::minutes(7));
        assert!(s.note_poll_failed("github"));
        assert!(s.github_rate_limit_wait.is_none());
    }

    #[test]
    fn rate_limit_wait_expires_without_a_follow_up_daemon_event() {
        let mut s = StatusCtx::new();
        let reset_at = Utc::now() + chrono::Duration::minutes(7);
        s.note_github_rate_limit_wait(98, 5000, reset_at);

        assert!(!s.tick_github_rate_limit_wait(reset_at - chrono::Duration::seconds(1)));
        assert!(s.github_rate_limit_wait.is_some());
        assert!(s.tick_github_rate_limit_wait(reset_at));
        assert!(s.github_rate_limit_wait.is_none());
    }

    #[test]
    fn note_spawning_lights_up_and_clear_tears_down() {
        let mut s = StatusCtx::new();
        assert!(s.spawning.is_none());

        s.note_spawning(
            "claude",
            "test:ws".into(),
            TerminalKind::Agent("claude".into()),
            0,
        );
        let sp = s.spawning.as_ref().expect("spawn indicator lit");
        assert_eq!(sp.label, "claude");
        // Fresh spawn: sub-second label has no elapsed suffix.
        assert_eq!(sp.label(), "spawning claude");

        assert!(s.clear_spawning(), "clear reports it removed one");
        assert!(s.spawning.is_none());
        // Second clear is a no-op.
        assert!(!s.clear_spawning());
    }

    #[test]
    fn note_spawning_restarts_on_back_to_back_spawns() {
        // A second spawn before the first lands replaces the indicator
        // (latest spawn is the one actually in flight) and resets the
        // animation frame.
        let mut s = StatusCtx::new();
        s.note_spawning("shell", "test:ws".into(), TerminalKind::Shell, 0);
        if let Some(sp) = s.spawning.as_mut() {
            sp.spinner_idx = 7;
        }
        s.note_spawning(
            "claude",
            "test:ws".into(),
            TerminalKind::Agent("claude".into()),
            0,
        );
        let sp = s.spawning.as_ref().unwrap();
        assert_eq!(sp.label, "claude");
        assert_eq!(sp.spinner_idx, 0, "frame counter restarts on re-spawn");
    }

    #[test]
    fn spawn_label_gains_elapsed_suffix_after_a_second() {
        let mut s = StatusCtx::new();
        s.note_spawning(
            "claude",
            "test:ws".into(),
            TerminalKind::Agent("claude".into()),
            0,
        );
        // Backdate so the elapsed branch fires deterministically.
        if let Some(sp) = s.spawning.as_mut() {
            sp.started_at = Instant::now() - Duration::from_secs(3);
        }
        assert_eq!(s.spawning.as_ref().unwrap().label(), "spawning claude · 3s");
    }

    #[test]
    fn spawn_guard_clears_after_a_dropped_terminal_spawned() {
        let mut s = StatusCtx::new();
        s.note_spawning(
            "claude",
            "test:ws".into(),
            TerminalKind::Agent("claude".into()),
            0,
        );
        // Backdate well past SPAWN_SPINNER_GUARD so the assertion
        // isn't sensitive to the exact constant.
        if let Some(sp) = s.spawning.as_mut() {
            sp.started_at = Instant::now() - Duration::from_secs(600);
        }
        assert!(s.tick_spawning(), "stale spawn indicator cleared by guard");
        assert!(s.spawning.is_none());
    }

    #[test]
    fn spawn_guard_leaves_a_fresh_indicator_alone() {
        let mut s = StatusCtx::new();
        s.note_spawning(
            "claude",
            "test:ws".into(),
            TerminalKind::Agent("claude".into()),
            0,
        );
        assert!(
            !s.tick_spawning(),
            "a just-started spawn must keep spinning"
        );
        assert!(s.spawning.is_some());
    }

    #[test]
    fn polling_tick_advances_the_spawn_spinner() {
        let mut s = StatusCtx::new();
        s.note_spawning(
            "claude",
            "test:ws".into(),
            TerminalKind::Agent("claude".into()),
            0,
        );
        // Backdate the heartbeat so the cadence gate (80ms) opens.
        s.polling_last_tick = Instant::now() - Duration::from_millis(200);
        let before = s.spawning.as_ref().unwrap().spinner_idx;
        let (_, advanced) = s.polling_tick();
        let after = s.spawning.as_ref().unwrap().spinner_idx;
        assert_eq!(after, before + 1, "spinner glyph advances on heartbeat");
        assert!(advanced, "a moved glyph must request a redraw");
    }

    /// Redraw gating for the run loop (spinner-driven re-render storm):
    /// a live indicator whose 80ms cadence gate hasn't opened yet must
    /// NOT report `advanced` — the glyph didn't move, so a ~16ms
    /// heartbeat between frames has nothing to repaint.
    #[test]
    fn polling_tick_between_frames_does_not_request_redraw() {
        let mut s = StatusCtx::new();
        s.note_spawning(
            "claude",
            "test:ws".into(),
            TerminalKind::Agent("claude".into()),
            0,
        );
        s.note_poll_progress("github", "Querying…");
        // Fresh heartbeat: the 80ms cadence gate is closed.
        s.polling_last_tick = Instant::now();
        let (msg, advanced) = s.polling_tick();
        assert!(msg.is_none());
        assert!(
            !advanced,
            "indicator exists but its glyph didn't move — no redraw"
        );
    }

    #[test]
    fn sync_log_records_and_caps() {
        let mut log = SyncLog::default();
        for i in 0..(SYNC_LOG_CAP + 10) {
            log.note_completed("github", i);
        }
        // Ring is bounded; oldest entries roll off the front.
        assert_eq!(log.recent().count(), SYNC_LOG_CAP);
    }

    #[test]
    fn sync_log_recent_is_most_recent_first() {
        let mut log = SyncLog::default();
        log.note_completed("github", 1);
        log.note_error("github", "auth", "bad token", "");
        let recent: Vec<_> = log.recent().collect();
        assert!(matches!(recent[0].outcome, SyncOutcome::Err { .. }));
        assert!(recent[0].source == "github");
        assert!(recent[1].is_ok());
    }

    #[test]
    fn sync_log_latest_per_source_keeps_newest_sorted() {
        let mut log = SyncLog::default();
        log.note_completed("linear", 3);
        log.note_completed("github", 1);
        log.note_error("github", "retryable", "rate limited", "");
        let latest = log.latest_per_source();
        // Sorted by source name: github, then linear.
        assert_eq!(latest[0].source, "github");
        assert!(matches!(latest[0].outcome, SyncOutcome::Err { .. }));
        assert_eq!(latest[1].source, "linear");
        assert!(latest[1].is_ok());
    }

    #[test]
    fn remote_status_is_hidden_until_a_box_is_configured() {
        // Default (NotConfigured) shows nothing — the local-only path is
        // unchanged (#1066).
        let s = StatusCtx::new();
        assert!(s.remote_status_busy().is_none());
        assert!(s.remote_status_steady().is_none());
    }

    #[test]
    fn remote_steady_state_shows_a_static_indicator() {
        let mut s = StatusCtx::new();
        s.note_remote_state(RemoteConnState::Connected {
            name: "obin".into(),
        });
        // A connected box is steady, not busy.
        assert!(s.remote_status_busy().is_none());
        let (glyph, label) = s.remote_status_steady().expect("connected shows");
        assert_eq!(glyph, "⇅");
        assert_eq!(label, "connected: obin");
    }

    #[test]
    fn remote_busy_state_animates_and_outranks_steady() {
        let mut s = StatusCtx::new();
        s.note_remote_state(RemoteConnState::Connecting);
        // Busy is surfaced by remote_status_busy (spinner); steady is None.
        assert!(s.remote_status_steady().is_none());
        let (_, label) = s.remote_status_busy().expect("connecting shows");
        assert_eq!(label, "connecting…");

        // The heartbeat advances the busy spinner and requests a redraw.
        s.polling_last_tick = Instant::now() - Duration::from_millis(200);
        let before = s.remote_spinner_idx;
        let (_, advanced) = s.polling_tick();
        assert_eq!(s.remote_spinner_idx, before + 1, "busy glyph advances");
        assert!(advanced);
    }

    #[test]
    fn remote_steady_state_does_not_animate() {
        let mut s = StatusCtx::new();
        s.note_remote_state(RemoteConnState::Disconnected);
        s.polling_last_tick = Instant::now() - Duration::from_millis(200);
        let before = s.remote_spinner_idx;
        let (_, advanced) = s.polling_tick();
        assert_eq!(s.remote_spinner_idx, before, "steady glyph is static");
        assert!(!advanced, "a static indicator needs no redraw");
    }

    #[test]
    fn bg_poll_guard_clears_after_long_silence() {
        let mut s = StatusCtx::new();
        s.note_poll_progress("github", "Querying…");
        // Backdate the last_event well past the guard so the
        // assertion isn't sensitive to the exact constant. (Guard
        // was bumped 20s → 90s when long-PR-details polls were
        // misread as stuck; the test now backdates 5 min.)
        if let Some(bg) = s.bg_poll.as_mut() {
            bg.last_event = Instant::now() - Duration::from_secs(300);
        }
        assert!(s.tick_bg_poll(), "stale bg_poll cleared by guard");
        assert!(s.bg_poll.is_none());
    }
}
