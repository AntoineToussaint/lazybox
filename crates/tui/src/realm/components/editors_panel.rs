//! `EditorsPanel` — the Settings "Editors" surface (#1102).
//!
//! Lists the custom editors configured under `editors:` in
//! `~/.lazybox/config.yaml` with a live "on PATH / not found" badge, and
//! lets the user add / edit / remove one without hand-editing YAML and
//! without a restart — the model re-discovers editors after every write
//! (mirroring the snippet hot-reload path).
//!
//! Presentation-only, like the `Settings` component: rows arrive
//! pre-built (id, display, command, availability) and key presses emit
//! `Msg::Editor{Add,Edit,Remove}` for the model to act on. Built-in
//! GUI editors are auto-detected and stay implicit — only the user's own
//! `editors:` entries are listed here, because those are what's editable.

use crate::realm::{Msg, UserEvent};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tuirealm::state::State;

/// One configured editor, resolved for display.
#[derive(Debug, Clone)]
pub(crate) struct EditorRow {
    pub id: String,
    pub display: String,
    pub command: String,
    /// Whether `command` resolves on PATH right now.
    pub available: bool,
}

/// The editors management modal.
pub(crate) struct EditorsPanel {
    rows: Vec<EditorRow>,
    cursor: usize,
    /// Display names of built-in editors detected on this machine (minus
    /// any a custom entry overrides). Shown as reference so the panel is
    /// never blank for a user who relies only on auto-detection.
    detected_builtins: Vec<String>,
    /// Absolute config path, shown so the user knows where writes land.
    config_path: String,
}

impl EditorsPanel {
    pub(crate) fn new(
        rows: Vec<EditorRow>,
        detected_builtins: Vec<String>,
        config_path: String,
    ) -> Self {
        Self {
            rows,
            cursor: 0,
            detected_builtins,
            config_path,
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        let cur = self.cursor as isize;
        self.cursor = (cur + delta).rem_euclid(len as isize) as usize;
    }

    fn focused_id(&self) -> Option<String> {
        self.rows.get(self.cursor).map(|r| r.id.clone())
    }
}

impl Component for EditorsPanel {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        // Width fits the widest row; height fits every row plus the
        // header/hint chrome, bounded to the viewport.
        let widest = self
            .rows
            .iter()
            .map(|r| {
                crate::util::visual_width(&format!("{}  ·  {}  ·  {}", r.display, r.id, r.command))
                    + 14
            })
            .max()
            .unwrap_or(0)
            .max(crate::util::visual_width(&self.config_path) + 6)
            .max(48);
        let modal_w = widest
            .saturating_add(6)
            .min(usize::from(area.width.saturating_sub(4))) as u16;
        let rows_h = self.rows.len().max(1) as u16;
        let modal_h = rows_h
            .saturating_add(6)
            .max(8)
            .min(area.height.saturating_sub(2));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme.modal_border())
            .title(" Editors ")
            .title_style(theme.modal_title());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);
        if inner.height < 3 {
            return;
        }

        let mut lines: Vec<Line> = Vec::with_capacity(inner.height as usize);
        lines.push(Line::from(Span::styled(
            "Custom editors · built-ins are auto-detected",
            Style::default().fg(theme.text_dim),
        )));
        if !self.detected_builtins.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("Detected: {}", self.detected_builtins.join(", ")),
                Style::default().fg(theme.text_dim),
            )));
        }
        lines.push(Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            Style::default().fg(theme.chrome),
        )));

        if self.rows.is_empty() {
            lines.push(Line::from(Span::styled(
                "No custom editors configured yet.",
                Style::default().fg(theme.text_dim),
            )));
            lines.push(Line::from(Span::styled(
                "Press a to add one.",
                Style::default().fg(theme.text_dim),
            )));
        } else {
            for (i, row) in self.rows.iter().enumerate() {
                let (caret, label_style) = if i == self.cursor {
                    ("▸ ", theme.row_focused())
                } else {
                    ("  ", Style::default().fg(theme.text_strong))
                };
                let (badge, badge_style) = if row.available {
                    ("✓ on PATH", Style::default().fg(theme.success))
                } else {
                    ("✗ not found", Style::default().fg(theme.error))
                };
                lines.push(Line::from(vec![
                    Span::styled(caret, Style::default().fg(theme.accent)),
                    Span::styled(row.display.clone(), label_style),
                    Span::styled(
                        format!("  ·  {}  ·  {}   ", row.id, row.command),
                        Style::default().fg(theme.text_dim),
                    ),
                    Span::styled(badge, badge_style),
                ]));
            }
        }

        // Body (everything but the bottom hint row).
        let body = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height - 1,
        };
        frame.render_widget(Paragraph::new(lines), body);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "a add · e edit · d remove · Esc close",
                theme.hint(),
            ))),
            Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            },
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

