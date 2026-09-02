//! `IssueBrowser` — a repo-scoped list of the issues lazybox already
//! tracks, with a live description preview and in-place triage (#1436).
//!
//! Right after an agent carves a batch of issues (#1434), you want to
//! skim them in one place, sanity-check the descriptions, and label /
//! comment / note them — without hunting each one down as its own
//! sidebar workspace. Open issues are already lazybox workspaces in
//! local state, so this is a filtered list view + detail preview over
//! existing state that reuses the three existing write flows
//! (`SetLabels` / `PostReply` / `SetNotes`) and the `#448` markdown
//! reader; no new provider fetch.
//!
//! Layout (same family as the `]]s` snippet picker): a left list of the
//! repo's tracked issues newest-first (`#123 · title · labels · age`)
//! and a right pane rendering the highlighted issue's description as
//! real markdown.
//!
//! Keys: `j`/`k` + arrows navigate, `g`/`G` jump to ends, `Enter` opens
//! the full scrollable reader, `l`/`r`/`n` edit labels / add a comment /
//! edit the local note on the highlighted issue, `o` opens it in the
//! browser, `/` starts filter-as-you-type (Enter commits, Esc clears),
//! and `Esc` clears an active filter or else dismisses.

use crate::components::markdown_doc::{RenderedDoc, render_markdown};
use crate::realm::{Msg, UserEvent};
use lazybox_core::{SessionKey, WorkspaceKey};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyEvent, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tuirealm::state::State;

/// One tracked issue in the browser list. Built by the Model from the
/// repo's issue-workspaces; the body is a snapshot taken at mount time.
#[derive(Clone, Debug)]
pub struct IssueRow {
    /// The issue-workspace's session key — the `Reply` / `Notes` target.
    pub session_key: SessionKey,
    /// The issue-workspace's key — the `SetLabels` target.
    pub workspace_key: WorkspaceKey,
    /// The trailing `#N`, when the task id carries one.
    pub number: Option<u64>,
    pub title: String,
    pub labels: Vec<String>,
    /// Pre-formatted relative age (e.g. `3d ago`).
    pub age: String,
    pub url: String,
    /// Raw description markdown, empty when the issue has no body.
    pub body: String,
    pub unread: bool,
}

impl IssueRow {
    /// The `owner/repo#N · title`-style reader header for this issue.
    fn reader_title(&self) -> String {
        match self.number {
            Some(n) => format!("#{n} · {}", self.title),
            None => self.title.clone(),
        }
    }

    /// The lowercased haystack the filter matches against: number, title,
    /// and labels.
    fn haystack(&self) -> String {
        let mut s = String::new();
        if let Some(n) = self.number {
            s.push('#');
            s.push_str(&n.to_string());
            s.push(' ');
        }
        s.push_str(&self.title);
        for l in &self.labels {
            s.push(' ');
            s.push_str(l);
        }
        s.to_lowercase()
    }
}

/// What a detail-pane keypress asks the Model to do with the highlighted
/// issue. Each variant carries the resolved target so a filtered / re-
/// ordered list can't dispatch against the wrong row (#512).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueBrowserAction {
    /// `Enter` — open the full scrollable markdown reader (#448).
    Read { title: String, body: String },
    /// `l` — edit labels → `Command::SetLabels`.
    Labels(WorkspaceKey),
    /// `r` — add a comment → `Command::PostReply`.
    Reply(SessionKey),
    /// `n` — edit the local note → `Command::SetNotes`.
    Note(SessionKey),
    /// `o` — open the issue in the browser.
    OpenUrl(String),
}

