//! `Messages` — the notices log window (default `Shift-M`, #309).
//!
//! The durable half of the footer's transient surface. Every non-hint
//! notice that flashes in the footer also accumulates in
//! `StatusCtx::messages`; this window renders that bounded ring so an
//! error that flashed and faded — or one the user simply missed — is
//! still readable after the fact, severity-colored and time-stamped.
//!
//! Read-only except for `c`, which clears the log; navigation keys
//! scroll; any other key dismisses. Built from a snapshot taken at
//! mount, with `now` captured once so relative ages don't drift while
//! the window is open.

use crate::realm::components::footer::NoticeSeverity;
use crate::realm::components::scrollable::{
    centered_rect, draw_frame, handle_scroll_key, max_scroll,
};
use crate::realm::status_ctx::MessageEntry;
use crate::realm::{Msg, UserEvent};
use chrono::{DateTime, Utc};
use lazybox_core::time::time_ago_at;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
#[cfg(test)]
use tuirealm::event::KeyModifiers;
use tuirealm::event::{Event, Key};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;
use tuirealm::state::State;

/// Notices-log window.
pub(crate) struct Messages {
    /// Recorded notices, most-recent-first.
    entries: Vec<MessageEntry>,
    /// Reference instant for relative-time rendering, captured at
    /// mount so ages stay stable while the window is open.
    now: DateTime<Utc>,
    /// Topmost visible body line.
    scroll: u16,
    /// Body viewport height, cached in `view` for page jumps.
    body_height: u16,
}

impl Messages {
    /// Build from a `MessageLog` snapshot. `entries` are cloned out so
    /// the window renders a stable view.
    pub(crate) fn new(entries: Vec<MessageEntry>, now: DateTime<Utc>) -> Self {
        Self {
            entries,
            now,
            scroll: 0,
            body_height: 0,
        }
    }

    /// The scrollable body, as styled lines. Re-derived each render so
    /// theme + width changes are picked up.
    fn body_lines(&self, theme: &crate::theme::Theme) -> Vec<Line<'static>> {
        if self.entries.is_empty() {
            return vec![Line::from(Span::styled(
                "No messages yet — notices you see in the footer collect here.",
                Style::default().fg(theme.text_dim),
            ))];
        }
        self.entries
            .iter()
            .map(|e| self.entry_line(e, theme))
            .collect()
    }

    /// One log row: `✗ merge rejected: … · 2m ago`, severity-colored.
    fn entry_line(&self, e: &MessageEntry, theme: &crate::theme::Theme) -> Line<'static> {
        let (glyph, color) = match e.severity {
            NoticeSeverity::Permanent => ("✗ ", theme.error),
            NoticeSeverity::Retryable | NoticeSeverity::Auth => ("⚠ ", theme.warn),
            NoticeSeverity::Info | NoticeSeverity::Hint => ("· ", theme.text_dim),
        };
        let ago = time_ago_at(&e.at, self.now);
        let mut spans = vec![
            Span::styled(glyph, Style::default().fg(color)),
            Span::styled(e.message.clone(), Style::default().fg(theme.text_strong)),
        ];
        // Collapsed re-fires carry their count — one honest `×12` row
        // instead of twelve identical ones drowning the log (#1245).
        if e.count > 1 {
            spans.push(Span::styled(
                format!(" ×{}", e.count),
                Style::default().fg(color),
            ));
        }
        spans.push(Span::styled(
            format!("  ·  {ago}"),
            Style::default().fg(theme.text_dim),
        ));
        Line::from(spans)
    }
}

impl Component for Messages {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 100u16.min(area.width.saturating_sub(4));
        let modal_h = 24u16.min(area.height.saturating_sub(2));
        let modal = centered_rect(area, modal_w, modal_h);
        let inner = draw_frame(frame, modal, " Messages ", theme);
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
        let max = max_scroll(lines.len(), self.body_height);
        if self.scroll > max {
            self.scroll = max;
        }
        frame.render_widget(Paragraph::new(lines).scroll((self.scroll, 0)), body_area);
        let hint = if self.entries.is_empty() {
            "any key to close"
        } else {
            "↑/↓ scroll · c clear · any other key to close"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, theme.hint()))),
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

impl AppComponent<Msg, UserEvent> for Messages {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        let Event::Keyboard(key) = ev else {
            return None;
        };
        if handle_scroll_key(&mut self.scroll, self.body_height, key) {
            return None;
        }
        match key.code {
            // Clear the whole log — the messages window is the one
            // place the durable history can be wiped. The model clears
            // `status.messages` and re-mounts this window empty.
            Key::Char('c') if !self.entries.is_empty() => Some(Msg::MessagesCleared),
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

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap()
    }

    fn entry(message: &str, severity: NoticeSeverity, secs_ago: i64) -> MessageEntry {
        MessageEntry {
            message: message.into(),
            severity,
            at: now() - chrono::Duration::seconds(secs_ago),
            count: 1,
        }
    }

    fn render(comp: &mut Messages, w: u16, h: u16) -> String {
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

    #[test]
    fn empty_log_renders_placeholder() {
        let mut comp = Messages::new(vec![], now());
        let out = render(&mut comp, 80, 12);
        assert!(out.contains("No messages yet"), "{out}");
        assert!(out.contains("Messages"), "{out}");
        // No clear hint when there's nothing to clear.
        assert!(!out.contains("c clear"), "{out}");
    }

    #[test]
    fn renders_entries_with_severity_and_age() {
        let entries = vec![
            entry(
                "merge rejected: base out of date",
                NoticeSeverity::Permanent,
                30,
            ),
            entry("auto-merging repo — CI green", NoticeSeverity::Info, 90),
        ];
        let mut comp = Messages::new(entries, now());
        let out = render(&mut comp, 90, 20);
        assert!(out.contains("merge rejected: base out of date"), "{out}");
        assert!(out.contains("auto-merging repo"), "{out}");
        assert!(out.contains("c clear"), "{out}");
    }

    #[test]
    fn navigation_scrolls_c_clears_other_keys_dismiss() {
        let entries: Vec<MessageEntry> = (0..40)
            .map(|i| entry(&format!("notice {i}"), NoticeSeverity::Info, i))
            .collect();
        let mut comp = Messages::new(entries, now());
        let _ = render(&mut comp, 80, 12);

        let down = Event::Keyboard(KeyEvent {
            code: Key::Down,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(comp.on(&down), None);
        assert_eq!(comp.scroll, 1);

        let clear = Event::Keyboard(KeyEvent {
            code: Key::Char('c'),
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(comp.on(&clear), Some(Msg::MessagesCleared));

        let esc = Event::Keyboard(KeyEvent {
            code: Key::Esc,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(comp.on(&esc), Some(Msg::ModalDismissed));
    }

    #[test]
    fn c_on_empty_log_dismisses_instead_of_clearing() {
        let mut comp = Messages::new(vec![], now());
        let _ = render(&mut comp, 80, 12);
        let clear = Event::Keyboard(KeyEvent {
            code: Key::Char('c'),
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(comp.on(&clear), Some(Msg::ModalDismissed));
    }
}
