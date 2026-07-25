//! `SnippetBrowser` — read-only catalog of the snippet library (#237).
//!
//! Snippets were undiscoverable: the only ways in were typing
//! `]]s<key>` (you had to already know the key) and the terminal-leader
//! popup, which never functions as a browsable list. This modal lists
//! every merged snippet — key, origin, description, and the full body —
//! so a user can see what's available and what each one expands to.
//! Reachable from any pane via `]`, the `,` Settings palette, and listed
//! in Ask Lazybox's shortcut index.
//!
//! Deliberately read-only: editing snippets stays "edit the YAML file"
//! by design (see `docs/snippets.md`). `e` is the bridge to that —
//! it closes the browser and opens `~/.lazybox/snippets.yaml` in the
//! configured editor (`Msg::OpenSnippetsFile`). Navigation keys scroll;
//! any other key dismisses.

use crate::components::comment_render::wrap_one;
use crate::realm::components::scrollable::{
    centered_rect, draw_frame, handle_scroll_key, max_scroll,
};
use crate::realm::{Msg, UserEvent};
use lazybox_config::{Snippet, SnippetOrigin};
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

/// One browser row — a snippet rendered in full. Unlike the picker's
/// `PickerRow` (which keeps only a one-line body preview), the browser
/// shows the whole body, so it owns the full text. Fields are private:
/// the only consumer is this module's renderer, and `new` is the sole
/// way the model builds one.
#[derive(Debug)]
pub struct BrowserRow {
    key: String,
    description: String,
    body: String,
    origin: SnippetOrigin,
}

impl BrowserRow {
    pub fn new(key: &str, snippet: &Snippet) -> Self {
        Self {
            key: key.to_string(),
            description: snippet.description.clone(),
            body: snippet.body.clone(),
            origin: snippet.origin,
        }
    }
}

/// Read-only snippets browser modal.
pub struct SnippetBrowser {
    /// Rows in key order (caller passes `Snippets::all`, a BTreeMap walk).
    rows: Vec<BrowserRow>,
    /// Topmost visible body line.
    scroll: u16,
    /// Body viewport height, cached in `view` for page jumps + clamping.
    body_height: u16,
    /// Live terminal leader character, so examples follow a remap.
    escape_char: char,
}

impl SnippetBrowser {
    pub fn new(rows: Vec<BrowserRow>, escape_char: char) -> Self {
        Self {
            rows,
            scroll: 0,
            body_height: 0,
            escape_char,
        }
    }

    /// The scrollable body, wrapped to `width` cells. Pre-wrapped (not
    /// `Paragraph::wrap`) so the scroll offset — which ratatui counts in
    /// pre-wrap lines — stays in step with what's drawn. Re-derived each
    /// render so theme + width changes are picked up. Pure given
    /// `(theme, width)`, so tests can assert wrapping without a frame.
    fn body_lines(&self, theme: &crate::theme::Theme, width: u16) -> Vec<Line<'static>> {
        let dim = Style::default().fg(theme.text_dim);
        if self.rows.is_empty() {
            let line = Line::from(Span::styled(
                "No snippets configured — add some to ~/.lazybox/snippets.yaml.",
                dim,
            ));
            return wrap_one(line, width);
        }
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (i, r) in self.rows.iter().enumerate() {
            if i > 0 {
                lines.push(Line::raw(""));
            }
            // Heading: `]]s<key>   description   [origin]`.
            let mut head: Vec<Span<'static>> = vec![Span::styled(
                format!("{0}{0}s{1}", self.escape_char, r.key,),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )];
            if !r.description.is_empty() {
                head.push(Span::raw("  "));
                head.push(Span::styled(
                    r.description.clone(),
                    Style::default().fg(theme.text_strong),
                ));
            }
            let origin = r.origin.label();
            if !origin.is_empty() {
                head.push(Span::raw("  "));
                head.push(Span::styled(
                    format!("[{origin}]"),
                    Style::default().fg(theme.text_dim).italic(),
                ));
            }
            lines.extend(wrap_one(Line::from(head), width));
            // Body, indented two cells so it reads as one block under
            // its key; long lines wrap (continuations re-indented) and
            // embedded newlines are preserved.
            for b in r.body.lines() {
                let body_line = Line::from(Span::styled(b.to_string(), dim));
                lines.extend(indent_wrapped(body_line, width, dim));
            }
        }
        lines
    }
}

