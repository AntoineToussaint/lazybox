//! Free helper functions used by the model layer:
//!
//! - **Layout / mouse hit-testing**: `rect_contains`, `split_for_footer`.
//! - **Rendering**: `paint_selection` (drag-selection reverse-video),
//!   `placeholder` (dev scaffold).
//! - **Key / catalog**: `key_event_to_chord` (crossterm → catalog
//!   chord), `find_action_for_chord` (catalog lookup honoring user
//!   overrides).
//! - **Clipboard**: `emit_clipboard_copy` (OSC 52).
//! - **Run loop entry points**: `run_with_client`, `run_loop_with_model`.
//! - **Loop-health guards**: `should_drop_stale_input` /
//!   `StaleInputTally` (bounded input replay after a stall),
//!   `LoopWatchdog` (frame-budget overrun logging),
//!   `BacklogMonitor` (daemon-event backlog logging).
//! - **Misc encoders**: `base64_encode` (OSC 52 payload).
//!
//! The run loop is single-threaded: dispatch, update, and render all
//! happen here, so any blocking call freezes the whole UI. Blocking
//! primitives are banned crate-wide via `crates/tui/clippy.toml`
//! (`disallowed-methods`); the unified idle wait and the input reader
//! thread are the only sanctioned exceptions.
//!
//! Most consumers are siblings (`keys.rs`, `events.rs`, the `view`
//! and `update` methods on `Model`). Co-locating the helpers here
//! keeps mod.rs focused on the `Model` struct + its constructors.

use super::{Model, PaneFocus};
use lazybox_ipc::{Client, Event as IpcEvent};
use std::time::Duration;
use tuirealm::application::PollStrategy;
use tuirealm::event::{Event as RealmEvent, Key, KeyEvent as RealmKey, KeyModifiers};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::widgets::{Block, Borders};
use tuirealm::terminal::TerminalAdapter;

/// True if `(col, row)` lies within `rect`'s half-open bounds.
pub(crate) fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Paint a drag-selection range as reverse-video over `rect`. `start`
/// and `end` are screen coordinates from the mouse events; we
/// normalize so the lower-row end is the start and the higher-row end
/// is the end, then highlight cells in the visual range:
///
/// - Single-row selection: cells from `min_col` to `max_col`.
/// - Multi-row selection: from `start_col` to end-of-row on the start
///   row, full rows between, and start-of-row to `end_col` on the
///   final row.
///
/// All writes are clipped to `rect` so a drag that strayed outside
/// the terminal pane can't recolor lazybox's sidebar or activity feed.
pub(crate) fn paint_selection(
    buf: &mut ratatui::buffer::Buffer,
    rect: Rect,
    start: (u16, u16),
    end: (u16, u16),
) {
    use ratatui::style::Modifier;
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let max_x = rect.x.saturating_add(rect.width.saturating_sub(1));
    let max_y = rect.y.saturating_add(rect.height.saturating_sub(1));
    // Normalize so `a` is row-earlier or equal to `b`.
    let (a, b) = if (start.1, start.0) <= (end.1, end.0) {
        (start, end)
    } else {
        (end, start)
    };
    // Clamp endpoints to the terminal rect.
    let clamp = |p: (u16, u16)| (p.0.clamp(rect.x, max_x), p.1.clamp(rect.y, max_y));
    let a = clamp(a);
    let b = clamp(b);
    // No-op for a degenerate "click without drag" — Up handler
    // already skips the copy in that case; the highlight pass would
    // just reverse-video one cell, which is more confusing than
    // helpful.
    if a == b {
        return;
    }
    let mut y = a.1;
    while y <= b.1 {
        let row_start = if y == a.1 { a.0 } else { rect.x };
        let row_end = if y == b.1 { b.0 } else { max_x };
        let (lo, hi) = if row_start <= row_end {
            (row_start, row_end)
        } else {
            (row_end, row_start)
        };
        let mut x = lo;
        while x <= hi {
            // `buf[(x, y)]` is bounds-checked but our clamp already
            // guarantees in-range; this just sets the modifier
            // without touching the underlying char.
            let cell = &mut buf[(x, y)];
            cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
            x = x.saturating_add(1);
        }
        if y == max_y {
            break;
        }
        y = y.saturating_add(1);
    }
}

/// Convert a crossterm `KeyEvent` to a typed `KeyStroke` for catalog
/// lookup. Uppercase letters auto-shift so `KeyEvent { Char('M'),
/// no_mods }` produces the same stroke as `KeyEvent { Char('m'),
/// SHIFT }` — matches the catalog's parser convention. Returns
/// `None` for codes the catalog doesn't model (function keys,
/// release events).
pub(crate) fn key_event_to_stroke(
    key: crossterm::event::KeyEvent,
) -> Option<lazybox_tui_core::action::KeyStroke> {
    use crossterm::event::{KeyCode, KeyModifiers};
    use lazybox_tui_core::action::{ChordCode, KeyStroke, NamedKey};

    let mut shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let code = match key.code {
        KeyCode::Char(c) => {
            if c.is_ascii_uppercase() {
                shift = true;
            }
            ChordCode::Char(c.to_ascii_lowercase())
        }
        KeyCode::Tab => ChordCode::Named(NamedKey::Tab),
        KeyCode::Enter => ChordCode::Named(NamedKey::Enter),
        KeyCode::Esc => ChordCode::Named(NamedKey::Esc),
        KeyCode::Backspace => ChordCode::Named(NamedKey::Backspace),
        KeyCode::Up => ChordCode::Named(NamedKey::Up),
        KeyCode::Down => ChordCode::Named(NamedKey::Down),
        KeyCode::Left => ChordCode::Named(NamedKey::Left),
        KeyCode::Right => ChordCode::Named(NamedKey::Right),
        KeyCode::Home => ChordCode::Named(NamedKey::Home),
        KeyCode::End => ChordCode::Named(NamedKey::End),
        KeyCode::PageUp => ChordCode::Named(NamedKey::PageUp),
        KeyCode::PageDown => ChordCode::Named(NamedKey::PageDown),
        KeyCode::Delete => ChordCode::Named(NamedKey::Delete),
        KeyCode::Insert => ChordCode::Named(NamedKey::Insert),
        KeyCode::F(n) => ChordCode::Named(NamedKey::Function(n)),
        // Space is reported as Char(' ') by crossterm — covered by
        // the Char arm above. Unmodeled variants fall through to None.
        _ => return None,
    };
    Some(KeyStroke {
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        shift,
        alt,
        code,
    })
}

/// Look up the catalog entry whose effective chords contain the single
/// keystroke `stroke`, in the sections the focused pane should resolve.
/// Globals always match; pane-scoped sections only match when their
/// pane is focused; lower `section_rank` wins a tie.
///
/// `catalog` is the model's runtime catalog ([`ActionDef::catalog`]):
/// its chords already reflect `ui.action_keys` overrides AND carry the
/// generated per-agent `SpawnAgent` rows. Multi-keystroke (`Seq`)
/// bindings are ignored here — they resolve through
/// [`find_action_for_seq`] after a leader arms.
///
/// Returns `None` when no entry matches — the caller falls back to
/// leader-arming or the legacy pane match arms.
pub(crate) fn find_action_for_stroke<'c>(
    stroke: &lazybox_tui_core::action::KeyStroke,
    focus: PaneFocus,
    catalog: &'c [lazybox_tui_core::action::CatalogEntry],
) -> Option<&'c lazybox_tui_core::action::CatalogEntry> {
    use lazybox_tui_core::action::Chord;
    catalog
        .iter()
        .filter_map(|e| section_rank(e.section, focus).map(|rank| (rank, e)))
        .filter(|(_, e)| {
            e.chords
                .iter()
                .any(|c| matches!(c, Chord::Key(k) if k == stroke))
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, e)| e)
}

