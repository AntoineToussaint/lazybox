//! Generic "first press arms, second press fires" latch.
//!
//! Reusable primitive for flows where repeating an action on the same
//! target confirms it. Destructive lazybox actions now prefer the richer
//! confirmation modal; the latch remains a small, independently-tested
//! building block for consumers that need a double-press interaction.
//!
//! Contract:
//! - `arm_or_fire(target)` returns `true` when the latch already held
//!   `target` (the user pressed twice in a row on the same row); the
//!   caller treats that as "fire."
//! - Returns `false` on the first press; the caller treats that as
//!   "arm" and renders the appropriate `[…?]` indicator.
//! - `disarm()` clears the latch (called from the handler when the
//!   user presses something other than the latch's key).
//!
//! Tested in isolation here; the call sites pin their per-action
//! semantics separately.

/// "First-press arms, second-press fires" latch. `K` is whatever the
/// caller uses to identify the action target — typically a workspace
/// key. The latch only fires when the second press matches the
/// armed key, so navigating to a different row between presses
/// disarms automatically.
#[derive(Debug, Default, Clone)]
pub struct ConfirmLatch<K> {
    armed: Option<K>,
}

impl<K: PartialEq + Clone> ConfirmLatch<K> {
    pub fn new() -> Self {
        Self { armed: None }
    }

    /// First press on `target` → arms; returns `false` ("don't fire
    /// yet"). Second press on the same `target` → fires; returns
    /// `true` and clears the latch. Press on a different `target`
    /// re-arms with the new one.
    pub fn arm_or_fire(&mut self, target: K) -> bool {
        if self.armed.as_ref() == Some(&target) {
            self.armed = None;
            return true;
        }
        self.armed = Some(target);
        false
    }

    /// Force-clear the latch. Called when any non-latch key arrives
    /// so unrelated input cannot leave a pending action armed.
    pub fn disarm(&mut self) {
        self.armed = None;
    }

    /// Currently armed target, for rendering `[kill?]` /
    /// `[merge?]` markers next to the row.
    pub fn armed(&self) -> Option<&K> {
        self.armed.as_ref()
    }
}

/// Time-based "armed for `delay`" latch. Same family as
/// `ConfirmLatch`, different trigger — instead of a second press,
/// the elapsed time gates the fire. Used by the right pane's
/// auto-mark-read timer (which used to live as a hand-rolled
/// `Option<Instant>` field with mutations scattered across ~10
/// call sites).
///
/// Contract:
/// - `arm()` records "now" as the start of the countdown.
/// - `disarm()` clears the latch.
/// - `ready(delay)` returns `true` iff the latch is armed and at
///   least `delay` has elapsed.
/// - `progress(delay)` returns `Some(0.0..=1.0)` while armed, for
///   rendering a progress bar.
#[derive(Debug, Default, Clone)]
pub struct TimerLatch {
    armed_at: Option<std::time::Instant>,
}

impl TimerLatch {
    pub fn new() -> Self {
        Self { armed_at: None }
    }

    /// Arm the timer. **Idempotent** — when already armed, the
    /// existing `armed_at` is left untouched so the elapsed time
    /// keeps accruing toward `delay`. Without this, callers that
    /// re-arm on every state-change tick (e.g. the right pane
    /// re-arms on every `WorkspaceUpserted`) would never see the
    /// timer elapse, because each tick reset `armed_at` back to
    /// `Instant::now`. Symptom this guards against: user sits on
    /// an unread activity > the poll interval (60s default) and
    /// auto-mark-read never fires.
    pub fn arm(&mut self) {
        if self.armed_at.is_none() {
            self.armed_at = Some(std::time::Instant::now());
        }
    }

    /// Force a fresh arm — sets `armed_at` to `now` even when
    /// already armed. Use sparingly; most call sites want the
    /// idempotent `arm`. The right pane uses this when the cursor
    /// MOVES to a new unread row — the previous row's accumulated
    /// time shouldn't count toward the new one.
    pub fn rearm_fresh(&mut self) {
        self.armed_at = Some(std::time::Instant::now());
    }

    pub fn disarm(&mut self) {
        self.armed_at = None;
    }

