//! Debounce-window coalescing for desktop-notification bursts.
//!
//! With 15–20 active agents, losing terminal focus used to unleash a
//! banner per workspace all at once: every rising edge that was
//! suppressed while focused (asking / done / rate-limited) fired its
//! own popup the moment focus was lost, and the render loop's OSC
//! flush emitted them back-to-back (#1370).
//!
//! This buffers queued [`PendingNotification`]s for a short window and
//! collapses a same-kind burst into a single summary banner
//! ("N agents need input") instead of N popups. A lone notification
//! still surfaces intact — only a genuine burst is summarized.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::components::sidebar::{NotificationKind, PendingNotification};
use lazybox_core::SessionKey;

/// How long to hold a notification before firing, giving a burst time
/// to accumulate so it can be collapsed. A tumbling window from the
/// first buffered notification — bounds the extra latency on a lone
/// banner to at most this long.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(500);

/// Buffers pending notifications and, once the debounce window has
/// elapsed, emits them with same-kind bursts collapsed into summaries.
#[derive(Default)]
pub struct NotificationCoalescer {
    buffer: Vec<PendingNotification>,
    /// When the current window opened — the arrival time of the first
    /// buffered notification. `None` when the buffer is empty.
    window_start: Option<Instant>,
}

impl NotificationCoalescer {
    /// Buffer a notification, opening the debounce window if this is
    /// the first one held.
    pub fn push(&mut self, now: Instant, notif: PendingNotification) {
        if self.buffer.is_empty() {
            self.window_start = Some(now);
        }
        self.buffer.push(notif);
    }

    /// Emit the buffered notifications once the window has elapsed,
    /// collapsing same-kind bursts into one summary each. Returns an
    /// empty vec while still inside the window or when nothing is
    /// buffered — call it each run-loop iteration.
    pub fn flush_due(&mut self, now: Instant) -> Vec<PendingNotification> {
        let Some(start) = self.window_start else {
            return Vec::new();
        };
        if now.duration_since(start) < COALESCE_WINDOW {
            return Vec::new();
        }
        self.window_start = None;
        summarize(std::mem::take(&mut self.buffer))
    }
}

/// Group notifications by kind (preserving first-seen kind order) and
/// collapse any group of two or more into a single summary banner.
fn summarize(notifs: Vec<PendingNotification>) -> Vec<PendingNotification> {
    let mut order: Vec<NotificationKind> = Vec::new();
    let mut groups: HashMap<NotificationKind, Vec<PendingNotification>> = HashMap::new();
    for n in notifs {
        if !groups.contains_key(&n.kind) {
            order.push(n.kind);
        }
        groups.entry(n.kind).or_default().push(n);
    }

    let mut out = Vec::new();
    for kind in order {
        let Some(group) = groups.remove(&kind) else {
            continue;
        };
        // Count distinct workspaces, not raw edges: one agent that flaps
        // in and out of a state within the window (plausible under an
        // event storm) must count once — otherwise a single workspace is
        // mislabeled as an "N agents" summary.
        let group = dedupe_by_workspace(group);
        if group.len() == 1 {
            out.extend(group);
        } else if let Some(summary) = summarize_group(kind, group) {
            out.push(summary);
        }
    }
    out
}

/// Collapse repeated notifications for the same workspace to a single
/// entry — the freshest one — while preserving first-seen order.
fn dedupe_by_workspace(group: Vec<PendingNotification>) -> Vec<PendingNotification> {
    let mut order: Vec<SessionKey> = Vec::new();
    let mut latest: HashMap<SessionKey, PendingNotification> = HashMap::new();
    for n in group {
        if !latest.contains_key(&n.workspace_key) {
            order.push(n.workspace_key.clone());
        }
        latest.insert(n.workspace_key.clone(), n);
    }
    order
        .into_iter()
        .filter_map(|k| latest.remove(&k))
        .collect()
}

/// Build one summary banner for a burst of same-kind notifications.
/// `group` is guaranteed non-empty by the caller.
fn summarize_group(
    kind: NotificationKind,
    group: Vec<PendingNotification>,
) -> Option<PendingNotification> {
    let count = group.len();
    let first = group.first()?;
    let workspace_key = first.workspace_key.clone();
    let headline = match kind {
        NotificationKind::Asking => format!("{count} agents need input"),
        NotificationKind::Done => format!("{count} agents finished"),
        NotificationKind::LimitReached => format!("{count} agents rate-limited"),
        NotificationKind::Activity => format!("{count} workspaces have new activity"),
    };
    Some(PendingNotification {
        title: format!("lazybox — {headline}"),
        body: summarize_names(&group),
        workspace_key,
        name: first.name.clone(),
        kind,
    })
}