/// Resolve the catalog entry for a completed two-keystroke leader
/// sequence (`prefix` then `second`) — e.g. `g m` → merge. Mirrors
/// [`find_action_for_stroke`]'s focus/rank rules but matches
/// `Chord::Seq([prefix, second])`.
pub(crate) fn find_action_for_seq<'c>(
    prefix: &lazybox_tui_core::action::KeyStroke,
    second: &lazybox_tui_core::action::KeyStroke,
    focus: PaneFocus,
    catalog: &'c [lazybox_tui_core::action::CatalogEntry],
) -> Option<&'c lazybox_tui_core::action::CatalogEntry> {
    use lazybox_tui_core::action::Chord;
    catalog
        .iter()
        .filter_map(|e| section_rank(e.section, focus).map(|rank| (rank, e)))
        .filter(|(_, e)| {
            e.chords.iter().any(|c| {
                matches!(c, Chord::Seq(s) if s.len() == 2 && &s[0] == prefix && &s[1] == second)
            })
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, e)| e)
}

/// Every `(second-keystroke, entry)` reachable as a leader chord
/// starting with `prefix` under `focus`. Drives both the which-key
/// popup and the decision to arm a leader: a non-empty result means
/// `prefix` is a live leader. Pure function of the catalog + the
/// pressed prefix — the data-driven replacement for `ActionGroup`.
pub(crate) fn seq_continuations<'c>(
    prefix: &lazybox_tui_core::action::KeyStroke,
    focus: PaneFocus,
    catalog: &'c [lazybox_tui_core::action::CatalogEntry],
) -> Vec<(
    lazybox_tui_core::action::KeyStroke,
    &'c lazybox_tui_core::action::CatalogEntry,
)> {
    use lazybox_tui_core::action::Chord;
    let mut out = Vec::new();
    for e in catalog {
        if section_rank(e.section, focus).is_none() {
            continue;
        }
        for c in &e.chords {
            if let Chord::Seq(s) = c
                && s.len() == 2
                && &s[0] == prefix
            {
                out.push((s[1], e));
            }
        }
    }
    out
}

/// Resolution priority of a catalog section under the given focus.
/// `None` = unreachable from this focus; lower rank wins a chord
/// collision.
///
/// Globals always resolve, first. Beyond that, the pane that owns the
/// cursor's reference frame wins:
/// - Sidebar focus: the Workspace section.
/// - Right focus: the Activity section first (the row cursor lives
///   there — `z` undo-mark-read, `Shift-G` jump-to-bottom must beat
///   the Workspace section's `z` snooze / `Shift-G` assignees), then
///   the Workspace section, which stays reachable because the sidebar
///   selection is still the active reference frame while reading
///   activity (Reply / Shift-V / merge all dual-fire on purpose).
/// - Terminal focus never reaches the catalog: the terminal pane
///   forwards `all keys` to the PTY and the escape sequence (`]]`)
///   has its own latch logic.
pub(crate) fn section_rank(
    section: lazybox_tui_core::action::Section,
    focus: PaneFocus,
) -> Option<u8> {
    use lazybox_tui_core::action::Section;
    match (section, focus) {
        (Section::Global, _) => Some(0),
        (Section::Workspace, PaneFocus::Sidebar) => Some(1),
        // Sidebar list-management keys resolve ONLY under sidebar
        // focus — they manage the list, not the selected row, so
        // (unlike Workspace) they must not be reachable while the
        // activity pane has focus.
        (Section::Sidebar, PaneFocus::Sidebar) => Some(1),
        (Section::Activity, PaneFocus::Right) => Some(1),
        (Section::Workspace, PaneFocus::Right) => Some(2),
        _ => None,
    }
}

/// Carve the bottom row off for the footer. Returns
/// (pane_area, footer_area) — `pane_area` is what the three panes
/// fill; `footer_area` is the 1-row hint/status line at the bottom.
pub(crate) fn split_for_footer(area: Rect) -> (Rect, Rect) {
    if area.height < 2 {
        return (area, Rect::default());
    }
    let pane = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height - 1,
    };
    let footer = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    (pane, footer)
}

#[allow(dead_code)]
fn placeholder(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" lazybox · realm migration scaffold ")
        .borders(Borders::ALL);
    f.render_widget(block, area);
}

/// Run the realm-based lazybox loop with a pre-built IPC client.
/// `main.rs::run_embedded_realm` constructs the client + daemon pair
/// before calling this so the daemon is already serving when the UI
/// boots.
pub fn run_with_client(client: Client) -> anyhow::Result<()> {
    let mut model = Model::new(client)?;
    let result = run_loop(&mut model);
    model.shutdown();
    result
}

/// Test-only: run with an unconnected client. Useful for manual
/// smoke tests without spinning up the full daemon stack.
#[allow(dead_code)]
pub fn run() -> anyhow::Result<()> {
    let (client, _server) = lazybox_ipc::channel::pair();
    run_with_client(client)
}

/// Run the loop on a pre-configured model. Used by
/// `main::run_embedded_realm` so it can install the on-setup-complete
/// hook + start the wizard before entering the loop.
pub fn run_loop_with_model<T: TerminalAdapter>(mut model: Model<T>) -> anyhow::Result<()> {
    let result = run_loop(&mut model);
    model.shutdown();
    result
}

/// Hard cap on how many daemon events one loop iteration may process
/// before it MUST fall through to the keyboard read. The daemon emits
/// one `TerminalOutput` event per PTY chunk into an *unbounded*
/// channel, so a chatty agent (Claude Code streaming) can push events
/// faster than we drain them. An unbounded `while let Ok(..)` drain
/// then never sees `Empty`, the loop never reaches the input read, and
/// the user "can't type in the agent" until the burst ends. Bounding
/// the drain makes that input starvation impossible BY DESIGN: every
/// iteration services the keyboard within ~one frame no matter how
/// much output is in flight. Leftover events ride to the next
/// iteration (which is entered immediately — see `had_backlog`).
pub(super) const MAX_EVENTS_PER_TICK: usize = 256;
/// Wall-clock companion to the count cap: even cheap events add up, so
/// stop draining once we've spent this long regardless of count. Keeps
/// the keyboard responsive even if event handling is briefly slow.
const DRAIN_BUDGET: Duration = Duration::from_millis(8);

/// Drain queued daemon events, bounded by [`MAX_EVENTS_PER_TICK`] and
/// [`DRAIN_BUDGET`], coalescing adjacent same-terminal `TerminalOutput`
/// into a single dispatch before handling. Returns `true` when a cap
/// was hit with events still likely queued, so the caller can skip the
/// idle poll-wait and loop straight back — output keeps flowing at
/// full speed while the keyboard is still checked between every batch.
///
/// `carried` is the daemon event the unified idle wait pulled off the
/// channel to wake up (see [`wait_for_wake`]) — it's the oldest event
/// of this batch, so it goes first to preserve daemon-stream order.
///
/// Coalescing is what keeps memory bounded under a chatty agent: the
/// daemon emits one event per PTY chunk, and `vt.feed(a); vt.feed(b)`
/// is identical to `vt.feed(a ++ b)` (the parser is a byte stream), so
/// merging a streaming burst collapses hundreds of tiny events into one
/// `append_output` per terminal. The residual depth left in the
/// channel after the drain is handed to [`BacklogMonitor`] so a
/// consumer that's falling behind surfaces in the log.
pub(super) fn drain_daemon_events<T: TerminalAdapter>(
    model: &mut Model<T>,
    carried: Option<IpcEvent>,
) -> bool {
    let start = std::time::Instant::now();
    let mut collected: Vec<IpcEvent> = Vec::new();
    if let Some(evt) = carried {
        collected.push(evt);
    }
    let mut backlog = false;
    while let Ok(evt) = model.client.rx.try_recv() {
        collected.push(evt);
        if collected.len() >= MAX_EVENTS_PER_TICK || start.elapsed() >= DRAIN_BUDGET {
            // Hit a cap — there may be more queued. Signal a backlog so
            // the loop comes right back here after servicing input.
            backlog = true;
            break;
        }
    }
    // Count resyncs in this batch before dispatching — each one is a
    // daemon-side overflow that dropped `TerminalOutput` and rebuilt the
    // grid from the ring, so surfacing it makes drops observable in
    // `/tmp/lazybox.log` (the #87 BacklogMonitor's remit, now extended to
    // the actual drop signal rather than just a growing-backlog guess).
    let resyncs = collected
        .iter()
        .filter(|e| matches!(e, IpcEvent::TerminalResync { .. }))
        .count();
    for evt in coalesce_adjacent_output(collected) {
        model.dispatch_daemon_event(evt);
    }
    // One pane projection for the whole batch: a merge burst or a
    // multi-row poll moves the sidebar selection several times in a
    // single drain, but only the final selection needs projecting onto
    // the right pane + terminal stack (see `Model::dispatch_daemon_event`).
    model.flush_pane_sync();
    model.event_backlog.observe_resyncs(resyncs);
    // Whatever is still queued after this drain is the backlog the
    // consumer hasn't caught up on — feed it to the monitor.
    let residual = model.client.rx.len();
    model.event_backlog.observe(residual);
    backlog
}

