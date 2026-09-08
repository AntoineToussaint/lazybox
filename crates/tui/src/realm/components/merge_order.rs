//! `MergeOrder` — the epic merge-order readout (`E m`, issue #1524).
//!
//! A read-only, keyboard-navigated list of an epic's PRs in the order
//! they may land — a topological sort of the merge-after graph
//! ([`lazybox_ipc::EpicSnapshot::merge_order`]). Each row carries a
//! status glyph so the held PRs are obvious at a glance:
//!
//! - `✓` merged (the PR has landed),
//! - `▶` mergeable (green, no conflict, free to merge now),
//! - `⏸` held — merge-ready but waiting on a merge-after predecessor
//!   (`held by owner/repo#N`), the merge-on-green hold made visible,
//! - `·` not ready (no live PR / CI pending / still in progress).
//!
//! `↑/↓` (or `j/k`) move the cursor, `Enter` jumps to the highlighted
//! workspace, any other key closes. The rows are resolved to display
//! names and glyphs at mount from a cached snapshot, so the window
//! renders a stable view.

use crate::realm::components::scrollable::{centered_rect, draw_frame, max_scroll};
use crate::realm::{Msg, UserEvent};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyEvent};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;
use tuirealm::state::State;

/// The status glyph a merge-order row carries, derived at mount from
/// the member's derived status plus its outstanding merge-after holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MergeRowKind {
    /// The PR has landed.
    Merged,
    /// Green, no conflict, free to merge now.
    Mergeable,
    /// Merge-ready but held by an unmerged merge-after predecessor.
    Held,
    /// No live PR / CI pending / still in progress.
    NotReady,
}

impl MergeRowKind {
    fn glyph(self) -> &'static str {
        match self {
            MergeRowKind::Merged => "✓",
            MergeRowKind::Mergeable => "▶",
            MergeRowKind::Held => "⏸",
            MergeRowKind::NotReady => "·",
        }
    }

    fn color(self, theme: &crate::theme::Theme) -> Color {
        match self {
            MergeRowKind::Merged => theme.success,
            MergeRowKind::Mergeable => theme.accent,
            MergeRowKind::Held => theme.warn,
            MergeRowKind::NotReady => theme.text_dim,
        }
    }
}

/// One pre-resolved merge-order row.
#[derive(Debug, Clone)]
pub(crate) struct MergeOrderRow {
    pub kind: MergeRowKind,
    /// Display label for the PR, e.g. `owner/repo#12  Fix parser`.
    pub label: String,
    /// Display names of the predecessors holding this PR (empty unless
    /// [`MergeRowKind::Held`]).
    pub held_by: Vec<String>,
    /// Session key jumped to on `Enter`.
    pub key: lazybox_core::SessionKey,
}

/// The epic merge-order readout modal.
pub(crate) struct MergeOrder {
    /// Epic display name, for the frame title.
    epic_name: String,
    /// Rows in topological merge order.
    rows: Vec<MergeOrderRow>,
    /// Cursor into `rows`; `None` when empty.
    cursor: Option<usize>,
    /// Topmost visible row, kept so the cursor stays on screen.
    scroll: usize,
    /// Body viewport height, cached in `view` for scroll math.
    body_height: usize,
}

impl MergeOrder {
    pub(crate) fn new(epic_name: impl Into<String>, rows: Vec<MergeOrderRow>) -> Self {
        let cursor = (!rows.is_empty()).then_some(0);
        Self {
            epic_name: epic_name.into(),
            rows,
            cursor,
            scroll: 0,
            body_height: 0,
        }
    }

    /// Move the cursor by `delta`, clamped to the row range, and keep
    /// the scroll window following it.
    fn move_cursor(&mut self, delta: isize) {
        let Some(cur) = self.cursor else {
            return;
        };
        let last = self.rows.len().saturating_sub(1);
        let next = (cur as isize + delta).clamp(0, last as isize) as usize;
        self.cursor = Some(next);
        self.follow_cursor();
    }

    /// Clamp `scroll` so the cursor row is inside the viewport.
    fn follow_cursor(&mut self) {
        let Some(cur) = self.cursor else {
            return;
        };
        if self.body_height == 0 {
            return;
        }
        if cur < self.scroll {
            self.scroll = cur;
        } else if cur >= self.scroll + self.body_height {
            self.scroll = cur + 1 - self.body_height;
        }
    }

    /// One rendered row: `1. ✓ owner/repo#12  Fix parser` (held rows
    /// append `held by …`). `selected` paints the cursor band.
    fn row_line(
        &self,
        n: usize,
        row: &MergeOrderRow,
        selected: bool,
        theme: &crate::theme::Theme,
    ) -> Line<'static> {
        let base = if selected {
            Style::default()
                .fg(theme.text_strong)
                .bg(theme.fill)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_strong)
        };
        let dim = if selected {
            base.fg(theme.text_dim)
        } else {
            Style::default().fg(theme.text_dim)
        };
        let caret = if selected { "▸ " } else { "  " };
        let mut spans = vec![
            Span::styled(caret.to_string(), base),
            Span::styled(format!("{n:>2}. "), dim),
            Span::styled(
                format!("{} ", row.kind.glyph()),
                base.fg(row.kind.color(theme)),
            ),
            Span::styled(row.label.clone(), base),
        ];
        if !row.held_by.is_empty() {
            spans.push(Span::styled(
                format!("  held by {}", row.held_by.join(", ")),
                base.fg(theme.warn),
            ));
        }
        Line::from(spans)
    }
}

