//! `SyncStatus` — the debug / sync-status window (default `Shift-D`).
//!
//! Surfaces what provider polling is actually doing: the last sync
//! time and outcome per source, plus a scrollable log of recent sync
//! attempts with error detail. Without it a failing poll (rate limit,
//! auth, network, GH API error) leaves the inbox silently stale with
//! no way to tell from the UI.
//!
//! Read-only: navigation keys scroll the log; any other key dismisses.
//! Built from a snapshot of the `SyncLog` (`crate::realm::status_ctx`)
//! at mount, with `now` captured once so the relative ages don't drift
//! while the window is open.

use crate::realm::components::scrollable::{
    centered_rect, draw_frame, handle_scroll_key, max_scroll,
};
use crate::realm::status_ctx::{SyncEntry, SyncOutcome};
use crate::realm::{Msg, UserEvent};
use chrono::{DateTime, Utc};
use lazybox_core::time::time_ago_at;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::Event;
#[cfg(test)]
use tuirealm::event::{Key, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;
use tuirealm::state::State;

/// Debug / sync-status window.
pub(crate) struct SyncStatus {
    /// Latest attempt per source, sorted by source name.
    summary: Vec<SyncEntry>,
    /// Every retained attempt, most-recent-first.
    recent: Vec<SyncEntry>,
    governor: Option<String>,
    /// Daemon resource posture (2026-08-19 audit) + the client's own
    /// frame-budget overrun tally. `None` until the daemon's
    /// [`lazybox_ipc::Event::ResourcePosture`] reply lands — the
    /// section renders a "measuring…" placeholder meanwhile.
    posture: Option<(lazybox_ipc::ResourcePosture, u64)>,
    /// Reference instant for relative-time rendering, captured at
    /// mount so ages stay stable while the window is open.
    now: DateTime<Utc>,
    /// Topmost visible body line.
    scroll: u16,
    /// Body viewport height, cached in `view` for page jumps.
    body_height: u16,
}

impl SyncStatus {
    /// Build from a `SyncLog` snapshot. `summary` and `recent` are
    /// cloned out so the window renders a stable view.
    pub(crate) fn new(summary: Vec<SyncEntry>, recent: Vec<SyncEntry>, now: DateTime<Utc>) -> Self {
        Self {
            summary,
            recent,
            governor: None,
            posture: None,
            now,
            scroll: 0,
            body_height: 0,
        }
    }

    pub(crate) fn with_governor(mut self, governor: Option<String>) -> Self {
        self.governor = governor;
        self
    }

    /// Attach the daemon's resource posture plus the client-side
    /// frame-budget overrun tally (the daemon can't see the client's
    /// run loop).
    pub(crate) fn with_posture(
        mut self,
        posture: lazybox_ipc::ResourcePosture,
        client_frame_overruns: u64,
    ) -> Self {
        self.posture = Some((posture, client_frame_overruns));
        self
    }

    /// `12.3 MB` / `842 KB` / `97 B` for the posture byte figures.
    fn human_bytes(bytes: u64) -> String {
        const KB: u64 = 1024;
        const MB: u64 = KB * 1024;
        const GB: u64 = MB * 1024;
        match bytes {
            b if b >= GB => format!("{:.1} GB", b as f64 / GB as f64),
            b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
            b if b >= KB => format!("{} KB", b / KB),
            b => format!("{b} B"),
        }
    }

    /// The "Resource posture" section (2026-08-19 audit): the ratchets
    /// that caused the incident — agent fleet, log growth, bus loss,
    /// frozen frames — as at-a-glance figures, warn-toned when a
    /// figure deserves attention.
    fn posture_lines(&self, theme: &crate::theme::Theme) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(Span::styled(
            "Resource posture",
            theme.section_heading(),
        )));
        let Some((p, client_overruns)) = &self.posture else {
            lines.push(Line::from(Span::styled(
                "  measuring…",
                Style::default().fg(theme.text_dim),
            )));
            lines.push(Line::raw(""));
            return lines;
        };
        let dim = Style::default().fg(theme.text_dim);
        let warn = Style::default().fg(theme.warn).add_modifier(Modifier::BOLD);

        let agents_text = match p.agent_cap {
            Some(cap) => format!("  agents alive      {} / {} cap", p.live_agents, cap),
            None => format!("  agents alive      {} (uncapped)", p.live_agents),
        };
        let agents_hot = p.agent_cap.is_some_and(|cap| p.live_agents >= cap);
        lines.push(Line::from(Span::styled(
            agents_text,
            if agents_hot { warn } else { dim },
        )));
        lines.push(Line::from(Span::styled(
            format!("  terminals         {}", p.terminals),
            dim,
        )));
        let fmt_opt = |v: Option<u64>| v.map_or("unknown".to_string(), Self::human_bytes);
        lines.push(Line::from(Span::styled(
            format!("  log file          {}", fmt_opt(p.log_bytes)),
            dim,
        )));
        lines.push(Line::from(Span::styled(
            format!("  state.db          {}", fmt_opt(p.state_db_bytes)),
            dim,
        )));
        let bus_hot = p.bus_lagged_events > 0;
        lines.push(Line::from(Span::styled(
            format!(
                "  bus events missed {} ({} recovery snapshots)",
                p.bus_lagged_events, p.bus_lag_recoveries
            ),
            if bus_hot { warn } else { dim },
        )));
        if p.terminal_output_dropped > 0 || p.terminal_resyncs > 0 {
            lines.push(Line::from(Span::styled(
                format!(
                    "  output dropped    {} ({} grid resyncs)",
                    p.terminal_output_dropped, p.terminal_resyncs
                ),
                warn,
            )));
        }
        if p.inline_budget_violations > 0 {
            lines.push(Line::from(Span::styled(
                format!("  daemon slow cmds  {}", p.inline_budget_violations),
                warn,
            )));
        }
        let frames_hot = *client_overruns > 0;
        lines.push(Line::from(Span::styled(
            format!(
                "  frozen frames     {} (UI iterations over budget)",
                client_overruns
            ),
            if frames_hot { warn } else { dim },
        )));
        lines.push(Line::raw(""));
        lines
    }

    /// The scrollable body, as styled lines. Re-derived each render so
    /// theme + width changes are picked up.
    fn body_lines(&self, theme: &crate::theme::Theme) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = self.posture_lines(theme);

        if self.summary.is_empty() {
            if self.governor.is_none() {
                lines.push(Line::from(Span::styled(
                    "No sync activity yet — lazybox hasn't completed a poll.",
                    Style::default().fg(theme.text_dim),
                )));
                return lines;
            }
        }

        if let Some(governor) = &self.governor {
            lines.push(Line::from(Span::styled(
                "GitHub budget governor",
                theme.section_heading(),
            )));
            lines.push(Line::from(Span::styled(
                governor.clone(),
                Style::default().fg(theme.text_dim),
            )));
            lines.push(Line::raw(""));
        }

        if !self.summary.is_empty() {
            lines.push(Line::from(Span::styled(
                "Last sync per source",
                theme.section_heading(),
            )));
        }
        for e in &self.summary {
            lines.push(self.summary_line(e, theme));
        }
        if self.summary.iter().any(SyncEntry::is_rate_limited) {
            lines.push(Line::from(Span::styled(
                "      GitHub user quota is shared by lazybox + gh + agents.",
                Style::default().fg(theme.text_dim),
            )));
            lines.push(Line::from(Span::styled(
                "      Separate PATs for the same user do not create a new quota.",
                Style::default().fg(theme.text_dim),
            )));
        }

        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Recent attempts",
            theme.section_heading(),
        )));
        for e in &self.recent {
            lines.push(self.entry_line(e, theme));
            if let SyncOutcome::Err { detail, .. } = &e.outcome
                && !detail.is_empty()
            {
                for d in detail.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("      {d}"),
                        Style::default().fg(theme.text_dim),
                    )));
                }
            }
        }
        lines
    }

    /// One per-source summary row: `● github   ✓ 12 tasks · 2m ago`.
    fn summary_line(&self, e: &SyncEntry, theme: &crate::theme::Theme) -> Line<'static> {
        let dot_color = if e.is_ok() {
            theme.success
        } else if e.is_rate_limited() {
            theme.warn
        } else {
            theme.error
        };
        let ago = time_ago_at(&e.at, self.now);
        Line::from(vec![
            Span::styled("● ", Style::default().fg(dot_color)),
            Span::styled(
                format!("{:<10}", e.source),
                Style::default()
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD),
            ),
            outcome_span(e, theme, self.now),
            Span::styled(format!("  ·  {ago}"), Style::default().fg(theme.text_dim)),
        ])
    }

    /// One log row: `✓ github · 12 tasks · 2m ago`.
    fn entry_line(&self, e: &SyncEntry, theme: &crate::theme::Theme) -> Line<'static> {
        let (glyph, glyph_color) = if e.is_ok() {
            ("✓ ", theme.success)
        } else if e.is_rate_limited() {
            ("◷ ", theme.warn)
        } else {
            ("✗ ", theme.error)
        };
        let ago = time_ago_at(&e.at, self.now);
        Line::from(vec![
            Span::styled(glyph, Style::default().fg(glyph_color)),
            Span::styled(
                format!("{:<10}", e.source),
                Style::default().fg(theme.text_strong),
            ),
            outcome_span(e, theme, self.now),
            Span::styled(format!("  ·  {ago}"), Style::default().fg(theme.text_dim)),
        ])
    }
}