/// Merge runs of consecutive `TerminalOutput` events that target the
/// same terminal into one event carrying the concatenated bytes and
/// the last chunk's `seq`. Order is otherwise preserved exactly — only
/// *adjacent* same-terminal output is merged, so an interleaved event
/// for another terminal (or any non-output event) ends the run. Pure;
/// unit-tested in `coalesce_tests`.
pub(super) fn coalesce_adjacent_output(events: Vec<IpcEvent>) -> Vec<IpcEvent> {
    let mut out: Vec<IpcEvent> = Vec::with_capacity(events.len());
    for evt in events {
        match evt {
            IpcEvent::TerminalOutput {
                terminal_id,
                bytes,
                seq,
            } => {
                if let Some(IpcEvent::TerminalOutput {
                    terminal_id: prev_id,
                    bytes: prev_bytes,
                    seq: prev_seq,
                }) = out.last_mut()
                    && *prev_id == terminal_id
                {
                    // Same terminal as the tail run — extend it.
                    prev_bytes.extend_from_slice(&bytes);
                    *prev_seq = seq;
                    continue;
                }
                out.push(IpcEvent::TerminalOutput {
                    terminal_id,
                    bytes,
                    seq,
                });
            }
            other => out.push(other),
        }
    }
    out
}

/// Depth above which a non-empty post-drain backlog is treated as the
/// consumer falling behind (vs. an ordinary single-frame burst).
const BACKLOG_WARN_THRESHOLD: usize = 1024;

/// Watches the inbound daemon-event channel for a backlog that doesn't
/// drain — the signature of the TUI consuming slower than the daemon
/// produces (a runaway producer, or a handler leaking time). Logging
/// only: it never blocks or drops, it just makes "we're falling
/// behind" visible in `/tmp/lazybox.log` instead of silent.
///
/// Healthy bursty load is silent — a warning fires only when the
/// residual climbs to a NEW high above [`BACKLOG_WARN_THRESHOLD`], so
/// a steady stream of warnings with a rising `residual` is the leak
/// signal.
#[derive(Default)]
pub(super) struct BacklogMonitor {
    /// Highest residual depth seen so far. Gates warnings to genuine
    /// new highs.
    hwm: usize,
    /// Consecutive drains that left the channel non-empty. A
    /// monotonically climbing count alongside a rising `residual` means
    /// the consumer never catches up.
    consecutive_backlog_ticks: u32,
    /// Total `TerminalResync` events seen — i.e. how many times the
    /// daemon's bounded event channel overflowed, dropped output, and
    /// rebuilt a terminal's grid from the ring. The hard-ceiling
    /// counterpart to `hwm`: with a bounded channel the backlog can no
    /// longer grow without bound, so this is the signal that drops
    /// actually happened.
    resyncs: u64,
}

impl BacklogMonitor {
    /// Record `n` resyncs observed in one drain. Logs at warn when any
    /// occurred so an overflow episode is greppable in `/tmp/lazybox.log`.
    pub(super) fn observe_resyncs(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.resyncs = self.resyncs.saturating_add(n as u64);
        tracing::warn!(
            resyncs = n,
            total = self.resyncs,
            "daemon dropped TerminalOutput on a full event channel and \
             re-synced the terminal grid from the ring — consumer fell \
             behind the producer (bounded-channel overflow)"
        );
    }

    /// Record the channel depth left after a drain. `residual` is the
    /// number of events still queued.
    pub(super) fn observe(&mut self, residual: usize) {
        if residual == 0 {
            if self.consecutive_backlog_ticks > 0 {
                tracing::debug!(
                    ticks = self.consecutive_backlog_ticks,
                    hwm = self.hwm,
                    "daemon-event backlog cleared"
                );
            }
            self.consecutive_backlog_ticks = 0;
            return;
        }
        self.consecutive_backlog_ticks = self.consecutive_backlog_ticks.saturating_add(1);
        if residual > self.hwm {
            self.hwm = residual;
            if residual >= BACKLOG_WARN_THRESHOLD {
                tracing::warn!(
                    residual,
                    consecutive_ticks = self.consecutive_backlog_ticks,
                    "daemon-event backlog growing — TUI consuming slower than the \
                     daemon produces (runaway producer or leak)"
                );
            }
        }
    }

    /// Test/diagnostic accessor: current consecutive-backlog streak.
    #[cfg(test)]
    pub(super) fn consecutive_backlog_ticks(&self) -> u32 {
        self.consecutive_backlog_ticks
    }

    /// Highest residual depth seen. Read by the perf sampler and tests.
    pub(super) fn hwm(&self) -> usize {
        self.hwm
    }

    /// Total resyncs observed. Read by the perf sampler and tests.
    pub(super) fn resyncs(&self) -> u64 {
        self.resyncs
    }
}

/// Upper bound on the idle wait: when nothing is queued on either
/// source the loop still wakes this often to run its periodic work
/// (latch timeouts like `q q` and the `]]` leader, spinner frames,
/// the modal-redraw window, `BacklogMonitor` observations). Events
/// interrupt the wait immediately — this is purely the heartbeat
/// floor, not a latency floor.
const POLL_IDLE: Duration = Duration::from_millis(16);

/// Bound on host-terminal input events buffered between the reader
/// thread and the run loop. Human input is tiny (bracketed paste
/// arrives as ONE `Paste` event, not per-char), so this never fills
/// in practice; if it somehow does, `blocking_send` parks the reader
/// thread — events then queue in the OS tty buffer exactly as they
/// did when the loop read crossterm directly.
const INPUT_CHANNEL_CAP: usize = 1024;

/// A host-terminal event stamped with when the reader thread pulled it
/// off the tty. The run loop normally consumes input within a frame
/// (~16ms), so `read_at.elapsed()` at dispatch time is effectively the
/// time the event sat buffered behind a stalled loop — the signal the
/// stale-input guard keys on.
pub(super) struct TimedInput {
    pub(super) read_at: std::time::Instant,
    pub(super) event: crossterm::event::Event,
}

/// Age past which a buffered key or mouse event is dropped instead of
/// dispatched. Under a healthy loop input is consumed within ~one
/// frame, so an event this old means the loop thread was blocked the
/// whole time the user was pressing keys and clicking at a frozen
/// screen. Replaying that backlog fires every queued action in a
/// burst against state the user never saw — including a buffered
/// quit chord. Generous enough that ordinary type-ahead during a
/// briefly busy loop is never touched.
pub(super) const STALE_INPUT_MAX_AGE: Duration = Duration::from_millis(500);

