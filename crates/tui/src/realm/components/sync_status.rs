//! `SyncStatus` — the debug / sync-status window (default `Shift-D`).
//!
//! Surfaces what provider polling is actually doing: the last sync
//! time and outcome per source, plus a scrollable log of recent sync
//! attempts with error detail. Without it a failing poll (rate limit,
//! auth, network, GH API error) leaves the inbox silently stale with
//! no way to tell from the UI.
//!
//! Read-only: navigation keys scroll the log; any other key dismisses.
//! Built from a snapshot of the [`SyncLog`](crate::realm::status_ctx)
//! at mount, with `now` captured once so the relative ages don't drift
//! while the window is open.

use crate::realm::status_ctx::{SyncEntry, SyncOutcome};
use crate::realm::{Msg, UserEvent};
use chrono::{DateTime, Utc};
use pilot_core::time::time_ago_at;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tuirealm::state::State;

/// Debug / sync-status window.
pub(crate) struct SyncStatus {
    /// Latest attempt per source, sorted by source name.
    summary: Vec<SyncEntry>,
    /// Every retained attempt, most-recent-first.
    recent: Vec<SyncEntry>,
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
            now,
            scroll: 0,
            body_height: 0,
        }
    }

    /// The scrollable body, as styled lines. Re-derived each render so
    /// theme + width changes are picked up.
    fn body_lines(&self, theme: &crate::theme::Theme) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();

        if self.summary.is_empty() {
            lines.push(Line::from(Span::styled(
                "No sync activity yet — pilot hasn't completed a poll.",
                Style::default().fg(theme.text_dim),
            )));
            return lines;
        }

        lines.push(Line::from(Span::styled(
            "Last sync per source",
            theme.section_heading(),
        )));
        for e in &self.summary {
            lines.push(self.summary_line(e, theme));
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
            outcome_span(e, theme),
            Span::styled(format!("  ·  {ago}"), Style::default().fg(theme.text_dim)),
        ])
    }

    /// One log row: `✓ github · 12 tasks · 2m ago`.
    fn entry_line(&self, e: &SyncEntry, theme: &crate::theme::Theme) -> Line<'static> {
        let (glyph, glyph_color) = if e.is_ok() {
            ("✓ ", theme.success)
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
            outcome_span(e, theme),
            Span::styled(format!("  ·  {ago}"), Style::default().fg(theme.text_dim)),
        ])
    }

    fn max_scroll(&self, total_lines: u16) -> u16 {
        total_lines.saturating_sub(self.body_height.max(1))
    }
}

/// The human-readable outcome fragment shared by the summary and log
/// rows — `12 tasks` on success, `auth: bad credentials` on failure.
fn outcome_span(e: &SyncEntry, theme: &crate::theme::Theme) -> Span<'static> {
    match &e.outcome {
        SyncOutcome::Ok { count } => {
            let noun = if *count == 1 { "task" } else { "tasks" };
            Span::styled(
                format!("{count} {noun}"),
                Style::default().fg(theme.text_dim),
            )
        }
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
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme.modal_border())
            .title(" Sync status ")
            .title_style(theme.modal_title());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);
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

        let lines = self.body_lines(theme);
        // Clamp scroll so a short log can't leave blank rows scrolled
        // off the top.
        let max = self.max_scroll(lines.len() as u16);
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
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.body_height.max(1);
        match key.code {
            Key::Down | Key::Char('j') => {
                self.scroll = self.scroll.saturating_add(1);
                None
            }
            Key::Up | Key::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                None
            }
            Key::PageDown => {
                self.scroll = self.scroll.saturating_add(page);
                None
            }
            Key::PageUp => {
                self.scroll = self.scroll.saturating_sub(page);
                None
            }
            Key::Char('d') if ctrl => {
                self.scroll = self.scroll.saturating_add((page / 2).max(1));
                None
            }
            Key::Char('u') if ctrl => {
                self.scroll = self.scroll.saturating_sub((page / 2).max(1));
                None
            }
            Key::Home | Key::Char('g') => {
                self.scroll = 0;
                None
            }
            // Any other key (Esc, q, Enter, …) closes the window.
            _ => Some(Msg::ModalDismissed),
        }
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

    #[test]
    fn singular_task_noun() {
        let n = now();
        let mut comp = SyncStatus::new(vec![ok("github", 1, 5, n)], vec![ok("github", 1, 5, n)], n);
        let out = render(&mut comp, 80, 12);
        assert!(out.contains("1 task"), "{out}");
        assert!(!out.contains("1 tasks"), "{out}");
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
