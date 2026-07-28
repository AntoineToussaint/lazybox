//! `MarkdownModal` — the scrollable full-description reader (#448).
//!
//! The inline `TaskBodyView` shows a first-few-lines teaser; this modal
//! is the "read the whole thing" surface. It renders the raw markdown
//! body through [`markdown_doc`](crate::components::markdown_doc) — real
//! headings, lists, code, blockquotes, links, tables — into a generous,
//! scrollable overlay mounted on `Model::modal_stack`.
//!
//! Scroll: `j`/`k`, arrows, `PageUp`/`PageDown` (and their `Shift-`
//! variants), `Home`/`End`, `g`/`G`, `Ctrl-d`/`Ctrl-u`, and the mouse
//! wheel. Links are click-mapped — a left-click on a link span opens it
//! in the browser (`Msg::OpenUrl`); a click outside the modal, or `Esc`
//! / `q`, dismisses. Reusable for any long body (issue #448 asks for
//! comments/activity too), so it takes a title + raw markdown, nothing
//! task-specific.

use crate::components::markdown_doc::{RenderedDoc, render_markdown};
use crate::components::scrollbar;
use crate::realm::components::scrollable::{
    centered_rect, draw_frame, handle_scroll_key, max_scroll,
};
use crate::realm::{Msg, UserEvent};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
#[cfg(test)]
use tuirealm::event::KeyModifiers;
use tuirealm::event::{Event, Key, MouseEventKind};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;
use tuirealm::state::State;

/// Wheel notches per scroll event — matches the pane scroll feel.
const WHEEL_STEP: u16 = 3;

pub(crate) struct MarkdownModal {
    title: String,
    source: String,
    scroll: u16,
    /// Body viewport height, cached in `view` for page jumps.
    body_height: u16,
    /// Text area from the last render, for mouse hit-testing.
    body_area: Rect,
    /// Whole modal rect from the last render, for outside-click dismiss.
    modal_rect: Rect,
    /// The doc rendered at `rendered_width`, kept so a click can map a
    /// `(row, col)` back to a link.
    rendered: RenderedDoc,
    rendered_width: u16,
}

impl MarkdownModal {
    pub(crate) fn new(title: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            source: source.into(),
            scroll: 0,
            body_height: 0,
            body_area: Rect::default(),
            modal_rect: Rect::default(),
            rendered: RenderedDoc::default(),
            rendered_width: 0,
        }
    }

    /// Resolve a click to a link URL, mapping the on-screen `(col, row)`
    /// through the current scroll offset into the rendered doc.
    fn link_at_click(&self, col: u16, row: u16) -> Option<&str> {
        if row < self.body_area.y || col < self.body_area.x {
            return None;
        }
        if row >= self.body_area.y + self.body_area.height
            || col >= self.body_area.x + self.body_area.width
        {
            return None;
        }
        let line = (row - self.body_area.y) as usize + self.scroll as usize;
        let local_col = col - self.body_area.x;
        self.rendered.link_at(line, local_col)
    }

    fn contains(&self, col: u16, row: u16) -> bool {
        let r = self.modal_rect;
        col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
    }
}

impl Component for MarkdownModal {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        // Generous: most of the pane, with a small margin — but never
        // taller/wider than the pane itself (tiny terminals).
        let modal_w = area.width.saturating_sub(6).clamp(20, 120).min(area.width);
        let modal_h = if area.height <= 6 {
            area.height
        } else {
            area.height.saturating_sub(4).max(6)
        };
        let modal = centered_rect(area, modal_w, modal_h);
        self.modal_rect = modal;

        let title = format!(" {} ", trim_title(&self.title, modal_w.saturating_sub(4)));
        let inner = draw_frame(frame, modal, &title, theme);
        if inner.height < 2 || inner.width < 2 {
            return;
        }

        // Reserve the bottom row for the hint line, and a right column
        // for the scrollbar gutter.
        let body_h = inner.height - 1;
        let gutter = 1u16;
        let text_w = inner.width.saturating_sub(gutter);
        let body_area = Rect {
            x: inner.x,
            y: inner.y,
            width: text_w,
            height: body_h,
        };
        let gutter_area = Rect {
            x: inner.x + text_w,
            y: inner.y,
            width: gutter,
            height: body_h,
        };
        let hint_area = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        self.body_area = body_area;
        self.body_height = body_area.height.max(1);

        // Re-render markdown when the content width changed (theme is a
        // process global read at render time, so a mid-open theme switch
        // is picked up on the next width-changing frame — acceptable for
        // a reading overlay).
        if self.rendered_width != text_w || self.rendered.lines.is_empty() {
            self.rendered = render_markdown(&self.source, text_w, theme);
            self.rendered_width = text_w;
        }