/// Whether a buffered input event should be discarded as stale.
/// Keys and mouse events are positional/stateful — they targeted UI
/// the user was looking at when they fired, and that UI is gone after
/// a stall. Paste stays: it's deliberate content, not an action, and
/// dropping it silently loses user data. Focus/resize stay: they
/// describe *current* terminal state regardless of when they fired.
pub(super) fn should_drop_stale_input(event: &crossterm::event::Event, age: Duration) -> bool {
    if age < STALE_INPUT_MAX_AGE {
        return false;
    }
    matches!(
        event,
        crossterm::event::Event::Key(_) | crossterm::event::Event::Mouse(_)
    )
}

/// Whether a host-terminal event is a mouse-wheel scroll. Scroll is the
/// one input that fires faster than a full repaint can keep up — a
/// trackpad flick emits dozens of notches a second — so its redraw is
/// routed through the background render throttle rather than painting
/// once per notch (see the run loop's render step). Dispatch of the
/// scroll itself stays per-event and cheap (it only moves a viewport
/// offset); it's the repaint that coalesces to the display refresh.
pub(super) fn is_scroll_event(event: &crossterm::event::Event) -> bool {
    use crossterm::event::MouseEventKind;
    matches!(
        event,
        crossterm::event::Event::Mouse(m) if matches!(
            m.kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        )
    )
}

/// Accumulates stale-input drops across a recovery burst so the
/// episode is reported once (one warn line, one footer notice) instead
/// of once per dropped event. `note` during the burst; `flush` when a
/// fresh event or idle tick shows the backlog has cleared.
#[derive(Default)]
pub(super) struct StaleInputTally {
    dropped: usize,
    oldest: Duration,
}

impl StaleInputTally {
    pub(super) fn note(&mut self, age: Duration) {
        self.dropped += 1;
        self.oldest = self.oldest.max(age);
    }

    /// Take the accumulated (count, oldest age) if any events were
    /// dropped since the last flush.
    pub(super) fn flush(&mut self) -> Option<(usize, Duration)> {
        if self.dropped == 0 {
            return None;
        }
        let report = (self.dropped, self.oldest);
        self.dropped = 0;
        self.oldest = Duration::ZERO;
        Some(report)
    }
}

/// Per-segment durations within a single run-loop work phase. The
/// watchdog records the whole phase as one number; this breaks that
/// number down so an over-budget warning names the segment that
/// blocked — render vs. daemon drain vs. a key handler — instead of
/// just "the UI thread froze for 80ms". That's the difference between
/// instrumentation you can act on and a number you have to guess from.
/// Filled in by the run loop with `Instant::now()` brackets (~10ns
/// each, negligible) and read only when the watchdog fires.
#[derive(Clone, Copy, Default)]
pub(super) struct PhaseTimings {
    /// Dispatching the event that woke the loop (key/mouse handling).
    pub(super) dispatch: Duration,
    /// Draining queued daemon events into the model.
    pub(super) drain: Duration,
    /// Per-frame heartbeat ticks (spinner, notice fade, right pane).
    pub(super) ticks: Duration,
    /// The tuirealm message pump plus the updates it produces.
    pub(super) messages: Duration,
    /// Rendering the frame.
    pub(super) render: Duration,
}

impl PhaseTimings {
    /// The longest segment and its duration — the prime suspect the
    /// watchdog names when the phase blows its budget.
    pub(super) fn worst(&self) -> (&'static str, Duration) {
        [
            ("dispatch", self.dispatch),
            ("drain", self.drain),
            ("ticks", self.ticks),
            ("messages", self.messages),
            ("render", self.render),
        ]
        .into_iter()
        .max_by_key(|&(_, d)| d)
        .unwrap_or(("none", Duration::ZERO))
    }
}

/// Budget for one run-loop iteration's work phase (drain + update +
/// render + dispatch, everything except the idle wait). Anything that
/// can wait must run as an async task and post back — an iteration
/// past this bound means something blocked the loop thread, which is
/// always a bug. The watchdog turns those from "the UI felt frozen"
/// field reports into warn lines in `/tmp/lazybox.log`.
pub(super) const FRAME_BUDGET: Duration = Duration::from_millis(50);

/// Minimum spacing between watchdog warn lines. A pathological case
/// (every iteration slow) logs a summary once a second instead of
/// flooding the log at frame rate.
const WATCHDOG_WARN_INTERVAL: Duration = Duration::from_secs(1);

/// Flags run-loop iterations that blow [`FRAME_BUDGET`]. Logging
/// only — it never throttles the loop, it makes a freeze observable
/// with its duration instead of only by feel.
#[derive(Default)]
pub(super) struct LoopWatchdog {
    last_warn_at: Option<std::time::Instant>,
    /// Over-budget iterations swallowed since the last warn line.
    suppressed: u32,
    /// Worst over-budget iteration among the suppressed ones.
    worst_suppressed: Duration,
}

impl LoopWatchdog {
    /// Record one iteration's work-phase duration, broken down into the
    /// per-segment `timings` so an over-budget warning names the culprit.
    /// Returns whether a warning was emitted (the return value exists for
    /// tests).
    pub(super) fn observe(
        &mut self,
        elapsed: Duration,
        timings: PhaseTimings,
        now: std::time::Instant,
    ) -> bool {
        if elapsed <= FRAME_BUDGET {
            return false;
        }
        let due = self
            .last_warn_at
            .is_none_or(|at| now.duration_since(at) >= WATCHDOG_WARN_INTERVAL);
        if !due {
            self.suppressed = self.suppressed.saturating_add(1);
            self.worst_suppressed = self.worst_suppressed.max(elapsed);
            return false;
        }
        let (worst_phase, worst_phase_dur) = timings.worst();
        tracing::warn!(
            iteration_ms = elapsed.as_millis() as u64,
            budget_ms = FRAME_BUDGET.as_millis() as u64,
            worst_phase,
            worst_phase_ms = worst_phase_dur.as_millis() as u64,
            dispatch_ms = timings.dispatch.as_millis() as u64,
            drain_ms = timings.drain.as_millis() as u64,
            ticks_ms = timings.ticks.as_millis() as u64,
            messages_ms = timings.messages.as_millis() as u64,
            render_ms = timings.render.as_millis() as u64,
            suppressed = self.suppressed,
            worst_suppressed_ms = self.worst_suppressed.as_millis() as u64,
            "run-loop iteration exceeded the frame budget — something \
             blocked the UI thread (input, rendering, and daemon events \
             were all frozen for this long)"
        );
        self.last_warn_at = Some(now);
        self.suppressed = 0;
        self.worst_suppressed = Duration::ZERO;
        true
    }
}

/// Minimum work-phase duration that earns a perf-log sample. Idle
/// heartbeat iterations (no drain, no render, no input) finish in a
/// few microseconds; sampling them would bury the signal under a
/// 60Hz stream of near-zero rows. A render (~1-2ms) or any dispatch
/// clears this floor, and over-budget or backlogged iterations sample
/// unconditionally.
const PERF_SAMPLE_FLOOR: Duration = Duration::from_micros(500);

/// Whether a run-loop iteration warrants a perf sample, given the
/// `LAZYBOX_PERF` flag and what the iteration did. Pulled out as a
/// pure predicate so the gating is unit-testable without the env var.
pub(super) fn sample_due(
    enabled: bool,
    elapsed: Duration,
    depth: usize,
    over_budget: bool,
) -> bool {
    enabled && (over_budget || depth > 0 || elapsed >= PERF_SAMPLE_FLOOR)
}