pub struct IssueBrowser {
    /// The repo whose issues these are, for the modal title.
    repo: String,
    /// Issue rows in display order (newest-first), as passed by the Model.
    rows: Vec<IssueRow>,
    filter: String,
    /// `/` filter-as-you-type mode: typed chars extend the filter instead
    /// of triggering detail actions.
    filtering: bool,
    /// Cursor into `visible`. `None` when nothing matches.
    cursor: Option<usize>,
    /// Indices into `rows` matching the current filter, in display order.
    visible: Vec<usize>,
    /// Topmost visible list line, kept in step with the cursor in `view`.
    list_scroll: usize,
    /// Cached preview render, keyed by `(row_idx, width)` so a re-render
    /// only happens when the selection or the pane width changes.
    preview: Option<(usize, u16, RenderedDoc)>,
}

impl IssueBrowser {
    pub fn new(repo: impl Into<String>, rows: Vec<IssueRow>) -> Self {
        let mut b = Self {
            repo: repo.into(),
            rows,
            filter: String::new(),
            filtering: false,
            cursor: None,
            visible: Vec::new(),
            list_scroll: 0,
            preview: None,
        };
        b.refilter();
        b
    }

    /// Recompute the visible index list for the current filter and snap
    /// the cursor to the first row.
    fn refilter(&mut self) {
        let q = self.filter.trim().to_lowercase();
        self.visible = (0..self.rows.len())
            .filter(|&i| q.is_empty() || self.rows[i].haystack().contains(&q))
            .collect();
        self.cursor = (!self.visible.is_empty()).then_some(0);
        self.list_scroll = 0;
    }

    /// The `rows` index under the cursor, if any.
    fn selected(&self) -> Option<&IssueRow> {
        self.cursor
            .and_then(|c| self.visible.get(c))
            .map(|&i| &self.rows[i])
    }

    fn move_cursor(&mut self, delta: isize) {
        let Some(c) = self.cursor else { return };
        let last = self.visible.len().saturating_sub(1);
        let next = (c as isize + delta).clamp(0, last as isize) as usize;
        self.cursor = Some(next);
    }