/// The human-readable outcome fragment shared by the summary and log
/// rows — `12 tasks` on success, `auth: bad credentials` on failure.
fn outcome_span(e: &SyncEntry, theme: &crate::theme::Theme, now: DateTime<Utc>) -> Span<'static> {
    match &e.outcome {
        SyncOutcome::Ok { count } => {
            let noun = if *count == 1 { "task" } else { "tasks" };
            Span::styled(
                format!("{count} {noun}"),
                Style::default().fg(theme.text_dim),
            )
        }
        SyncOutcome::RateLimited {
            remaining,
            limit,
            reset_at,
        } => Span::styled(
            format!(
                "rate-limited · {}",
                crate::realm::status_ctx::rate_limit_wait_detail(
                    *remaining, *limit, *reset_at, now
                )
            ),
            Style::default().fg(theme.warn),
        ),
        SyncOutcome::Err { kind, message, .. } => {
            let text = if kind.is_empty() {
                message.clone()
            } else {
                format!("{kind}: {message}")
            };
            Span::styled(text, Style::default().fg(theme.error))
        }
    }
}

impl Component for SyncStatus {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 100u16.min(area.width.saturating_sub(4));
        let modal_h = 24u16.min(area.height.saturating_sub(2));
        let modal = centered_rect(area, modal_w, modal_h);
        let inner = draw_frame(frame, modal, " Sync status ", theme);
        if inner.height < 2 {
            return;
        }