impl Component for MergeOrder {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 90u16.min(area.width.saturating_sub(4));
        let modal_h = 24u16.min(area.height.saturating_sub(2));
        let modal = centered_rect(area, modal_w, modal_h);
        let title = format!(" Merge order · {} ", self.epic_name);
        let inner = draw_frame(frame, modal, &title, theme);
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
        self.body_height = body_area.height.max(1) as usize;
        self.follow_cursor();

        let lines: Vec<Line<'static>> = if self.rows.is_empty() {
            vec![Line::from(Span::styled(
                "  No PRs in this epic yet.",
                Style::default().fg(theme.text_dim),
            ))]
        } else {
            let max = max_scroll(self.rows.len(), self.body_height as u16) as usize;
            if self.scroll > max {
                self.scroll = max;
            }
            self.rows
                .iter()
                .enumerate()
                .skip(self.scroll)
                .take(self.body_height)
                .map(|(i, row)| self.row_line(i + 1, row, Some(i) == self.cursor, theme))
                .collect()
        };
        frame.render_widget(Paragraph::new(lines), body_area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "↑/↓ navigate · Enter jump · any other key to close",
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

impl AppComponent<Msg, UserEvent> for MergeOrder {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        let Event::Keyboard(key) = ev else {
            return None;
        };
        match key {
            KeyEvent {
                code: Key::Down | Key::Char('j'),
                ..
            } => {
                self.move_cursor(1);
                None
            }
            KeyEvent {
                code: Key::Up | Key::Char('k'),
                ..
            } => {
                self.move_cursor(-1);
                None
            }
            KeyEvent {
                code: Key::Enter, ..
            } => {
                let key = self.cursor.and_then(|c| self.rows.get(c)).map(|r| r.key.clone());
                // Enter with no rows is a no-op close.
                match key {
                    Some(key) => Some(Msg::EpicJumpToWorkspace(key)),
                    None => Some(Msg::ModalDismissed),
                }
            }
            // Ctrl-C and any other key close.
            _ => Some(Msg::ModalDismissed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::event::KeyModifiers;

    fn key_at(i: usize) -> lazybox_core::SessionKey {
        lazybox_core::SessionKey::new(format!("ws-{i}"))
    }

    fn ke(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn rows() -> Vec<MergeOrderRow> {
        vec![
            MergeOrderRow {
                kind: MergeRowKind::Merged,
                label: "owner/repo#10  Core types".to_string(),
                held_by: vec![],
                key: key_at(0),
            },
            MergeOrderRow {
                kind: MergeRowKind::Mergeable,
                label: "owner/repo#11  IPC wire".to_string(),
                held_by: vec![],
                key: key_at(1),
            },
            MergeOrderRow {
                kind: MergeRowKind::Held,
                label: "owner/repo#12  TUI readout".to_string(),
                held_by: vec!["owner/repo#11".to_string()],
                key: key_at(2),
            },
        ]
    }

    fn render(comp: &mut MergeOrder, w: u16, h: u16) -> String {
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
    fn merge_order_modal_lists_prs_in_order_with_hold_reason() {
        let mut comp = MergeOrder::new("Orchestration", rows());
        let out = render(&mut comp, 90, 16);
        // Frame + epic name.
        assert!(out.contains("Merge order · Orchestration"), "{out}");
        // Topological numbering, in order.
        assert!(out.contains("1."), "{out}");
        assert!(out.contains("2."), "{out}");
        assert!(out.contains("3."), "{out}");
        // The three PRs are listed in merge order.
        let p10 = out.find("owner/repo#10").expect("row 10");
        let p11 = out.find("owner/repo#11").expect("row 11");
        let p12 = out.find("owner/repo#12").expect("row 12");
        assert!(p10 < p11 && p11 < p12, "rows in topological order: {out}");
        // Glyphs.
        assert!(out.contains('✓'), "merged glyph: {out}");
        assert!(out.contains('▶'), "mergeable glyph: {out}");
        assert!(out.contains('⏸'), "held glyph: {out}");
        // The hold reason names the predecessor.
        assert!(out.contains("held by owner/repo#11"), "{out}");
    }

    #[test]
    fn enter_jumps_to_the_cursor_workspace() {
        let mut comp = MergeOrder::new("Epic", rows());
        // Cursor starts on row 0.
        assert_eq!(comp.on(&ke(Key::Down)), None);
        assert_eq!(comp.on(&ke(Key::Down)), None);
        // Now on row 2 (the held PR).
        assert_eq!(
            comp.on(&ke(Key::Enter)),
            Some(Msg::EpicJumpToWorkspace(key_at(2)))
        );
    }

    #[test]
    fn cursor_clamps_at_the_ends() {
        let mut comp = MergeOrder::new("Epic", rows());
        // Up from the top stays at 0.
        assert_eq!(comp.on(&ke(Key::Up)), None);
        assert_eq!(comp.cursor, Some(0));
        // Down past the end stops at the last row.
        for _ in 0..10 {
            assert_eq!(comp.on(&ke(Key::Down)), None);
        }
        assert_eq!(comp.cursor, Some(2));
    }

    #[test]
    fn esc_closes_and_empty_is_safe() {
        let mut comp = MergeOrder::new("Epic", rows());
        assert_eq!(comp.on(&ke(Key::Esc)), Some(Msg::ModalDismissed));

        let mut empty = MergeOrder::new("Epic", vec![]);
        assert_eq!(empty.cursor, None);
        // Enter on an empty list closes rather than jumping.
        assert_eq!(empty.on(&ke(Key::Enter)), Some(Msg::ModalDismissed));
        let out = render(&mut empty, 80, 12);
        assert!(out.contains("No PRs in this epic yet"), "{out}");
    }
}