    pub fn on_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, Key::Char('c')) {
            return Some(Msg::ModalDismissed);
        }
        if self.filtering {
            return self.on_filter_key(key);
        }
        match key.code {
            Key::Esc => {
                // Esc clears an active filter first, then dismisses — the
                // same "narrow, then leave" chain the sidebar search uses.
                if self.filter.is_empty() {
                    Some(Msg::ModalDismissed)
                } else {
                    self.filter.clear();
                    self.refilter();
                    None
                }
            }
            Key::Down | Key::Char('j') => {
                self.move_cursor(1);
                None
            }
            Key::Up | Key::Char('k') => {
                self.move_cursor(-1);
                None
            }
            Key::Home | Key::Char('g') => {
                self.cursor = (!self.visible.is_empty()).then_some(0);
                None
            }
            Key::End | Key::Char('G') => {
                self.cursor = (!self.visible.is_empty()).then_some(self.visible.len() - 1);
                None
            }
            Key::Char('/') => {
                self.filtering = true;
                None
            }
            Key::Enter => self.selected().map(|r| {
                Msg::IssueBrowserAction(IssueBrowserAction::Read {
                    title: r.reader_title(),
                    body: if r.body.is_empty() {
                        "*(no description)*".to_string()
                    } else {
                        r.body.clone()
                    },
                })
            }),
            Key::Char('l') => self.selected().map(|r| {
                Msg::IssueBrowserAction(IssueBrowserAction::Labels(r.workspace_key.clone()))
            }),
            Key::Char('r') => self
                .selected()
                .map(|r| Msg::IssueBrowserAction(IssueBrowserAction::Reply(r.session_key.clone()))),
            Key::Char('n') => self
                .selected()
                .map(|r| Msg::IssueBrowserAction(IssueBrowserAction::Note(r.session_key.clone()))),
            Key::Char('o') => self.selected().and_then(|r| {
                (!r.url.is_empty())
                    .then(|| Msg::IssueBrowserAction(IssueBrowserAction::OpenUrl(r.url.clone())))
            }),
            _ => None,
        }
    }

    fn on_filter_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        match key.code {
            // Leave filter mode: Esc clears the filter, Enter keeps it.
            Key::Esc => {
                self.filtering = false;
                self.filter.clear();
                self.refilter();
                None
            }
            Key::Enter => {
                self.filtering = false;
                None
            }
            Key::Down => {
                self.move_cursor(1);
                None
            }
            Key::Up => {
                self.move_cursor(-1);
                None
            }
            Key::Backspace => {
                self.filter.pop();
                self.refilter();
                None
            }
            Key::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.push(c);
                self.refilter();
                None
            }
            _ => None,
        }
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect, theme: &crate::theme::Theme) {
        if self.visible.is_empty() {
            let msg = if self.rows.is_empty() {
                "  (no tracked issues)"
            } else {
                "  (no matches)"
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    msg,
                    Style::default().fg(theme.text_dim).italic(),
                ))),
                area,
            );
            return;
        }

        let h = area.height.max(1) as usize;
        if let Some(c) = self.cursor {
            if c < self.list_scroll {
                self.list_scroll = c;
            }
            if c >= self.list_scroll + h {
                self.list_scroll = c + 1 - h;
            }
        }
        let max_scroll = self.visible.len().saturating_sub(h);
        if self.list_scroll > max_scroll {
            self.list_scroll = max_scroll;
        }

        let mut lines: Vec<Line> = Vec::with_capacity(h);
        for (vis_i, &row_idx) in self
            .visible
            .iter()
            .enumerate()
            .skip(self.list_scroll)
            .take(h)
        {
            let r = &self.rows[row_idx];
            let is_cursor = self.cursor == Some(vis_i);
            let bg = |s: Style| if is_cursor { s.bg(theme.fill) } else { s };
            let caret = if is_cursor { "▸ " } else { "  " };
            let mut base = Style::default().fg(theme.text_strong);
            if is_cursor {
                base = base.add_modifier(Modifier::BOLD);
            }
            let num = match r.number {
                Some(n) => format!("#{n}"),
                None => "#?".to_string(),
            };
            let mut spans = vec![Span::styled(caret.to_string(), bg(base))];
            if r.unread {
                spans.push(Span::styled(
                    "● ".to_string(),
                    bg(Style::default().fg(theme.accent)),
                ));
            } else {
                spans.push(Span::styled("  ".to_string(), bg(base)));
            }
            spans.push(Span::styled(
                format!("{num:<7} "),
                bg(Style::default().fg(theme.accent)),
            ));
            spans.push(Span::styled(r.title.clone(), bg(base)));
            for l in &r.labels {
                spans.push(Span::styled(
                    format!("  [{l}]"),
                    bg(Style::default().fg(theme.warn)),
                ));
            }
            spans.push(Span::styled(
                format!("  · {}", r.age),
                bg(Style::default().fg(theme.text_dim)),
            ));
            lines.push(Line::from(spans));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_preview(&mut self, frame: &mut Frame, area: Rect, theme: &crate::theme::Theme) {
        let Some(c) = self.cursor else {
            return;
        };
        let row_idx = self.visible[c];
        let need_render = self
            .preview
            .as_ref()
            .map(|(idx, w, _)| *idx != row_idx || *w != area.width)
            .unwrap_or(true);
        if need_render {
            let src = if self.rows[row_idx].body.is_empty() {
                "*(no description)*"
            } else {
                self.rows[row_idx].body.as_str()
            };
            let doc = render_markdown(src, area.width, theme);
            self.preview = Some((row_idx, area.width, doc));
        }
        if let Some((_, _, doc)) = &self.preview {
            frame.render_widget(Paragraph::new(doc.lines.clone()), area);
        }
    }
}