    pub fn is_armed(&self) -> bool {
        self.armed_at.is_some()
    }

    /// True iff armed and `delay` has elapsed since `arm`.
    pub fn ready(&self, delay: std::time::Duration) -> bool {
        self.armed_at.map(|t| t.elapsed() >= delay).unwrap_or(false)
    }

    /// Elapsed fraction of `delay`, clamped to `[0.0, 1.0]`. None
    /// when disarmed.
    pub fn progress(&self, delay: std::time::Duration) -> Option<f32> {
        let elapsed = self.armed_at?.elapsed();
        let ratio = elapsed.as_secs_f32() / delay.as_secs_f32();
        Some(ratio.clamp(0.0, 1.0))
    }
}

/// "Two presses within a window" latch. Same family as
/// `ConfirmLatch` (two-press) and `TimerLatch` (elapsed-delay),
/// but the trigger is "second press arrived BEFORE the window
/// expired" — used for `q q` quit and `]]` exit-terminal.
///
/// Replaces hand-rolled `Option<Instant>` fields that previously
/// lived inline on the orchestrator (`q_armed_at`,
/// `escape_armed_at`) with the arm / disarm mutations scattered
/// across ~15 call sites each. The contract:
///
/// - `tap(window)`: first press arms; second press within `window`
///   fires (returns `true`); second press past `window` re-arms.
/// - `disarm()`: any other key clears the latch (caller invokes
///   this after dispatching a non-latch key, just like every
///   other latch in the family).
///
/// `is_armed` lets the footer / hint bar render a "press again to
/// confirm" indicator without exposing the timestamp.
#[derive(Debug, Default, Clone)]
pub struct DoubleTapLatch {
    armed_at: Option<std::time::Instant>,
}

impl DoubleTapLatch {
    pub fn new() -> Self {
        Self { armed_at: None }
    }

    /// First press → arm; returns `false` ("don't fire yet").
    /// Second press within `window` → fire and clear; returns `true`.
    /// Second press past `window` → re-arm with a fresh timestamp;
    /// returns `false`.
    pub fn tap(&mut self, window: std::time::Duration) -> bool {
        if let Some(t) = self.armed_at
            && t.elapsed() <= window
        {
            self.armed_at = None;
            return true;
        }
        self.armed_at = Some(std::time::Instant::now());
        false
    }

    /// Force-clear the latch. Called after dispatching any
    /// non-latch key so a typed `q j` doesn't leave the quit
    /// prompt armed.
    pub fn disarm(&mut self) {
        self.armed_at = None;
    }

    pub fn is_armed(&self) -> bool {
        self.armed_at.is_some()
    }

    /// True when a single tap has been held (armed) for strictly longer
    /// than `window` — the second tap never came, so the held key should
    /// be released as a literal rather than kept waiting for a chord.
    pub fn armed_past(&self, window: std::time::Duration) -> bool {
        self.armed_at.map(|t| t.elapsed() > window).unwrap_or(false)
    }
}

/// One-shot "consume the next key" prefix latch.
///
/// Third member of the latch family: `ConfirmLatch` keys off a
/// payload (two presses of the same target fire), `TimerLatch`
/// keys off elapsed time, and `PrefixLatch` keys off "did the
/// previous key arm this?" — a tmux-style prefix where the next
/// keystroke is interpreted as a tile-management action instead
/// of being forwarded to the focused PTY.
///
/// Replaces the hand-rolled `ctrl_w_armed: bool` field in
/// `TerminalStack`. The mutation pattern was the same as the
/// other latches (arm on prefix, disarm on consume, force-disarm
/// on context change) but lived inline as ad-hoc `self.x = bool`
/// assignments — pulling it into a typed struct lets tests cover
/// the contract once instead of through every consumer.
///
/// Contract:
/// - `arm()` flips the latch on.
/// - `take()` returns the current value and unconditionally
///   disarms — `Ctrl-w` + any next key should consume the prefix
///   exactly once, even if the next key isn't a recognised tile
///   action.
/// - `disarm()` force-clears (called from `set_layout`-style
///   transitions where any in-flight prefix should evaporate).
#[derive(Debug, Default, Clone)]
pub struct PrefixLatch {
    armed_at: Option<std::time::Instant>,
}