        let total = self.rendered.lines.len();
        let max = max_scroll(total, self.body_height);
        if self.scroll > max {
            self.scroll = max;
        }
        frame.render_widget(
            Paragraph::new(self.rendered.lines.clone()).scroll((self.scroll, 0)),
            body_area,
        );
        scrollbar::render_vertical(
            frame,
            gutter_area,
            total,
            self.body_height as usize,
            self.scroll as usize,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "↑/↓ · PgUp/PgDn scroll · click links · Esc close",
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

impl AppComponent<Msg, UserEvent> for MarkdownModal {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(key) => {
                if handle_scroll_key(&mut self.scroll, self.body_height, key) {
                    return None;
                }
                match key.code {
                    // Clamp-to-bottom happens in `view` via `max_scroll`.
                    Key::End | Key::Char('G') => {
                        self.scroll = u16::MAX;
                        None
                    }
                    Key::Esc | Key::Char('q') | Key::Enter => Some(Msg::ModalDismissed),
                    // Reading surface: unknown keys are inert, never an
                    // accidental dismiss.
                    _ => None,
                }
            }
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollDown => {
                    self.scroll = self.scroll.saturating_add(WHEEL_STEP);
                    None
                }
                MouseEventKind::ScrollUp => {
                    self.scroll = self.scroll.saturating_sub(WHEEL_STEP);
                    None
                }
                MouseEventKind::Down(_) => {
                    if let Some(url) = self.link_at_click(m.column, m.row) {
                        Some(Msg::OpenUrl(url.to_string()))
                    } else if !self.contains(m.column, m.row) {
                        Some(Msg::ModalDismissed)
                    } else {
                        None
                    }
                }
                _ => None,
            },
            _ => None,
        }
    }
}

/// Clip a modal title to `max` display columns with an ellipsis.
fn trim_title(title: &str, max: u16) -> String {
    crate::util::truncate_ellipsis(title, max as usize).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::event::{KeyEvent, MouseButton, MouseEvent};

    fn render(comp: &mut MarkdownModal, w: u16, h: u16) -> String {
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

    fn key(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn renders_title_and_headings() {
        let mut comp = MarkdownModal::new("owner/repo#12", "# Title\n\nHello **world**.");
        let out = render(&mut comp, 60, 20);
        assert!(out.contains("owner/repo#12"), "{out}");
        assert!(out.contains("Title"), "{out}");
        assert!(out.contains("Hello"), "{out}");
        assert!(out.contains("world"), "{out}");
    }

    #[test]
    fn esc_and_q_dismiss_arrows_scroll() {
        let body = (0..80)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut comp = MarkdownModal::new("t", body);
        let _ = render(&mut comp, 40, 12);
        assert_eq!(comp.on(&key(Key::Down)), None);
        assert_eq!(comp.scroll, 1);
        assert_eq!(comp.on(&key(Key::Up)), None);
        assert_eq!(comp.scroll, 0);
        // Unknown key is inert (not a dismiss).
        assert_eq!(comp.on(&key(Key::Char('x'))), None);
        assert_eq!(comp.on(&key(Key::Esc)), Some(Msg::ModalDismissed));
        assert_eq!(comp.on(&key(Key::Char('q'))), Some(Msg::ModalDismissed));
    }

    #[test]
    fn wheel_scrolls() {
        let body = (0..80)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut comp = MarkdownModal::new("t", body);
        let _ = render(&mut comp, 40, 12);
        let down = Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            modifiers: KeyModifiers::NONE,
            column: 5,
            row: 5,
        });
        assert_eq!(comp.on(&down), None);
        assert_eq!(comp.scroll, WHEEL_STEP);
    }

    #[test]
    fn click_on_link_opens_url() {
        let mut comp = MarkdownModal::new("t", "See [the docs](https://example.com/x) now.");
        let _ = render(&mut comp, 60, 12);
        // The link text "the docs" sits after "See " on the first body
        // row; find its column from the recorded hit.
        let hit = comp.rendered.links.first().cloned().expect("a link");
        let click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            modifiers: KeyModifiers::NONE,
            column: comp.body_area.x + hit.start,
            row: comp.body_area.y + hit.line as u16,
        });
        assert_eq!(
            comp.on(&click),
            Some(Msg::OpenUrl("https://example.com/x".to_string()))
        );
    }

    #[test]
    fn tiny_terminal_stays_within_bounds() {
        // A pane shorter than the modal's minimum must not paint outside
        // the screen (the modal height is capped to the area).
        let mut comp = MarkdownModal::new("t", "one\n\ntwo\n\nthree\n\nfour");
        let out = render(&mut comp, 30, 4);
        assert!(out.lines().count() <= 4, "modal must fit the 4-row pane");
        assert!(comp.modal_rect.height <= 4);
    }

    #[test]
    fn title_trimming_uses_terminal_cells() {
        let title = trim_title("界界界", 4);
        assert_eq!(title, "界…");
        assert!(crate::util::visual_width(&title) <= 4);
        assert_eq!(trim_title("👩‍💻abc", 4), "👩‍💻a…");
        assert_eq!(trim_title("title", 0), "");
    }

    #[test]
    fn click_outside_dismisses() {
        let mut comp = MarkdownModal::new("t", "short body");
        let _ = render(&mut comp, 60, 20);
        let click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            modifiers: KeyModifiers::NONE,
            column: 0,
            row: 0,
        });
        assert_eq!(comp.on(&click), Some(Msg::ModalDismissed));
    }
}