/// Opt-in (`LAZYBOX_PERF=1`) run-loop perf sampler. Routes the
/// watchdog's per-phase timings, channel depth, and drop/resync
/// counters to the dedicated [`crate::perf::TARGET`] tracing target
/// that `init_tracing` pipes to its own perf log. A no-op when the
/// flag is unset — it short-circuits before formatting any field.
pub(super) struct PerfMonitor {
    enabled: bool,
    /// Cumulative buffered-input events dropped as stale. The headline
    /// "must stay 0" health signal — a nonzero value means the loop
    /// stalled long enough to discard the user's keystrokes.
    dropped_input: u64,
}

impl PerfMonitor {
    pub(super) fn new() -> Self {
        Self {
            enabled: crate::perf::enabled(),
            dropped_input: 0,
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Fold a stale-input drop episode into the running total and mark
    /// it in the perf log. Drops are the one counter that must stay 0,
    /// so they get their own greppable line, not just a sample field.
    pub(super) fn note_dropped_input(&mut self, dropped: usize, oldest: Duration) {
        self.dropped_input = self.dropped_input.saturating_add(dropped as u64);
        if self.enabled {
            tracing::info!(
                target: crate::perf::TARGET,
                phase = "input_dropped",
                dropped,
                dropped_input_total = self.dropped_input,
                oldest_ms = oldest.as_millis() as u64,
                "buffered input dropped after a run-loop stall"
            );
        }
    }

    /// Emit one per-iteration perf sample. Skipped for idle heartbeat
    /// iterations (see [`sample_due`]) so the perf log stays signal.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn sample(
        &self,
        elapsed: Duration,
        timings: PhaseTimings,
        depth: usize,
        resyncs: u64,
        backlog_hwm: usize,
        over_budget: bool,
    ) {
        if !sample_due(self.enabled, elapsed, depth, over_budget) {
            return;
        }
        tracing::info!(
            target: crate::perf::TARGET,
            phase = "iteration",
            iteration_ms = elapsed.as_micros() as f64 / 1000.0,
            budget_ms = FRAME_BUDGET.as_millis() as u64,
            over_budget,
            dispatch_ms = timings.dispatch.as_micros() as f64 / 1000.0,
            drain_ms = timings.drain.as_micros() as f64 / 1000.0,
            ticks_ms = timings.ticks.as_micros() as f64 / 1000.0,
            messages_ms = timings.messages.as_micros() as f64 / 1000.0,
            render_ms = timings.render.as_micros() as f64 / 1000.0,
            chan_depth = depth,
            resyncs_total = resyncs,
            backlog_hwm,
            dropped_input_total = self.dropped_input,
            "run-loop iteration"
        );
    }

    /// Cumulative stale-input drops folded in so far.
    #[cfg(test)]
    pub(super) fn dropped_input(&self) -> u64 {
        self.dropped_input
    }
}

/// Minimum spacing between *background*-driven renders. A daemon
/// output flood drives `drain_daemon_events` at the drain-batch rate
/// (a fresh batch every ~8ms once a cap is hit), and the old loop
/// re-rendered the whole UI on every one of those batches — burning
/// the single loop thread on frames the user can't even read while
/// input dispatch waited its turn behind them. Coalescing background
/// frames to one display refresh caps that at ~60fps; input-driven
/// redraws bypass it entirely (see [`RenderThrottle::should_render`]),
/// so a scroll gesture still renders progressively per event — the
/// property the run loop's render comment is careful to preserve.
pub(super) const MIN_BACKGROUND_RENDER_INTERVAL: Duration = POLL_IDLE;

/// Rate-caps renders that aren't driven by user input. Input redraws
/// (a key, a scroll wheel tick) always render immediately so the UI
/// stays as responsive as the loop thread allows; renders driven by
/// daemon output or spinner ticks are coalesced to
/// [`MIN_BACKGROUND_RENDER_INTERVAL`] so an output burst degrades into
/// dropped *redundant frames*, never dropped keystrokes. A deferred
/// frame isn't lost — `model.redraw` stays set, so the next eligible
/// iteration (the `POLL_IDLE` heartbeat at worst) paints it within one
/// refresh.
#[derive(Default)]
pub(super) struct RenderThrottle {
    last_render: Option<std::time::Instant>,
}

impl RenderThrottle {
    /// Whether a pending redraw should paint now. Input-driven redraws
    /// always do. Background redraws wait until a refresh interval has
    /// elapsed since the last paint; the very first frame (no prior
    /// paint) always renders so startup isn't delayed.
    pub(super) fn should_render(&self, now: std::time::Instant, input_driven: bool) -> bool {
        if input_driven {
            return true;
        }
        match self.last_render {
            None => true,
            Some(last) => now.duration_since(last) >= MIN_BACKGROUND_RENDER_INTERVAL,
        }
    }

    /// Record that a frame painted at `now`.
    pub(super) fn record(&mut self, now: std::time::Instant) {
        self.last_render = Some(now);
    }
}

/// What woke the run loop's unified idle wait.
pub(super) enum Wake {
    /// Host-terminal event from the crossterm reader thread, stamped
    /// with its read time so the loop can drop it if it went stale
    /// behind a stall.
    Input(TimedInput),
    /// One daemon event pulled off `client.rx` by the wait itself.
    /// Carried into the next iteration's [`drain_daemon_events`] as
    /// the head of the batch so daemon-stream order is preserved.
    /// Boxed: `IpcEvent` is ~250 bytes (inline output buffers) and
    /// the other variants are small.
    Daemon(Box<IpcEvent>),
    /// Idle heartbeat (timeout elapsed, or a source closed).
    Tick,
}

/// The executor the run loop blocks on for its unified wait. The loop
/// runs on a `spawn_blocking` thread, so normally we borrow the
/// ambient runtime's handle (`Handle::block_on` from a blocking
/// thread is the supported pattern). Test/standalone callers without
/// a runtime get a private current-thread runtime instead — `block_on`
/// on an owned runtime drives its timers itself.
pub(super) enum LoopRuntime {
    Handle(tokio::runtime::Handle),
    Owned(tokio::runtime::Runtime),
}

impl LoopRuntime {
    pub(super) fn acquire() -> anyhow::Result<Self> {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => Ok(Self::Handle(handle)),
            Err(_) => Ok(Self::Owned(
                tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()?,
            )),
        }
    }

    // The ONE sanctioned block_on in this crate: the unified idle
    // wait parks here until input / a daemon event / the heartbeat.
    // Everything else must stay off the loop thread — see
    // crates/tui/clippy.toml.
    #[allow(clippy::disallowed_methods)]
    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        match self {
            Self::Handle(handle) => handle.block_on(fut),
            Self::Owned(rt) => rt.block_on(fut),
        }
    }
}