/// A comma-joined list of the first few workspace names, with an
/// "+N more" tail so a large burst's body stays short.
fn summarize_names(group: &[PendingNotification]) -> String {
    const SHOWN: usize = 3;
    let names: Vec<&str> = group.iter().map(|n| n.name.as_str()).take(SHOWN).collect();
    let mut body = names.join(", ");
    let overflow = group.len().saturating_sub(SHOWN);
    if overflow > 0 {
        body.push_str(&format!(", +{overflow} more"));
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notif(name: &str, kind: NotificationKind) -> PendingNotification {
        notif_keyed(name, name, kind)
    }

    /// Build a notification with an explicit workspace key, so a test
    /// can push several edges for the *same* workspace.
    fn notif_keyed(key: &str, name: &str, kind: NotificationKind) -> PendingNotification {
        PendingNotification {
            title: format!("lazybox — {name} needs input"),
            body: name.to_string(),
            workspace_key: SessionKey::from(key.to_string()),
            name: name.to_string(),
            kind,
        }
    }

    #[test]
    fn holds_until_window_elapses() {
        let mut c = NotificationCoalescer::default();
        let t0 = Instant::now();
        c.push(t0, notif("alpha", NotificationKind::Asking));
        // Still inside the window: nothing fires yet.
        assert!(c.flush_due(t0).is_empty());
        assert!(c.flush_due(t0 + COALESCE_WINDOW / 2).is_empty());
        // Window elapsed: the lone notification surfaces intact.
        let out = c.flush_due(t0 + COALESCE_WINDOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "lazybox — alpha needs input");
        // Drained — a second flush is empty.
        assert!(c.flush_due(t0 + COALESCE_WINDOW * 2).is_empty());
    }

    #[test]
    fn empty_coalescer_flushes_nothing() {
        let mut c = NotificationCoalescer::default();
        assert!(c.flush_due(Instant::now()).is_empty());
    }

    #[test]
    fn collapses_same_kind_burst_into_one_summary() {
        let mut c = NotificationCoalescer::default();
        let t0 = Instant::now();
        for name in ["alpha", "bravo", "charlie", "delta", "echo"] {
            c.push(t0, notif(name, NotificationKind::Asking));
        }
        let out = c.flush_due(t0 + COALESCE_WINDOW);
        assert_eq!(out.len(), 1, "5 asking edges collapse to one banner");
        assert_eq!(out[0].title, "lazybox — 5 agents need input");
        // Body lists the first three names, then summarizes the tail.
        assert_eq!(out[0].body, "alpha, bravo, charlie, +2 more");
        assert_eq!(out[0].kind, NotificationKind::Asking);
    }

    #[test]
    fn separate_kinds_each_get_their_own_summary() {
        let mut c = NotificationCoalescer::default();
        let t0 = Instant::now();
        c.push(t0, notif("a", NotificationKind::Asking));
        c.push(t0, notif("b", NotificationKind::Asking));
        c.push(t0, notif("c", NotificationKind::Done));
        c.push(t0, notif("d", NotificationKind::Done));
        c.push(t0, notif("e", NotificationKind::LimitReached));
        c.push(t0, notif("f", NotificationKind::LimitReached));

        let out = c.flush_due(t0 + COALESCE_WINDOW);
        let titles: Vec<&str> = out.iter().map(|n| n.title.as_str()).collect();
        // First-seen kind order is preserved.
        assert_eq!(
            titles,
            vec![
                "lazybox — 2 agents need input",
                "lazybox — 2 agents finished",
                "lazybox — 2 agents rate-limited",
            ]
        );
    }

    #[test]
    fn same_workspace_flapping_counts_once() {
        // One workspace that flaps in and out of asking within the
        // window pushes two same-key edges. It must collapse to a single
        // passthrough banner, not a bogus "2 agents need input" summary.
        let mut c = NotificationCoalescer::default();
        let t0 = Instant::now();
        c.push(t0, notif_keyed("alpha", "alpha", NotificationKind::Asking));
        c.push(t0, notif_keyed("alpha", "alpha", NotificationKind::Asking));

        let out = c.flush_due(t0 + COALESCE_WINDOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "lazybox — alpha needs input");
    }

    #[test]
    fn summary_counts_distinct_workspaces_not_edges() {
        // Two distinct workspaces plus a duplicate edge for one of them
        // is still a two-agent burst.
        let mut c = NotificationCoalescer::default();
        let t0 = Instant::now();
        c.push(t0, notif_keyed("alpha", "alpha", NotificationKind::Asking));
        c.push(t0, notif_keyed("bravo", "bravo", NotificationKind::Asking));
        c.push(t0, notif_keyed("alpha", "alpha", NotificationKind::Asking));

        let out = c.flush_due(t0 + COALESCE_WINDOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "lazybox — 2 agents need input");
        assert_eq!(out[0].body, "alpha, bravo");
    }

    #[test]
    fn single_of_each_kind_passes_through_untouched() {
        let mut c = NotificationCoalescer::default();
        let t0 = Instant::now();
        c.push(t0, notif("solo-ask", NotificationKind::Asking));
        c.push(t0, notif("solo-done", NotificationKind::Done));

        let out = c.flush_due(t0 + COALESCE_WINDOW);
        assert_eq!(out.len(), 2);
        // Original single-workspace titles survive — no summarizing.
        assert_eq!(out[0].title, "lazybox — solo-ask needs input");
        assert_eq!(out[1].title, "lazybox — solo-done needs input");
    }

    #[test]
    fn window_reopens_after_a_flush() {
        let mut c = NotificationCoalescer::default();
        let t0 = Instant::now();
        c.push(t0, notif("first", NotificationKind::Asking));
        assert_eq!(c.flush_due(t0 + COALESCE_WINDOW).len(), 1);
        // A later notification starts a fresh window rather than firing
        // immediately off the stale first-arrival time.
        let t1 = t0 + COALESCE_WINDOW * 3;
        c.push(t1, notif("second", NotificationKind::Asking));
        assert!(c.flush_due(t1).is_empty());
        assert_eq!(c.flush_due(t1 + COALESCE_WINDOW).len(), 1);
    }
}