impl PrefixLatch {
    pub fn new() -> Self {
        Self { armed_at: None }
    }

    pub fn arm(&mut self) {
        self.armed_at = Some(std::time::Instant::now());
    }

    pub fn disarm(&mut self) {
        self.armed_at = None;
    }

    pub fn is_armed(&self) -> bool {
        self.armed_at.is_some()
    }

    /// Read-and-disarm. Returns `true` iff the latch was armed; the
    /// latch is always cleared on return (so a single `Ctrl-w` +
    /// non-tile-action key reliably resets to "forward everything
    /// to the PTY").
    pub fn take(&mut self) -> bool {
        self.armed_at.take().is_some()
    }

    /// Read-and-disarm, but only HONOR the prefix when it was armed
    /// within `window`. A prefix left armed longer than that — a lone
    /// `Ctrl-w` the user never followed up — is treated as expired so
    /// the next key reaches the PTY normally instead of being silently
    /// eaten as a tile action much later. The latch is cleared either
    /// way.
    pub fn take_within(&mut self, window: std::time::Duration) -> bool {
        match self.armed_at.take() {
            // Strict `<` so a zero (or instant-resolution) window reads as
            // expired even when `elapsed()` rounds to exactly zero on a fast
            // clock — matching the documented "any elapsed time exceeds a zero
            // window" contract and keeping the behavior deterministic.
            Some(t) => t.elapsed() < window,
            None => false,
        }
    }
}

/// Operator-pending "leader" latch — fifth member of the latch
/// family, backing the grouped two-step (leader-key) chords from
/// issue #126. The first key of the chord arms it with the selected
/// group `G`; the next key picks the action within that group.
///
/// Unlike `DoubleTapLatch` it is deliberately NOT timed: it stays
/// armed until the next key arrives (vim operator-pending style). A
/// timed second-key window — like the existing 1s quit / escape
/// windows — risks the "laggy or misfires" failure mode the issue
/// flags, and the which-key popup gives the user an explicit pending-
/// state cue instead of a hidden clock.
///
/// Contract:
/// - `arm(group)` selects the pending group (replacing any prior one).
/// - `pending()` exposes it for the which-key popup without consuming.
/// - `take()` reads-and-disarms — the second keystroke resolves the
///   chord exactly once whether or not it names a valid in-group key.
/// - `disarm()` force-clears (context change / cancel).
#[derive(Debug, Default, Clone)]
pub struct LeaderLatch<G> {
    armed: Option<G>,
}

impl<G: Clone> LeaderLatch<G> {
    pub fn new() -> Self {
        Self { armed: None }
    }

    /// Arm with the group selected by the leader key. A second leader
    /// press before the action key just re-selects.
    pub fn arm(&mut self, group: G) {
        self.armed = Some(group);
    }

    /// Currently armed group, for rendering the which-key popup.
    pub fn pending(&self) -> Option<&G> {
        self.armed.as_ref()
    }

    pub fn is_armed(&self) -> bool {
        self.armed.is_some()
    }

    /// Read-and-disarm. Returns the armed group (if any) and clears,
    /// so the action-selector keystroke resolves the chord once.
    pub fn take(&mut self) -> Option<G> {
        self.armed.take()
    }