/// Spawn the dedicated crossterm reader thread. `crossterm::event::
/// read()` is a blocking call with no async story, so it gets its own
/// thread that forwards every host-terminal event into a bounded
/// channel the run loop can select on. The thread is detached on
/// purpose: at quit it's parked inside `read()`, and joining it would
/// block shutdown on the user pressing one more key — instead the
/// receiver drops with the loop, the next forwarded event hits a
/// closed channel, and the thread exits (or process exit reaps it).
///
/// Reading from a side thread is safe across the mouse-capture toggle
/// (F8 / Alt-s), bracketed paste, and the shutdown restore sequence:
/// those are all stdout writes / termios changes on the main thread,
/// independent of the blocked stdin read.
fn spawn_input_reader() -> anyhow::Result<tokio::sync::mpsc::Receiver<TimedInput>> {
    let (tx, rx) = tokio::sync::mpsc::channel(INPUT_CHANNEL_CAP);
    std::thread::Builder::new()
        .name("lazybox-input".into())
        .spawn(move || {
            loop {
                // The dedicated reader thread is the one place the
                // blocking crossterm read is allowed — see
                // crates/tui/clippy.toml.
                #[allow(clippy::disallowed_methods)]
                let read = crossterm::event::read();
                match read {
                    Ok(event) => {
                        let timed = TimedInput {
                            read_at: std::time::Instant::now(),
                            event,
                        };
                        if tx.blocking_send(timed).is_err() {
                            // Run loop is gone — nobody to deliver to.
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("input reader thread exiting: {e}");
                        break;
                    }
                }
            }
        })?;
    Ok(rx)
}

/// Block until SOMETHING needs the loop: a host-terminal event, a
/// daemon event, or the idle heartbeat. This is the latency fix for
/// "typing in the embedded terminal feels laggy": the old loop blocked
/// in `crossterm::event::poll(16ms)`, which only input could
/// interrupt — every daemon event (keystroke echo, streaming agent
/// output) sat in the channel for up to a full 16ms until the poll
/// expired. Selecting over both sources means either one wakes the
/// loop immediately; the sleep only wins when truly idle.
///
/// `biased` checks input first so a ready keystroke is never queued
/// behind a ready output event — input latency is at worst what it was
/// when the loop blocked on input alone. Daemon events stay in
/// `client.rx` until this wait pops at most ONE to wake on; the
/// bounded channel (and the daemon's drop-and-resync overflow path)
/// remains the only buffering and the only backpressure point.
///
/// A `None` recv (source closed: reader thread died, daemon hung up)
/// flips the corresponding `*_open` flag so the branch is disabled on
/// subsequent calls — a closed channel must degrade to the heartbeat,
/// not a busy spin.
pub(super) fn wait_for_wake(
    rt: &LoopRuntime,
    input_rx: &mut tokio::sync::mpsc::Receiver<TimedInput>,
    input_open: &mut bool,
    daemon_rx: &mut tokio::sync::mpsc::Receiver<IpcEvent>,
    daemon_open: &mut bool,
    idle: Duration,
) -> Wake {
    rt.block_on(async {
        tokio::select! {
            biased;
            event = input_rx.recv(), if *input_open => match event {
                Some(event) => Wake::Input(event),
                None => {
                    *input_open = false;
                    Wake::Tick
                }
            },
            event = daemon_rx.recv(), if *daemon_open => match event {
                Some(event) => Wake::Daemon(Box::new(event)),
                None => {
                    *daemon_open = false;
                    Wake::Tick
                }
            },
            () = tokio::time::sleep(idle) => Wake::Tick,
        }
    })
}

fn run_loop<T: TerminalAdapter>(model: &mut Model<T>) -> anyhow::Result<()> {
    let rt = LoopRuntime::acquire()?;
    let mut input_rx = spawn_input_reader()?;
    let mut input_open = true;
    let mut daemon_open = true;
    // Daemon event the previous idle wait woke on — head of the next
    // drain batch (see `Wake::Daemon`).
    let mut carried: Option<IpcEvent> = None;
    let mut stale_tally = StaleInputTally::default();
    let mut watchdog = LoopWatchdog::default();
    let mut perf = PerfMonitor::new();
    let mut render_throttle = RenderThrottle::default();
    // Set when the redraw pending at the top of the loop was produced
    // by user input (a dispatched key/mouse, or a modal interaction) —
    // those paint immediately; everything else is rate-capped. One-shot:
    // consumed and cleared by the render step each iteration.
    let mut redraw_is_input = false;
    // Start of the current iteration's work phase — reset right after
    // each wait so the watchdog never counts time spent idle.
    let mut work_start = std::time::Instant::now();
    // Per-segment latency of the current work phase. Reset alongside
    // `work_start`; the dispatch of the wake event lands here first
    // (bottom of the loop), then the top-of-loop segments, then the
    // watchdog reads it.
    let mut timings = PhaseTimings::default();
    while !model.quit {
        // 1. Drain inbound daemon events — BOUNDED so heavy PTY output
        // can never starve keyboard input (see `drain_daemon_events`).
        let drain_start = std::time::Instant::now();
        let had_backlog = drain_daemon_events(model, carried.take());
        timings.drain = drain_start.elapsed();

        // 2. Polling-modal spinner heartbeat + retryable notice fade.
        let ticks_start = std::time::Instant::now();
        if let Some(msg) = model.polling_tick() {
            model.dismiss_polling();
            model.update(msg);
        }
        model.tick_notice();
        model.tick_tips();
        model.tick_right();
        model.tick_working();
        model.tick_terminal_leader();
        model.tick_work_leader();
        timings.ticks = ticks_start.elapsed();

        // 3. Process tuirealm-side messages (timer ticks for Loading,
        // injected modal keys). Non-blocking — listener thread already
        // queued any work it had.
        let messages_start = std::time::Instant::now();
        if let Ok(messages) = model.app.tick(PollStrategy::Once(Duration::ZERO)) {
            if !messages.is_empty() {
                model.redraw = true;
                for msg in messages {
                    model.update(msg);
                }
            }
        }
        timings.messages = messages_start.elapsed();

        // A modal key just forwarded to the listener is delivered
        // asynchronously and may mutate the modal without producing a
        // `Msg` (Confirm arrows, Input typing). Re-render across the
        // short window armed by `forward_modal_event` so the change
        // shows up without blocking on it above.
        if model.modal_redraw_pending() {
            model.redraw = true;
            // A modal key the user just pressed — paint it without
            // waiting on the background frame cap.
            redraw_is_input = true;
        }

        // 4. Render if dirty — before the blocking input read so the
        // user sees their last action immediately. Background-driven
        // frames (daemon output, spinner ticks) and high-rate mouse-wheel
        // scroll are coalesced to one display refresh so neither an output
        // flood nor a trackpad flick can saturate the render path;
        // discrete input (keystrokes, clicks) bypasses the cap and paints
        // at once. A frame the throttle defers keeps `model.redraw` set,
        // so it paints on the next eligible iteration (≤ one refresh away).
        if model.redraw {
            let now = std::time::Instant::now();
            if render_throttle.should_render(now, redraw_is_input) {
                // Per-frame timing log behind the `lazybox=debug`
                // filter. Lets us see in `/tmp/lazybox.log` whether a
                // slow scroll is the render itself (would show large
                // `frame_ms`) versus daemon round-trips between
                // renders. Cheap — `Instant::now` is ~10ns and
                // `tracing::debug!` is a no-op when the level isn't on.
                model.view();
                timings.render = now.elapsed();
                let elapsed_ms = timings.render.as_micros() as f32 / 1000.0;
                tracing::debug!(frame_ms = elapsed_ms, "render");
                model.redraw = false;
                render_throttle.record(now);
            }
        }
        redraw_is_input = false;

        // Emit any OSC desktop notifications queued during this
        // iteration's drain. Here — after the frame flush, on the
        // render thread — is the one point where the escape bytes
        // can't interleave with a half-written ratatui frame, which
        // would paint the payload as literal text and lose the
        // banner (#296).
        crate::notify::flush_pending_osc();

        // 5. Block on the unified wait. One input event per
        // iteration, render between events — the "drain all then
        // render once" pattern looked good on paper (fewer renders
        // per second) but broke scroll fluidity: a 30-event trackpad
        // gesture collapsed into a single jump-cut render, so the
        // user saw the screen teleport from start to end with no
        // intermediate frames ("not progressive, I don't even see
        // which direction I'm going"). The render cost is 1-2ms
        // (verified via the `render frame_ms` debug log) so per-
        // event rendering at 50-100Hz easily keeps up.
        //
        // `POLL_IDLE` is the IDLE-WAIT bound: with nothing queued we
        // block up to one display-refresh worth so the periodic work
        // above keeps its ~16ms heartbeat. Any event on EITHER source
        // interrupts the wait immediately (see `wait_for_wake`) — we
        // never pay the 16ms when there's work; during an active
        // scroll or output burst this loop runs as fast as render +
        // daemon-roundtrip allows, which is what gives the
        // progressive-scroll feel.
        //
        // When the daemon drain hit its cap (`had_backlog`), there are
        // more events already waiting — don't block at all: service
        // any pending key non-blocking and loop straight back to
        // drain the rest. That keeps output flowing at full speed
        // without ever blocking the keyboard behind it.
        //
        // The work phase ends here — everything past this line is the
        // blocking wait. An over-budget reading means some handler or
        // render above stalled the thread; it lands in the log with
        // its duration instead of surfacing only as a frozen UI.
        let work_elapsed = work_start.elapsed();
        let over_budget = work_elapsed > FRAME_BUDGET;
        if watchdog.observe(work_elapsed, timings, std::time::Instant::now()) && perf.enabled() {
            // The watchdog rate-limits to ≤1 warn/s, so this footer
            // flash never floods. Gated behind LAZYBOX_PERF so a normal
            // session's UX is untouched — it's a debug-time signal.
            let (worst_phase, worst_dur) = timings.worst();
            model.flash_perf_stall(work_elapsed, worst_phase, worst_dur);
        }
        perf.sample(
            work_elapsed,
            timings,
            model.client.rx.len(),
            model.event_backlog.resyncs(),
            model.event_backlog.hwm(),
            over_budget,
        );
        let wake = if had_backlog {
            match input_rx.try_recv() {
                Ok(timed) => Wake::Input(timed),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => Wake::Tick,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    input_open = false;
                    Wake::Tick
                }
            }
        } else {
            wait_for_wake(
                &rt,
                &mut input_rx,
                &mut input_open,
                &mut model.client.rx,
                &mut daemon_open,
                POLL_IDLE,
            )
        };
        work_start = std::time::Instant::now();
        timings = PhaseTimings::default();
        match wake {
            Wake::Input(timed) => {
                // Input that sat buffered while the loop was stalled
                // is discarded instead of replayed — a recovered UI
                // must never burst-fire seconds of queued clicks and
                // keystrokes (least of all a buffered quit chord).
                let age = timed.read_at.elapsed();
                if should_drop_stale_input(&timed.event, age) {
                    stale_tally.note(age);
                } else {
                    report_stale_drops(model, &mut stale_tally, &mut perf);
                    let is_scroll = is_scroll_event(&timed.event);
                    let dispatch_start = std::time::Instant::now();
                    dispatch_event(model, timed.event);
                    timings.dispatch = dispatch_start.elapsed();
                    // Keystrokes and clicks are low-rate and paint at once
                    // so the UI feels instant. Mouse-wheel scroll is the
                    // exception: a flick outpaces a full repaint, so
                    // painting per notch makes the loop fall behind, queued
                    // wheel events age past the stale bound, and they get
                    // dropped — surfacing as the "UI stalled" flash on an
                    // otherwise-idle screen. Routing scroll redraws through
                    // the background throttle coalesces them to the display
                    // refresh: every notch still updates the offset, but
                    // the screen repaints at ~60fps instead of per event.
                    redraw_is_input = !is_scroll;
                }
            }
            // Any non-input wake means the buffered-input burst is
            // over (`biased` drains input first) — report the episode
            // now rather than deferring it behind a busy agent stream.
            Wake::Daemon(event) => {
                report_stale_drops(model, &mut stale_tally, &mut perf);
                carried = Some(*event);
            }
            Wake::Tick => report_stale_drops(model, &mut stale_tally, &mut perf),
        }
    }
    Ok(())
}