        // Reserve the bottom row for the hint line.
        let body_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height - 1,
        };
        let hint_area = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        self.body_height = body_area.height.max(1);

        let lines = self
            .body_lines(theme)
            .into_iter()
            .flat_map(|line| crate::components::comment_render::wrap_one(line, body_area.width))
            .collect::<Vec<_>>();
        // Clamp scroll so a short log can't leave blank rows scrolled
        // off the top.
        let max = max_scroll(lines.len(), self.body_height);
        if self.scroll > max {
            self.scroll = max;
        }
        frame.render_widget(Paragraph::new(lines).scroll((self.scroll, 0)), body_area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "↑/↓ scroll · any other key to close",
                theme.hint(),
            ))),
            hint_area,
        );
    }

    fn query(&self, _: Attribute) -> Option<QueryResult<'_>> {
        None
    }
    fn attr(&mut self, _: Attribute, _: AttrValue) {}
    fn state(&self) -> State {
        State::None
    }
    fn perform(&mut self, _: Cmd) -> CmdResult {
        CmdResult::NoChange
    }
}

impl AppComponent<Msg, UserEvent> for SyncStatus {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        let Event::Keyboard(key) = ev else {
            return None;
        };
        if handle_scroll_key(&mut self.scroll, self.body_height, key) {
            return None;
        }
        // Any other key (Esc, q, Enter, …) closes the window.
        Some(Msg::ModalDismissed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use tuirealm::event::KeyEvent;

    fn at(secs_ago: i64, now: DateTime<Utc>) -> DateTime<Utc> {
        now - chrono::Duration::seconds(secs_ago)
    }

    fn ok(source: &str, count: usize, secs_ago: i64, now: DateTime<Utc>) -> SyncEntry {
        SyncEntry {
            source: source.into(),
            at: at(secs_ago, now),
            outcome: SyncOutcome::Ok { count },
        }
    }

    fn err(source: &str, kind: &str, msg: &str, secs_ago: i64, now: DateTime<Utc>) -> SyncEntry {
        SyncEntry {
            source: source.into(),
            at: at(secs_ago, now),
            outcome: SyncOutcome::Err {
                kind: kind.into(),
                message: msg.into(),
                detail: String::new(),
            },
        }
    }

    fn rate_limited(now: DateTime<Utc>) -> SyncEntry {
        SyncEntry {
            source: "github".into(),
            at: now - chrono::Duration::seconds(5),
            outcome: SyncOutcome::RateLimited {
                remaining: 98,
                limit: 5000,
                reset_at: now + chrono::Duration::minutes(7),
            },
        }
    }

    fn render(comp: &mut SyncStatus, w: u16, h: u16) -> String {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|frame| comp.view(frame, Rect::new(0, 0, w, h)))
            .unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                let mut row = String::new();
                for x in 0..buf.area.width {
                    row.push_str(buf[(x, y)].symbol());
                }
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap()
    }