    pub fn disarm(&mut self) {
        self.armed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_press_arms_does_not_fire() {
        let mut latch: ConfirmLatch<&'static str> = ConfirmLatch::new();
        assert!(!latch.arm_or_fire("a"));
        assert_eq!(latch.armed(), Some(&"a"));
    }

    #[test]
    fn second_press_on_same_target_fires_and_clears() {
        let mut latch: ConfirmLatch<&'static str> = ConfirmLatch::new();
        latch.arm_or_fire("a");
        assert!(latch.arm_or_fire("a"));
        assert_eq!(latch.armed(), None);
    }

    #[test]
    fn second_press_on_different_target_re_arms() {
        // Repeating the action on a different row is a fresh arm, not
        // a confirmation of the first row.
        let mut latch: ConfirmLatch<&'static str> = ConfirmLatch::new();
        latch.arm_or_fire("a");
        assert!(!latch.arm_or_fire("b"));
        assert_eq!(latch.armed(), Some(&"b"));
    }

    #[test]
    fn disarm_clears_armed_state() {
        let mut latch: ConfirmLatch<&'static str> = ConfirmLatch::new();
        latch.arm_or_fire("a");
        latch.disarm();
        assert_eq!(latch.armed(), None);
    }

    #[test]
    fn fire_after_disarm_just_re_arms() {
        let mut latch: ConfirmLatch<&'static str> = ConfirmLatch::new();
        latch.arm_or_fire("a");
        latch.disarm();
        assert!(!latch.arm_or_fire("a"));
        assert_eq!(latch.armed(), Some(&"a"));
    }

    // ── TimerLatch tests ─────────────────────────────────────────

    #[test]
    fn timer_starts_disarmed() {
        let t = TimerLatch::new();
        assert!(!t.is_armed());
        assert!(!t.ready(std::time::Duration::from_millis(10)));
        assert_eq!(t.progress(std::time::Duration::from_millis(10)), None);
    }

    #[test]
    fn timer_arm_sets_armed_flag() {
        let mut t = TimerLatch::new();
        t.arm();
        assert!(t.is_armed());
    }

    #[test]
    fn timer_disarm_clears_flag() {
        let mut t = TimerLatch::new();
        t.arm();
        t.disarm();
        assert!(!t.is_armed());
    }

    #[test]
    fn timer_progress_grows_while_armed() {
        let mut t = TimerLatch::new();
        t.arm();
        // Immediately after arm the ratio is near 0 but not None.
        let p = t.progress(std::time::Duration::from_secs(1));
        assert!(p.is_some_and(|r| (0.0..=1.0).contains(&r)));
    }

    #[test]
    fn timer_ready_after_delay() {
        let mut t = TimerLatch::new();
        t.arm();
        std::thread::sleep(std::time::Duration::from_millis(15));
        assert!(t.ready(std::time::Duration::from_millis(10)));
    }

    /// Regression for the "user sits on a row > poll interval and
    /// auto-mark never fires" bug. Pre-fix, every poll's
    /// WorkspaceUpserted re-armed the timer, resetting `armed_at`
    /// back to `now`. The timer never accumulated past the rearm
    /// cadence. Now `arm()` is idempotent: a re-arm while already
    /// armed leaves `armed_at` untouched.
    #[test]
    fn timer_arm_is_idempotent_while_armed() {
        let mut t = TimerLatch::new();
        t.arm();
        std::thread::sleep(std::time::Duration::from_millis(15));
        // Re-arm — must NOT reset the countdown.
        t.arm();
        assert!(
            t.ready(std::time::Duration::from_millis(10)),
            "idempotent arm must preserve elapsed time",
        );
    }

    /// `rearm_fresh` is the explicit "I want a new countdown" path —
    /// used by the right pane when the cursor moves to a different
    /// row. Distinct from `arm()` so the call sites are visibly
    /// different at the seams: idempotent arm-on-tick vs.
    /// reset-on-cursor-move.
    #[test]
    fn timer_rearm_fresh_resets_countdown_even_when_armed() {
        let mut t = TimerLatch::new();
        t.arm();
        std::thread::sleep(std::time::Duration::from_millis(15));
        t.rearm_fresh();
        // 0ms has elapsed since the rearm — the 10ms threshold can't
        // possibly be ready yet.
        assert!(
            !t.ready(std::time::Duration::from_millis(10)),
            "rearm_fresh must reset armed_at to now",
        );
        // But still armed.
        assert!(t.is_armed());
    }

    // ── PrefixLatch tests ────────────────────────────────────────

    #[test]
    fn prefix_starts_disarmed() {
        let p = PrefixLatch::new();
        assert!(!p.is_armed());
    }

    #[test]
    fn prefix_arm_sets_flag() {
        let mut p = PrefixLatch::new();
        p.arm();
        assert!(p.is_armed());
    }

    #[test]
    fn prefix_take_when_armed_returns_true_and_disarms() {
        let mut p = PrefixLatch::new();
        p.arm();
        assert!(p.take());
        assert!(!p.is_armed(), "take must clear the latch");
    }

    #[test]
    fn prefix_take_when_disarmed_returns_false() {
        let mut p = PrefixLatch::new();
        assert!(!p.take());
        assert!(!p.is_armed());
    }

    #[test]
    fn prefix_take_within_honors_window_and_always_clears() {
        // A generous window honors a freshly-armed prefix.
        let mut p = PrefixLatch::new();
        p.arm();
        assert!(p.take_within(std::time::Duration::from_secs(60)));
        assert!(!p.is_armed(), "honored take clears the latch");

        // A zero window is already exceeded by any elapsed time, so a
        // prefix reads as expired — and is cleared either way so it can't
        // eat a later key.
        p.arm();
        assert!(!p.take_within(std::time::Duration::ZERO));
        assert!(!p.is_armed(), "expired take still clears the latch");

        // Disarmed → false.
        assert!(!p.take_within(std::time::Duration::from_secs(60)));
    }

    #[test]
    fn prefix_disarm_clears_armed_state() {
        // `set_layout` and similar context-change transitions
        // force-clear; this is the contract behind that.
        let mut p = PrefixLatch::new();
        p.arm();
        p.disarm();
        assert!(!p.is_armed());
    }

    // ── DoubleTapLatch tests ─────────────────────────────────────

    #[test]
    fn double_tap_first_press_arms_does_not_fire() {
        let mut l = DoubleTapLatch::new();
        assert!(!l.tap(std::time::Duration::from_secs(1)));
        assert!(l.is_armed());
    }

    #[test]
    fn double_tap_second_within_window_fires_and_clears() {
        let mut l = DoubleTapLatch::new();
        l.tap(std::time::Duration::from_secs(1));
        assert!(l.tap(std::time::Duration::from_secs(1)));
        assert!(!l.is_armed());
    }

    #[test]
    fn double_tap_second_past_window_re_arms() {
        let mut l = DoubleTapLatch::new();
        l.tap(std::time::Duration::from_millis(10));
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!l.tap(std::time::Duration::from_millis(10)));
        assert!(l.is_armed(), "stale arm refreshed, not fired");
    }