/// Surface a finished stale-drop episode: one warn line for the log,
/// one footer notice so the user knows their queued input was
/// deliberately discarded (and why nothing "happened" when the UI
/// came back). No-op while the tally is empty.
fn report_stale_drops<T: TerminalAdapter>(
    model: &mut Model<T>,
    tally: &mut StaleInputTally,
    perf: &mut PerfMonitor,
) {
    if let Some((dropped, oldest)) = tally.flush() {
        tracing::warn!(
            dropped,
            oldest_ms = oldest.as_millis() as u64,
            "dropped input events buffered while the run loop was \
             stalled — replaying them would burst-fire actions against \
             state the user never saw"
        );
        perf.note_dropped_input(dropped, oldest);
        model.flash_hint(format!(
            "UI stalled — dropped {dropped} buffered input event{}",
            if dropped == 1 { "" } else { "s" }
        ));
    }
}

/// Route one crossterm event to the right handler. Extracted from
/// the run-loop body so the loop can `dispatch_event` once per
/// poll, then poll(0) to drain the rest before rendering — the
/// batching is what turns 20 scroll-wheel events into 1 frame.
fn dispatch_event<T: TerminalAdapter>(model: &mut Model<T>, event: crossterm::event::Event) {
    match event {
        crossterm::event::Event::Key(key) => {
            // With KeyboardEnhancementFlags::REPORT_EVENT_TYPES pushed
            // at startup, the host terminal distinguishes Press /
            // Repeat / Release. We skip Release only — Repeat must
            // be honored so held keys autorepeat (arrow keys in
            // Claude code, holding j to scroll, etc.). The previous
            // filter skipped Repeat too, which made every "held key"
            // feel broken even though Backspace worked (Backspace
            // events arrive as Press from the terminal's auto-repeat
            // emulation when extended keyboards aren't on).
            if matches!(key.kind, crossterm::event::KeyEventKind::Release) {
                return;
            }
            let realm_key = crossterm_to_realm(key);
            if model.modal_stack.is_empty() {
                model.handle_pane_key(realm_key);
            } else {
                // Hand the key to the listener channel and return
                // immediately — the run loop's `app.tick` delivers the
                // resulting `Msg` on a later iteration. Blocking here
                // (the old 150ms busy-wait) froze the dispatcher on
                // every keystroke, which is exactly when the
                // out-of-scope Confirm modal is shown during sync:
                // daemon events backed up and keys (incl. `Y`) appeared
                // to drop. `forward_modal_event` arms a redraw window
                // so the modal still re-renders for keys that mutate
                // state without emitting a `Msg`.
                model.forward_modal_event(RealmEvent::Keyboard(realm_key));
            }
        }
        crossterm::event::Event::Mouse(m) => {
            if model.modal_stack.is_empty() {
                model.handle_mouse(m);
            } else if model.dismiss_modal_on_outside_click(m) {
                // A non-blocking overlay was up: the press closed it and
                // then did its normal thing (focus a pane, select a
                // workspace). Nothing left to forward.
            } else if let Some(realm_mouse) = crossterm_mouse_to_realm(m) {
                // A modal owns input — route button presses to it so
                // its buttons respond to clicks. Only presses are
                // forwarded; drag/move/scroll noise stays out of the
                // modal's event queue. Forwarded non-blocking like keys
                // (see the keyboard arm) so the dispatcher never stalls
                // on modal input.
                model.forward_modal_event(RealmEvent::Mouse(realm_mouse));
            }
        }
        crossterm::event::Event::Paste(text) => {
            // Bracketed paste arrived. Two destinations depending on
            // where focus is — both go through `handle_paste` which
            // inspects pane state.
            if model.modal_stack.is_empty() {
                model.handle_paste(&text);
            } else {
                // Modal owns input — forward as raw text via the
                // modal event channel. The textarea modal will see
                // this as a multi-char paste and insert at cursor.
                model.forward_modal_event(RealmEvent::Paste(text));
            }
        }
        // The host terminal changed size (SIGWINCH). Ratatui's draw
        // autoresizes its buffers to the backend, but only a draw
        // does that — and nothing else guarantees one here, so an
        // unhandled resize left the UI painted for the old size until
        // some unrelated event forced a frame. Full clear + repaint:
        // the clear also covers the size-unchanged reports some
        // terminals emit on fullscreen toggles, where a plain redraw
        // would diff to nothing against a screen the host rebuilt.
        crossterm::event::Event::Resize(_, _) => {
            model.force_full_redraw();
        }
        // Terminal focus changed (DEC mode 1004). Recorded process-
        // globally so `platform::notify_user` can suppress banners
        // while lazybox is the focused window. Regaining focus also
        // repaints from scratch: display sleep/wake and window
        // restores can wipe the host's screen without any resize
        // event, and this is the first signal we get afterwards.
        crossterm::event::Event::FocusGained => {
            crate::notify::set_terminal_focus(true);
            model.force_full_redraw();
        }
        crossterm::event::Event::FocusLost => {
            crate::notify::set_terminal_focus(false);
        }
    }
}