    #[test]
    fn empty_log_renders_placeholder() {
        let mut comp = SyncStatus::new(vec![], vec![], now());
        let out = render(&mut comp, 80, 12);
        assert!(out.contains("No sync activity yet"), "{out}");
        assert!(out.contains("Sync status"), "{out}");
    }

    #[test]
    fn renders_summary_and_recent_with_outcomes() {
        let n = now();
        let summary = vec![
            err("github", "auth", "bad credentials", 30, n),
            ok("linear", 4, 90, n),
        ];
        let recent = vec![
            err("github", "auth", "bad credentials", 30, n),
            ok("linear", 4, 90, n),
            ok("github", 12, 200, n),
        ];
        let mut comp = SyncStatus::new(summary, recent, n);
        let out = render(&mut comp, 90, 20);
        assert!(out.contains("Last sync per source"), "{out}");
        assert!(out.contains("Recent attempts"), "{out}");
        assert!(out.contains("auth: bad credentials"), "{out}");
        assert!(out.contains("4 tasks"), "{out}");
        assert!(out.contains("12 tasks"), "{out}");
    }

    /// The resource-posture section (2026-08-19 audit): renders its
    /// figures at the top, warn-flags the ratchets that need eyes
    /// (fleet at/over cap, bus loss, frozen frames), and shows
    /// "measuring…" until the daemon reply lands.
    #[test]
    fn renders_resource_posture_with_ratchet_flags() {
        let n = now();
        let mut waiting = SyncStatus::new(vec![ok("github", 4, 5, n)], Vec::new(), n);
        let out = render(&mut waiting, 90, 20);
        assert!(out.contains("Resource posture"), "{out}");
        assert!(out.contains("measuring…"), "{out}");

        let mut comp = SyncStatus::new(vec![ok("github", 4, 5, n)], Vec::new(), n).with_posture(
            lazybox_ipc::ResourcePosture {
                live_agents: 47,
                agent_cap: Some(32),
                terminals: 60,
                log_bytes: Some(138 * 1024 * 1024),
                state_db_bytes: Some(4 * 1024 * 1024),
                bus_lagged_events: 12,
                bus_lag_recoveries: 2,
                terminal_output_dropped: 0,
                terminal_resyncs: 0,
                inline_budget_violations: 0,
            },
            7,
        );
        let out = render(&mut comp, 90, 24);
        assert!(out.contains("agents alive      47 / 32 cap"), "{out}");
        assert!(out.contains("terminals         60"), "{out}");
        assert!(out.contains("138.0 MB"), "{out}");
        assert!(
            out.contains("bus events missed 12 (2 recovery snapshots)"),
            "{out}"
        );
        assert!(out.contains("frozen frames     7"), "{out}");
        assert!(
            out.find("Resource posture") < out.find("Last sync per source"),
            "posture leads the body: {out}"
        );
    }