impl AppComponent<Msg, UserEvent> for EditorsPanel {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        let Event::Keyboard(key) = ev else {
            return None;
        };
        match key.code {
            Key::Down | Key::Char('j') => {
                self.move_cursor(1);
                None
            }
            Key::Up | Key::Char('k') => {
                self.move_cursor(-1);
                None
            }
            Key::Home | Key::Char('g') => {
                self.cursor = 0;
                None
            }
            Key::End | Key::Char('G') => {
                self.cursor = self.rows.len().saturating_sub(1);
                None
            }
            Key::Char('a') => Some(Msg::EditorAdd),
            Key::Enter | Key::Char('e') => self.focused_id().map(Msg::EditorEdit),
            Key::Char('d') | Key::Char('x') => self.focused_id().map(Msg::EditorRemove),
            Key::Esc | Key::Char('q') => Some(Msg::ModalDismissed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::event::{KeyEvent, KeyModifiers};

    fn rows() -> Vec<EditorRow> {
        vec![
            EditorRow {
                id: "fleet".into(),
                display: "JetBrains Fleet".into(),
                command: "fleet".into(),
                available: true,
            },
            EditorRow {
                id: "my-editor".into(),
                display: "My editor".into(),
                command: "/opt/edit/bin/edit".into(),
                available: false,
            },
        ]
    }

    fn key(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn render(comp: &mut EditorsPanel, w: u16, h: u16) -> String {
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
    fn lists_rows_with_availability_badges() {
        let mut comp =
            EditorsPanel::new(rows(), Vec::new(), "/home/me/.lazybox/config.yaml".into());
        let out = render(&mut comp, 90, 12);
        assert!(out.contains("JetBrains Fleet"), "{out}");
        assert!(out.contains("my-editor"), "{out}");
        assert!(out.contains("on PATH"), "{out}");
        assert!(out.contains("not found"), "{out}");
    }

    #[test]
    fn empty_state_prompts_to_add() {
        let mut comp = EditorsPanel::new(Vec::new(), Vec::new(), "/c.yaml".into());
        let out = render(&mut comp, 60, 10);
        assert!(out.contains("No custom editors"), "{out}");
    }

    #[test]
    fn shows_detected_builtins_as_reference() {
        let mut comp = EditorsPanel::new(
            Vec::new(),
            vec!["Zed".into(), "VS Code".into()],
            "/c.yaml".into(),
        );
        let out = render(&mut comp, 60, 10);
        assert!(out.contains("Detected: Zed, VS Code"), "{out}");
    }

    #[test]
    fn keys_emit_add_edit_remove_for_focused_row() {
        let mut comp = EditorsPanel::new(rows(), Vec::new(), "/c.yaml".into());
        assert_eq!(comp.on(&key(Key::Char('a'))), Some(Msg::EditorAdd));
        assert_eq!(
            comp.on(&key(Key::Enter)),
            Some(Msg::EditorEdit("fleet".into()))
        );
        assert_eq!(comp.on(&key(Key::Char('j'))), None);
        assert_eq!(
            comp.on(&key(Key::Char('d'))),
            Some(Msg::EditorRemove("my-editor".into()))
        );
    }

    #[test]
    fn esc_dismisses() {
        let mut comp = EditorsPanel::new(rows(), Vec::new(), "/c.yaml".into());
        assert_eq!(comp.on(&key(Key::Esc)), Some(Msg::ModalDismissed));
    }

    #[test]
    fn add_works_with_no_rows() {
        let mut comp = EditorsPanel::new(Vec::new(), Vec::new(), "/c.yaml".into());
        assert_eq!(comp.on(&key(Key::Char('a'))), Some(Msg::EditorAdd));
        // Edit / remove are no-ops with nothing focused.
        assert_eq!(comp.on(&key(Key::Enter)), None);
        assert_eq!(comp.on(&key(Key::Char('d'))), None);
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let mut comp = EditorsPanel::new(rows(), vec!["Zed".into()], "/c.yaml".into());
        let out = render(&mut comp, 20, 3);
        assert!(out.lines().count() <= 3);
    }
}