    #[test]
    fn double_tap_disarm_clears() {
        let mut l = DoubleTapLatch::new();
        l.tap(std::time::Duration::from_secs(1));
        l.disarm();
        assert!(!l.is_armed());
    }

    // ── LeaderLatch tests ────────────────────────────────────────

    #[test]
    fn leader_starts_disarmed() {
        let l: LeaderLatch<&'static str> = LeaderLatch::new();
        assert!(!l.is_armed());
        assert_eq!(l.pending(), None);
    }

    #[test]
    fn leader_arm_then_pending_exposes_group_without_consuming() {
        let mut l: LeaderLatch<&'static str> = LeaderLatch::new();
        l.arm("github");
        assert!(l.is_armed());
        assert_eq!(l.pending(), Some(&"github"));
        // pending() must not consume — the popup reads it every frame.
        assert!(l.is_armed());
    }

    #[test]
    fn leader_take_reads_and_disarms() {
        let mut l: LeaderLatch<&'static str> = LeaderLatch::new();
        l.arm("github");
        assert_eq!(l.take(), Some("github"));
        assert!(!l.is_armed(), "take must clear the latch");
        assert_eq!(l.take(), None, "second take is empty");
    }

    #[test]
    fn leader_re_arm_replaces_group() {
        let mut l: LeaderLatch<&'static str> = LeaderLatch::new();
        l.arm("github");
        l.arm("workspace");
        assert_eq!(l.pending(), Some(&"workspace"));
    }

    #[test]
    fn leader_disarm_clears() {
        let mut l: LeaderLatch<&'static str> = LeaderLatch::new();
        l.arm("github");
        l.disarm();
        assert!(!l.is_armed());
    }

    #[test]
    fn prefix_second_take_is_false() {
        // The `Ctrl-w` + 'j' + 'k' sequence: 'k' must NOT be
        // treated as a tile action because 'j' already consumed
        // the prefix.
        let mut p = PrefixLatch::new();
        p.arm();
        assert!(p.take()); // 'j' lands on tile-action handler
        assert!(!p.take()); // 'k' does not
    }
}