/// Translate crossterm's modifier bitflags into tuirealm's. Lazybox only
/// distinguishes Shift / Control / Alt — the rest (Super, Hyper, …) are
/// dropped.
fn convert_modifiers(m: crossterm::event::KeyModifiers) -> KeyModifiers {
    use crossterm::event::KeyModifiers as CKM;
    let mut out = KeyModifiers::empty();
    out.set(KeyModifiers::SHIFT, m.contains(CKM::SHIFT));
    out.set(KeyModifiers::CONTROL, m.contains(CKM::CONTROL));
    out.set(KeyModifiers::ALT, m.contains(CKM::ALT));
    out
}

/// Lift a crossterm mouse press into tuirealm's `MouseEvent`. Returns
/// `None` for everything but button-down — modals only care about
/// clicks, and forwarding drag/move/scroll would flood the channel.
fn crossterm_mouse_to_realm(
    m: crossterm::event::MouseEvent,
) -> Option<tuirealm::event::MouseEvent> {
    use crossterm::event::{MouseButton as CMB, MouseEventKind as CMK};
    use tuirealm::event::{MouseButton as RMB, MouseEventKind as RMK};

    let kind = match m.kind {
        CMK::Down(CMB::Left) => RMK::Down(RMB::Left),
        CMK::Down(CMB::Right) => RMK::Down(RMB::Right),
        CMK::Down(CMB::Middle) => RMK::Down(RMB::Middle),
        _ => return None,
    };
    Some(tuirealm::event::MouseEvent {
        kind,
        modifiers: convert_modifiers(m.modifiers),
        column: m.column,
        row: m.row,
    })
}

fn crossterm_to_realm(key: crossterm::event::KeyEvent) -> RealmKey {
    use crossterm::event::KeyCode as CKC;
    let code = match key.code {
        CKC::Char(c) => Key::Char(c),
        CKC::Enter => Key::Enter,
        CKC::Esc => Key::Esc,
        CKC::Backspace => Key::Backspace,
        CKC::Left => Key::Left,
        CKC::Right => Key::Right,
        CKC::Up => Key::Up,
        CKC::Down => Key::Down,
        CKC::Home => Key::Home,
        CKC::End => Key::End,
        CKC::PageUp => Key::PageUp,
        CKC::PageDown => Key::PageDown,
        CKC::Tab => Key::Tab,
        CKC::BackTab => Key::BackTab,
        CKC::Delete => Key::Delete,
        CKC::Insert => Key::Insert,
        CKC::F(n) => Key::Function(n),
        _ => Key::Null,
    };
    RealmKey::new(code, convert_modifiers(key.modifiers))
}

/// Write OSC 52 clipboard-set to the host terminal's stdout. The host
/// (Ghostty / iTerm2 / Kitty / WezTerm) lands the text on the system
/// clipboard. Format: `ESC ] 52 ; c ; <base64> ESC \`. Wraps the
/// lazybox-side "copy from terminal selection" gesture — without OSC 52
/// the extracted text would just live in memory.
pub(crate) fn emit_clipboard_copy(text: &str) {
    let encoded = base64_encode(text.as_bytes());
    let sequence = format!("\x1b]52;c;{encoded}\x1b\\");
    use std::io::Write;
    let _ = std::io::stdout().write_all(sequence.as_bytes());
    let _ = std::io::stdout().flush();
}

/// Tiny RFC 4648 base64 encoder. Lazybox doesn't have a `base64` dep
/// and pulling one in for one OSC 52 call is overkill. ~25 lines,
/// allocation-free aside from the output `String`.
pub(super) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

#[cfg(test)]
mod host_event_redraw_tests {
    //! The host-terminal events that must repaint the UI (issue #285).
    //! A resize (SIGWINCH) or a focus regain after display sleep/wake
    //! used to fall through `dispatch_event` without setting the
    //! redraw flag, so the screen stayed painted for the old size /
    //! stale content until an unrelated event forced a frame.
    use super::{Model, dispatch_event};
    use lazybox_ipc::channel;
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    #[test]
    fn resize_event_forces_redraw() {
        let mut m = build_model();
        m.redraw = false;
        dispatch_event(&mut m, crossterm::event::Event::Resize(200, 60));
        assert!(m.redraw, "a terminal resize must schedule a repaint");
    }

    #[test]
    fn focus_gained_forces_redraw() {
        let mut m = build_model();
        m.redraw = false;
        dispatch_event(&mut m, crossterm::event::Event::FocusGained);
        assert!(
            m.redraw,
            "regaining focus (e.g. after display sleep/wake) must repaint"
        );
    }

    #[test]
    fn focus_lost_does_not_redraw() {
        let mut m = build_model();
        m.redraw = false;
        dispatch_event(&mut m, crossterm::event::Event::FocusLost);
        assert!(!m.redraw, "losing focus needs no repaint");
    }

    /// The catalog's manual escape hatch (`Ctrl-l`) goes through the
    /// same full-repaint path as the host events.
    #[test]
    fn force_redraw_action_repaints() {
        let mut m = build_model();
        m.redraw = false;
        m.dispatch_action_unchecked(&lazybox_tui_core::action::Action::ForceRedraw);
        assert!(m.redraw, "the redraw action must schedule a repaint");
    }
}

#[cfg(test)]
mod collision_tests {
    use super::{PaneFocus, section_rank};
    use lazybox_tui_core::action::{ActionDef, Chord};
    use std::collections::HashMap;

    /// Collision detector across the *real* resolution model (issue
    /// #98). `find_action_for_chord` resolves a chord by collecting
    /// the catalog entries reachable from the focused pane, grouping
    /// by `section_rank`, and picking the lowest rank. Two entries
    /// that resolve at the SAME rank under the SAME focus are a true
    /// ambiguity — `min_by_key`'s tie-break is iteration order, so one
    /// is silently unreachable. The deliberate cross-rank shadowing
    /// (Workspace rank 2 under Right focus vs Activity rank 1) is
    /// fine and not flagged, because different ranks never tie.
    #[test]
    fn no_same_rank_chord_collisions_per_focus() {
        // Exercise the RUNTIME catalog (static rows + the generated
        // per-agent SpawnAgent rows) so `c` / `x` / `u` are collision-
        // checked alongside everything else.
        let agents: Vec<String> = ["claude", "codex", "cursor"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let catalog = ActionDef::catalog(&agents, &std::collections::BTreeMap::new());
        for focus in [PaneFocus::Sidebar, PaneFocus::Right, PaneFocus::Terminals] {
            let mut seen: HashMap<(u8, Chord), String> = HashMap::new();
            for entry in &catalog {
                let Some(rank) = section_rank(entry.section, focus) else {
                    continue;
                };
                // Every alternative (leader sequence AND legacy alias)
                // is a distinct binding — a collision on any one is a
                // genuine ambiguity.
                for chord in &entry.chords {
                    let id = format!("{:?}/{:?}", entry.kind, entry.param);
                    if let Some(prev) = seen.insert((rank, chord.clone()), id.clone()) {
                        panic!(
                            "under {focus:?}, chord {chord:?} resolves to two actions \
                             at rank {rank}: {prev} and {id}",
                        );
                    }
                }
            }
        }
    }
}
