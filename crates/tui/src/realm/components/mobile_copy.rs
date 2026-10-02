//! Clipboard-only line selection over a frozen terminal snapshot.
use crate::realm::{ChoicePayload, Msg, UserEvent};
use tuirealm::{
    command::{Cmd, CmdResult},
    component::{AppComponent, Component},
    event::{Event, Key, KeyEvent, KeyModifiers, MouseButton, MouseEventKind},
    props::{AttrValue, Attribute, QueryResult},
    ratatui::{
        Frame,
        layout::Rect,
        text::Line,
        widgets::{Block, Borders, Clear, Paragraph},
    },
    state::State,
};

pub(crate) struct MobileCopy {
    lines: Vec<String>,
    cursor: usize,
    anchor: Option<usize>,
    review: Option<String>,
    dedent: bool,
    scroll: usize,
    body: Rect,
    visible: Vec<usize>,
    // A frozen history snapshot may contain thousands of lines. Rewrap only
    // when width/content changes, never for each cursor move or live PTY frame.
    wrapped_width: Option<u16>,
    rendered: Vec<String>,
    owners: Vec<usize>,
    row_starts: Vec<usize>,
}
impl MobileCopy {
    pub(crate) fn new(mut lines: Vec<String>, cursor: usize) -> Self {
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            cursor: cursor.min(lines.len() - 1),
            lines,
            anchor: None,
            review: None,
            dedent: false,
            scroll: 0,
            body: Rect::default(),
            visible: Vec::new(),
            wrapped_width: None,
            rendered: Vec::new(),
            owners: Vec::new(),
            row_starts: Vec::new(),
        }
    }
    pub(crate) fn review(text: String) -> Self {
        let lines: Vec<_> = text.split('\n').map(str::to_owned).collect();
        let mut copy = Self::new(lines, 0);
        copy.anchor = Some(0);
        copy.cursor = copy.lines.len() - 1;
        copy.review = Some(text);
        copy
    }
    fn range(&self) -> std::ops::RangeInclusive<usize> {
        let anchor = self.anchor.unwrap_or(self.cursor);
        anchor.min(self.cursor)..=anchor.max(self.cursor)
    }
    fn selected(&self) -> String {
        let lines = &self.lines[self.range()];
        let margin = if self.dedent {
            lines
                .iter()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.bytes().take_while(|b| *b == b' ').count())
                .min()
                .unwrap_or(0)
        } else {
            0
        };
        lines
            .iter()
            .map(|l| {
                if margin == 0 {
                    l.as_str()
                } else if l.trim().is_empty() {
                    ""
                } else {
                    &l[margin..]
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn wrap_at(&mut self, width: u16) {
        if self.wrapped_width == Some(width) {
            return;
        }
        self.wrapped_width = Some(width);
        let width = usize::from(width.max(1));
        let source = self
            .review
            .as_ref()
            .map(|text| text.split('\n').map(str::to_owned).collect::<Vec<_>>());
        let lines = source.as_ref().unwrap_or(&self.lines);
        self.rendered.clear();
        self.owners.clear();
        self.row_starts.clear();

        for (i, line) in lines.iter().enumerate() {
            self.row_starts.push(self.rendered.len());
            let mut text = String::new();
            for g in crate::util::graphemes(line) {
                if !text.is_empty()
                    && crate::util::visual_width(&text) + crate::util::visual_width(g) > width
                {
                    self.rendered.push(std::mem::take(&mut text));
                    self.owners.push(i);
                }
                text.push_str(g);
            }
            self.rendered.push(text);
            self.owners.push(i);
        }
    }
    fn move_by(&mut self, delta: isize) {
        if self.review.is_some() {
            self.scroll = self.scroll.saturating_add_signed(delta);
        } else {
            self.cursor = self
                .cursor
                .saturating_add_signed(delta)
                .min(self.lines.len() - 1);
        }
    }
}

impl Component for MobileCopy {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let title = if self.review.is_some() {
            "Review script / text"
        } else {
            "Select script / text"
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(theme.modal_border());
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        self.body = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(2),
        );
        self.wrap_at(inner.width);
        let height = usize::from(self.body.height);
        if self.review.is_none()
            && let Some(&row) = self.row_starts.get(self.cursor)
        {
            if row < self.scroll {
                self.scroll = row;
            }
            if row >= self.scroll.saturating_add(height) {
                self.scroll = row.saturating_sub(height.saturating_sub(1));
            }
        }
        self.scroll = self.scroll.min(self.rendered.len().saturating_sub(height));
        self.visible = self
            .owners
            .iter()
            .skip(self.scroll)
            .take(height)
            .copied()
            .collect();
        let rows: Vec<Line<'_>> = self
            .rendered
            .iter()
            .zip(&self.owners)
            .skip(self.scroll)
            .take(height)
            .map(|(text, i)| {
                let style = if self.review.is_none() {
                    theme.row_band(*i == self.cursor, true, self.range().contains(i))
                } else {
                    None
                };
                Line::styled(
                    text.as_str(),
                    style.unwrap_or_else(|| {
                        tuirealm::ratatui::style::Style::default().fg(theme.text_strong)
                    }),
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(rows), self.body);
        let help = if self.review.is_some() {
            if self.dedent {
                "Enter copy  Esc edit\nj/k scroll  d keep indentation"
            } else {
                "Enter copy  Esc edit\nj/k scroll  d remove margin"
            }
        } else {
            "j/k move  v mark  Enter review\ng/G ends  Esc back"
        };
        frame.render_widget(
            Paragraph::new(help),
            Rect::new(
                inner.x,
                self.body.bottom(),
                inner.width,
                inner.height.min(2),
            ),
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
impl AppComponent<Msg, UserEvent> for MobileCopy {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        if let Event::Mouse(mouse) = ev {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.move_by(-1),
                MouseEventKind::ScrollDown => self.move_by(1),
                MouseEventKind::Down(MouseButton::Left)
                    if self.review.is_none()
                        && self.body.contains((mouse.column, mouse.row).into()) =>
                {
                    if let Some(i) = self.visible.get(usize::from(mouse.row - self.body.y)) {
                        self.cursor = *i;
                    }
                }
                _ => (),
            }
            return None;
        }
        let Event::Keyboard(KeyEvent { code, modifiers }) = ev else {
            return None;
        };
        if *code == Key::Esc {
            if self.review.take().is_some() {
                self.wrapped_width = None;
                self.scroll = 0;
                return None;
            }
            return Some(Msg::ModalDismissed);
        }
        if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return None;
        }
        match code {
            Key::Char('j') | Key::Down => self.move_by(1),
            Key::Char('k') | Key::Up => self.move_by(-1),
            Key::Char('g') => {
                if self.review.is_some() {
                    self.scroll = 0;
                } else {
                    self.cursor = 0;
                }
            }
            Key::Char('G') => {
                if self.review.is_some() {
                    self.scroll = usize::MAX;
                } else {
                    self.cursor = self.lines.len() - 1;
                }
            }
            Key::Char('v' | ' ') if self.review.is_none() => {
                self.anchor = if self.anchor.is_some() {
                    None
                } else {
                    Some(self.cursor)
                }
            }
            Key::Char('d') if self.review.is_some() => {
                self.dedent = !self.dedent;
                self.review = Some(self.selected());
                self.wrapped_width = None;
            }
            Key::Enter => {
                if let Some(text) = &self.review {
                    return Some(Msg::ChoicePicked(vec![ChoicePayload::Text(text.clone())]));
                }
                self.review = Some(self.selected());
                self.wrapped_width = None;
                self.scroll = 0;
            }
            _ => (),
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(c: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent::from(c))
    }
    #[test]
    fn range_review_preserves_script_and_requires_explicit_copy() {
        let mut copy = MobileCopy::new(
            vec![
                "explanation".into(),
                "  cat <<'EOF'".into(),
                "    日本 é".into(),
                "".into(),
                "  EOF".into(),
                "more explanation".into(),
            ],
            1,
        );
        copy.on(&key(Key::Char('v')));
        for _ in 0..3 {
            copy.on(&key(Key::Char('j')));
        }
        assert!(copy.on(&key(Key::Enter)).is_none());
        assert_eq!(
            copy.review.as_deref(),
            Some("  cat <<'EOF'\n    日本 é\n\n  EOF")
        );
        copy.on(&key(Key::Char('d')));
        assert!(
            matches!(copy.on(&key(Key::Enter)), Some(Msg::ChoicePicked(p)) if matches!(&p[0], ChoicePayload::Text(s) if s == "cat <<'EOF'\n  日本 é\n\nEOF"))
        );
        copy.on(&key(Key::Esc));
        assert!(copy.review.is_none());
        assert!(matches!(copy.on(&key(Key::Esc)), Some(Msg::ModalDismissed)));
    }
    #[test]
    fn reverse_selection_and_single_command_are_exact() {
        let mut copy = MobileCopy::new(
            vec!["printf '%s\\n' 'a b'".into(), "".into(), "echo done".into()],
            2,
        );
        assert_eq!(copy.selected(), "echo done");
        copy.on(&key(Key::Char('v')));
        copy.on(&key(Key::Char('g')));
        assert_eq!(copy.selected(), "printf '%s\\n' 'a b'\n\necho done");
    }
    #[test]
    fn a_wheel_report_moves_the_line_cursor() {
        // The only pointer a phone has. This arm was unreachable until
        // `Id::MobileCopyText` was added to `Id::consumes_scroll`, which is
        // where the router decides whether to forward a notch at all.
        let mut copy = MobileCopy::new((0..10).map(|i| format!("line {i}")).collect(), 0);
        let wheel = |kind| {
            Event::Mouse(tuirealm::event::MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        copy.on(&wheel(MouseEventKind::ScrollDown));
        copy.on(&wheel(MouseEventKind::ScrollDown));
        assert_eq!(copy.cursor, 2);
        copy.on(&wheel(MouseEventKind::ScrollUp));
        assert_eq!(copy.cursor, 1);
    }

    #[test]
    fn narrow_copy_controls_and_wrapped_text_remain_visible() {
        use tuirealm::ratatui::{Terminal, backend::TestBackend};
        for (width, height) in [(32, 12), (39, 18), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut copy = MobileCopy::new(
                vec!["echo 'a very long argument that wraps across several phone rows'".into()],
                0,
            );
            terminal.draw(|f| copy.view(f, f.area())).unwrap();
            if width > 1 {
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>();
                assert!(text.contains("j/k move  v mark  Enter review"));
                assert!(text.contains("Esc back"));
                assert!(copy.visible.len() > 1);
            }
        }
    }
}