    #[test]
    fn renders_governor_snapshot_before_sync_history() {
        let n = now();
        let mut comp = SyncStatus::new(vec![ok("github", 4, 5, n)], vec![ok("github", 4, 5, n)], n)
            .with_governor(Some(
                "share=55% · graphql 4300/5000 reserve=2250 allowance=4/9".into(),
            ));
        let out = render(&mut comp, 100, 16);
        assert!(out.contains("GitHub budget governor"), "{out}");
        assert!(out.contains("share=55%"), "{out}");
        assert!(
            out.find("GitHub budget governor") < out.find("Last sync per source"),
            "{out}"
        );
    }

    #[test]
    fn narrow_modal_wraps_the_complete_governor_snapshot() {
        let n = now();
        let mut comp = SyncStatus::new(Vec::new(), Vec::new(), n).with_governor(Some(
            "share=55% · graphql 4300/5000 reset=2026-04-01T13:00:00Z reserve=2250 \
             allowance=4/9 · next=global reconcile"
                .into(),
        ));
        let out = render(&mut comp, 50, 16);
        assert!(out.contains("share=55%"), "{out}");
        assert!(out.contains("next=global reconcile"), "{out}");
    }

    #[test]
    fn singular_task_noun() {
        let n = now();
        let mut comp = SyncStatus::new(vec![ok("github", 1, 5, n)], vec![ok("github", 1, 5, n)], n);
        let out = render(&mut comp, 80, 12);
        assert!(out.contains("1 task"), "{out}");
        assert!(!out.contains("1 tasks"), "{out}");
    }

    #[test]
    fn renders_rate_limit_budget_reset_and_per_user_contention_guidance() {
        let n = now();
        let entry = rate_limited(n);
        let mut comp = SyncStatus::new(vec![entry.clone()], vec![entry], n);
        let out = render(&mut comp, 100, 16);

        assert!(
            out.contains("rate-limited · ~7m · 12:07 UTC · 98/5000 left"),
            "{out}"
        );
        assert!(
            out.contains("user quota is shared by lazybox + gh + agents"),
            "{out}"
        );
        assert!(
            out.contains("Separate PATs for the same user do not create a new quota"),
            "{out}"
        );
    }

    #[test]
    fn historical_rate_limit_attempt_shows_its_reset_instead_of_now() {
        let n = now();
        let entry = SyncEntry {
            source: "github".into(),
            at: n - chrono::Duration::minutes(2),
            outcome: SyncOutcome::RateLimited {
                remaining: 98,
                limit: 5000,
                reset_at: n - chrono::Duration::minutes(1),
            },
        };
        let mut comp = SyncStatus::new(vec![entry.clone()], vec![entry], n);
        let out = render(&mut comp, 100, 16);

        assert!(
            out.contains("rate-limited · reset 11:59 UTC · 98/5000 left"),
            "{out}"
        );
        assert!(!out.contains("rate-limited · now"), "{out}");
    }

    #[test]
    fn navigation_scrolls_other_keys_dismiss() {
        let n = now();
        let recent: Vec<SyncEntry> = (0..40).map(|i| ok("github", i, i as i64, n)).collect();
        let mut comp = SyncStatus::new(vec![ok("github", 39, 0, n)], recent, n);
        // Prime body_height via a render.
        let _ = render(&mut comp, 80, 12);

        let down = Event::Keyboard(KeyEvent {
            code: Key::Down,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(comp.on(&down), None);
        assert_eq!(comp.scroll, 1);

        let up = Event::Keyboard(KeyEvent {
            code: Key::Up,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(comp.on(&up), None);
        assert_eq!(comp.scroll, 0);

        let esc = Event::Keyboard(KeyEvent {
            code: Key::Esc,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(comp.on(&esc), Some(Msg::ModalDismissed));
    }
}