/// Wrap `line` to fit `width` with a hanging two-cell indent: every
/// produced row (the first and any wrap continuations) is prefixed so
/// the whole body block sits under its heading.
fn indent_wrapped(line: Line<'static>, width: u16, style: Style) -> Vec<Line<'static>> {
    const INDENT: &str = "  ";
    let inner = width.saturating_sub(INDENT.len() as u16).max(1);
    wrap_one(line, inner)
        .into_iter()
        .map(|l| {
            let mut spans = Vec::with_capacity(l.spans.len() + 1);
            spans.push(Span::styled(INDENT, style));
            spans.extend(l.spans);
            Line::from(spans)
        })
        .collect()
}

impl Component for SnippetBrowser {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 90u16.min(area.width.saturating_sub(4));
        let modal_h = 24u16.min(area.height.saturating_sub(2));
        let modal = centered_rect(area, modal_w, modal_h);
        let inner = draw_frame(frame, modal, " Snippets ", theme);
        if inner.height < 2 {
            return;
        }

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

        let lines = self.body_lines(theme, body_area.width);
        let max = max_scroll(lines.len(), self.body_height);
        if self.scroll > max {
            self.scroll = max;
        }
        frame.render_widget(Paragraph::new(lines).scroll((self.scroll, 0)), body_area);

        // Hint reflects whether there's more below, so the user knows to
        // scroll instead of assuming the list ends at the fold.
        let hint = if self.scroll < max {
            "↑/↓ scroll (more below) · e edit YAML · any other key to close"
        } else {
            "↑/↓ scroll · e edit YAML · any other key to close"
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

impl AppComponent<Msg, UserEvent> for SnippetBrowser {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        let Event::Keyboard(key) = ev else {
            return None;
        };
        if handle_scroll_key(&mut self.scroll, self.body_height, key) {
            return None;
        }
        match key.code {
            // `e` hands off to the editor on the YAML file.
            Key::Char('e') => Some(Msg::OpenSnippetsFile),
            // Any other key (Esc, q, Enter, …) closes the browser.
            _ => Some(Msg::ModalDismissed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::event::KeyEvent;

    fn rows() -> Vec<BrowserRow> {
        vec![
            BrowserRow::new(
                "pr",
                &Snippet {
                    description: "Open a PR".into(),
                    category: "Git & PR".into(),
                    body: "Please open a PR for the current branch.".into(),
                    origin: SnippetOrigin::BuiltIn,
                },
            ),
            BrowserRow::new(
                "rev",
                &Snippet {
                    description: "Review diff".into(),
                    category: "Review".into(),
                    body: "Review the current diff\nfor correctness bugs.".into(),
                    origin: SnippetOrigin::Global,
                },
            ),
        ]
    }

    fn render(comp: &mut SnippetBrowser, w: u16, h: u16) -> String {
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

    fn keyed(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn lists_keys_descriptions_bodies_and_origins() {
        let mut comp = SnippetBrowser::new(rows(), ']');
        let out = render(&mut comp, 90, 20);
        assert!(out.contains("Snippets"), "missing title: {out}");
        assert!(out.contains("]]spr"), "missing pr key: {out}");
        assert!(out.contains("]]srev"), "missing rev key: {out}");
        assert!(out.contains("Open a PR"), "missing description: {out}");
        assert!(
            out.contains("Please open a PR for the current branch."),
            "missing body: {out}"
        );
        assert!(out.contains("[built-in]"), "missing origin tag: {out}");
        assert!(out.contains("[global]"), "missing origin tag: {out}");
    }

    #[test]
    fn empty_library_renders_placeholder() {
        let mut comp = SnippetBrowser::new(vec![], ']');
        let out = render(&mut comp, 80, 12);
        assert!(out.contains("No snippets configured"), "{out}");
    }

    #[test]
    fn long_body_wraps_and_continuations_stay_indented() {
        let long = "alpha bravo charlie delta echo foxtrot golf hotel india juliet";
        let comp = SnippetBrowser::new(
            vec![BrowserRow::new(
                "rev",
                &Snippet {
                    description: "Review".into(),
                    category: "Review".into(),
                    body: long.into(),
                    origin: SnippetOrigin::Global,
                },
            )],
            ']',
        );
        let theme = crate::theme::current();
        // 30 cells: the heading (`]]srev  Review  [global]`) fits on one
        // line, but the long single-line body is forced to wrap.
        let width = 30u16;
        let lines = comp.body_lines(theme, width);
        let body: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .filter(|s| !s.starts_with("]]srev")) // drop the heading row
            .filter(|s| !s.trim().is_empty())
            .collect();
        assert!(body.len() > 1, "a long body must wrap: {body:?}");
        assert!(
            body.iter().all(|l| l.starts_with("  ")),
            "every wrapped body row keeps the hanging indent: {body:?}",
        );
        // No drawn row exceeds the viewport, so nothing clips off-screen.
        assert!(
            lines.iter().all(|l| l
                .spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>()
                <= width as usize),
            "no line wider than the viewport",
        );
    }

    #[test]
    fn e_opens_the_yaml_file() {
        let mut comp = SnippetBrowser::new(rows(), ']');
        assert_eq!(comp.on(&keyed(Key::Char('e'))), Some(Msg::OpenSnippetsFile));
    }

    #[test]
    fn navigation_scrolls_other_keys_dismiss() {
        let mut comp = SnippetBrowser::new(rows(), ']');
        let _ = render(&mut comp, 80, 12);
        assert_eq!(comp.on(&keyed(Key::Down)), None);
        assert_eq!(comp.scroll, 1);
        assert_eq!(comp.on(&keyed(Key::Up)), None);
        assert_eq!(comp.scroll, 0);
        assert_eq!(comp.on(&keyed(Key::Esc)), Some(Msg::ModalDismissed));
        assert_eq!(comp.on(&keyed(Key::Char('q'))), Some(Msg::ModalDismissed));
    }
}