impl Component for IssueBrowser {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 100u16.min(area.width.saturating_sub(4));
        let modal_h = 28u16.min(area.height.saturating_sub(4));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let title = format!(" Issues · {} ", self.repo);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(title, theme.modal_title()))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        if inner.height < 4 || inner.width < 8 {
            return;
        }

        // Header: filter (left) + visible/total count (right).
        let header_rect = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        };
        let count = format!("{}/{}", self.visible.len(), self.rows.len());
        let count_w = count.len() as u16;
        let filter_w = inner.width.saturating_sub(count_w + 1);
        let mut filter_spans = vec![Span::styled(
            "🔍 ",
            Style::default().fg(theme.accent).bold(),
        )];
        if self.filter.is_empty() && !self.filtering {
            filter_spans.push(Span::styled(
                "/ to filter",
                Style::default().fg(theme.text_dim).italic(),
            ));
        } else {
            filter_spans.push(Span::styled(
                self.filter.clone(),
                Style::default().fg(theme.text_strong),
            ));
            if self.filtering {
                filter_spans.push(Span::styled("▌", Style::default().fg(theme.accent)));
            }
        }
        frame.render_widget(
            Paragraph::new(Line::from(filter_spans)),
            Rect {
                width: filter_w,
                ..header_rect
            },
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                count,
                Style::default().fg(theme.text_dim),
            )))
            .alignment(Alignment::Right),
            Rect {
                x: inner.x + filter_w,
                width: count_w,
                ..header_rect
            },
        );

        // Divider under the header.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                theme.divider(),
            ))),
            Rect {
                x: inner.x,
                y: inner.y + 1,
                width: inner.width,
                height: 1,
            },
        );

        // Main = list | preview, leaving the last inner line for help.
        let main = Rect {
            x: inner.x,
            y: inner.y + 2,
            width: inner.width,
            height: inner.height - 3,
        };
        let list_w = if main.width >= 60 {
            (main.width / 2).clamp(28, 52)
        } else {
            main.width
        };
        let list_rect = Rect {
            width: list_w,
            ..main
        };
        self.render_list(frame, list_rect, theme);

        if main.width > list_w + 1 {
            let div_x = main.x + list_w;
            let divider: Vec<Line> = (0..main.height)
                .map(|_| Line::from(Span::styled("│", theme.divider())))
                .collect();
            frame.render_widget(
                Paragraph::new(divider),
                Rect {
                    x: div_x,
                    width: 1,
                    ..main
                },
            );
            let preview_rect = Rect {
                x: div_x + 2,
                width: main.width - list_w - 2,
                ..main
            };
            self.render_preview(frame, preview_rect, theme);
        }

        // Help line.
        let help = if self.filtering {
            vec![
                Span::styled("Type", Style::default().fg(theme.accent).bold()),
                Span::raw(" filter  "),
                Span::styled("Enter", Style::default().fg(theme.success).bold()),
                Span::raw(" done  "),
                Span::styled("Esc", Style::default().fg(theme.error).bold()),
                Span::raw(" clear"),
            ]
        } else {
            vec![
                Span::styled("↑↓", Style::default().fg(theme.accent).bold()),
                Span::raw(" nav  "),
                Span::styled("Enter", Style::default().fg(theme.success).bold()),
                Span::raw(" read  "),
                Span::styled("l", Style::default().fg(theme.accent).bold()),
                Span::raw(" labels  "),
                Span::styled("r", Style::default().fg(theme.accent).bold()),
                Span::raw(" comment  "),
                Span::styled("n", Style::default().fg(theme.accent).bold()),
                Span::raw(" note  "),
                Span::styled("o", Style::default().fg(theme.accent).bold()),
                Span::raw(" open  "),
                Span::styled("/", Style::default().fg(theme.accent).bold()),
                Span::raw(" filter  "),
                Span::styled("Esc", Style::default().fg(theme.error).bold()),
                Span::raw(" close"),
            ]
        };
        frame.render_widget(
            Paragraph::new(Line::from(help)),
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

impl AppComponent<Msg, UserEvent> for IssueBrowser {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(key) => self.on_key(key),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(number: u64, title: &str, labels: &[&str], body: &str) -> IssueRow {
        IssueRow {
            session_key: SessionKey::from(format!("github:o/r#{number}")),
            workspace_key: WorkspaceKey::new(format!("github:o/r#{number}")),
            number: Some(number),
            title: title.to_string(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            age: "3d ago".to_string(),
            url: format!("https://github.com/o/r/issues/{number}"),
            body: body.to_string(),
            unread: false,
        }
    }

    fn rows() -> Vec<IssueRow> {
        vec![
            row(30, "Add carve workflow", &["enhancement"], "Body of 30."),
            row(20, "Fix polling backoff", &["bug"], "Body of 20."),
            row(10, "Refactor sidebar", &[], "Body of 10."),
        ]
    }

    fn ke(c: char) -> KeyEvent {
        KeyEvent::new(Key::Char(c), KeyModifiers::NONE)
    }
    fn key(code: Key) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn cursor_starts_on_first_row() {
        let b = IssueBrowser::new("o/r", rows());
        assert_eq!(b.cursor, Some(0));
        assert_eq!(b.visible, vec![0, 1, 2]);
    }

    #[test]
    fn navigation_clamps_at_both_ends() {
        let mut b = IssueBrowser::new("o/r", rows());
        assert!(b.on_key(&key(Key::Up)).is_none());
        assert_eq!(b.cursor, Some(0));
        let _ = b.on_key(&ke('j'));
        let _ = b.on_key(&ke('j'));
        let _ = b.on_key(&ke('j'));
        assert_eq!(b.cursor, Some(2));
        let _ = b.on_key(&ke('G'));
        assert_eq!(b.cursor, Some(2));
        let _ = b.on_key(&ke('g'));
        assert_eq!(b.cursor, Some(0));
    }

    #[test]
    fn enter_reads_the_selected_issue() {
        let mut b = IssueBrowser::new("o/r", rows());
        match b.on_key(&key(Key::Enter)) {
            Some(Msg::IssueBrowserAction(IssueBrowserAction::Read { title, body })) => {
                assert_eq!(title, "#30 · Add carve workflow");
                assert_eq!(body, "Body of 30.");
            }
            other => panic!("expected a Read action, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_bodiless_issue_reads_placeholder() {
        let mut b = IssueBrowser::new("o/r", vec![row(1, "Empty", &[], "")]);
        match b.on_key(&key(Key::Enter)) {
            Some(Msg::IssueBrowserAction(IssueBrowserAction::Read { body, .. })) => {
                assert_eq!(body, "*(no description)*");
            }
            other => panic!("expected placeholder body, got {other:?}"),
        }
    }

    #[test]
    fn write_actions_carry_the_selected_row_target() {
        let mut b = IssueBrowser::new("o/r", rows());
        let _ = b.on_key(&ke('j')); // select #20
        match b.on_key(&ke('l')) {
            Some(Msg::IssueBrowserAction(IssueBrowserAction::Labels(wk))) => {
                assert_eq!(wk.as_str(), "github:o/r#20");
            }
            other => panic!("expected Labels(#20), got {other:?}"),
        }
        match b.on_key(&ke('r')) {
            Some(Msg::IssueBrowserAction(IssueBrowserAction::Reply(sk))) => {
                assert_eq!(sk.as_str(), "github:o/r#20");
            }
            other => panic!("expected Reply(#20), got {other:?}"),
        }
        match b.on_key(&ke('n')) {
            Some(Msg::IssueBrowserAction(IssueBrowserAction::Note(sk))) => {
                assert_eq!(sk.as_str(), "github:o/r#20");
            }
            other => panic!("expected Note(#20), got {other:?}"),
        }
        match b.on_key(&ke('o')) {
            Some(Msg::IssueBrowserAction(IssueBrowserAction::OpenUrl(url))) => {
                assert!(url.ends_with("/issues/20"), "{url}");
            }
            other => panic!("expected OpenUrl(#20), got {other:?}"),
        }
    }

    #[test]
    fn slash_enters_filter_mode_and_letters_filter_not_act() {
        let mut b = IssueBrowser::new("o/r", rows());
        assert!(b.on_key(&ke('/')).is_none());
        assert!(b.filtering);
        // "poll" only matches #20 by title; the `l` is a filter char here,
        // not a label action.
        for c in "poll".chars() {
            assert!(b.on_key(&ke(c)).is_none());
        }
        assert_eq!(b.visible, vec![1]);
        assert_eq!(b.cursor, Some(0));
        // Enter commits the filter (no action emitted) and leaves filter mode.
        assert!(b.on_key(&key(Key::Enter)).is_none());
        assert!(!b.filtering);
        // Now Enter reads the single filtered row.
        match b.on_key(&key(Key::Enter)) {
            Some(Msg::IssueBrowserAction(IssueBrowserAction::Read { title, .. })) => {
                assert_eq!(title, "#20 · Fix polling backoff");
            }
            other => panic!("expected Read(#20), got {other:?}"),
        }
    }

    #[test]
    fn filter_matches_number_and_labels() {
        let mut b = IssueBrowser::new("o/r", rows());
        let _ = b.on_key(&ke('/'));
        for c in "bug".chars() {
            let _ = b.on_key(&ke(c));
        }
        assert_eq!(b.visible, vec![1], "label 'bug' → #20");
        // Clear and filter by number.
        let _ = b.on_key(&key(Key::Esc));
        assert!(!b.filtering);
        assert_eq!(b.visible.len(), 3);
        let _ = b.on_key(&ke('/'));
        for c in "10".chars() {
            let _ = b.on_key(&ke(c));
        }
        assert_eq!(b.visible, vec![2], "number 10 → #10");
    }

    #[test]
    fn esc_clears_filter_then_dismisses() {
        let mut b = IssueBrowser::new("o/r", rows());
        let _ = b.on_key(&ke('/'));
        let _ = b.on_key(&ke('b'));
        // Esc in filter mode clears + exits filter mode.
        assert!(b.on_key(&key(Key::Esc)).is_none());
        assert!(!b.filtering);
        assert_eq!(b.visible.len(), 3);
        // A stray committed filter: Esc clears it before dismissing.
        let _ = b.on_key(&ke('/'));
        let _ = b.on_key(&ke('b'));
        let _ = b.on_key(&key(Key::Enter)); // commit, stay filtered
        assert!(
            b.on_key(&key(Key::Esc)).is_none(),
            "first Esc clears filter"
        );
        assert_eq!(b.visible.len(), 3);
        // Now Esc with no filter dismisses.
        assert_eq!(b.on_key(&key(Key::Esc)), Some(Msg::ModalDismissed));
    }

    #[test]
    fn ctrl_c_dismisses() {
        let mut b = IssueBrowser::new("o/r", rows());
        assert_eq!(
            b.on_key(&KeyEvent::new(Key::Char('c'), KeyModifiers::CONTROL)),
            Some(Msg::ModalDismissed)
        );
    }

    #[test]
    fn empty_list_is_safe() {
        let mut b = IssueBrowser::new("o/r", Vec::new());
        assert!(b.cursor.is_none());
        assert!(b.on_key(&key(Key::Enter)).is_none());
        assert!(b.on_key(&ke('l')).is_none());
        assert!(b.on_key(&ke('j')).is_none());
    }

    fn render(b: &mut IssueBrowser, w: u16, h: u16) -> String {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|frame| b.view(frame, Rect::new(0, 0, w, h)))
            .unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                let mut r = String::new();
                for x in 0..buf.area.width {
                    r.push_str(buf[(x, y)].symbol());
                }
                r.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_shows_title_rows_and_preview() {
        let mut b = IssueBrowser::new("o/r", rows());
        let out = render(&mut b, 100, 24);
        assert!(out.contains("Issues · o/r"), "modal title: {out}");
        assert!(out.contains("#30"), "issue number: {out}");
        assert!(out.contains("Add carve workflow"), "issue title: {out}");
        assert!(out.contains("3/3"), "count: {out}");
        assert!(out.contains("Body of 30"), "preview body: {out}");
    }
}
