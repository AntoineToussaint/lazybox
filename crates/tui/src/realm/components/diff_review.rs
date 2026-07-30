use crate::components::scrollbar;
use crate::realm::components::scrollable::{centered_rect, draw_frame};
use crate::realm::{Msg, UserEvent};
use lazybox_core::WorkspaceKey;
use lazybox_ipc::{DiffLineKindDto, TerminalId, WorkspaceDiffDto, WorkspaceDiffTarget};
use std::borrow::Cow;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyModifiers, MouseEventKind};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;
use tuirealm::state::State;

const WHEEL_STEP: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffReviewComment {
    pub path: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub hunk_header: String,
    pub referenced_line: String,
    pub context: Vec<String>,
    pub body: String,
    pub(crate) anchor_row: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    StatusHeader,
    Status(usize),
    Clean,
    Truncated,
    Spacer,
    StatHeader,
    Stat(usize),
    File(usize),
    Header(usize, usize),
    Hunk(usize, usize),
    DiffLine(usize, usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisualKind {
    Dim,
    File,
    Hunk,
    Context,
    Addition,
    Deletion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputMode {
    Normal,
    Search(String),
    Comment(String),
}

pub struct DiffReview {
    workspace_key: WorkspaceKey,
    target: WorkspaceDiffTarget,
    agent_terminal_ids: Vec<TerminalId>,
    diff: WorkspaceDiffDto,
    rows: Vec<RowKind>,
    cursor: usize,
    scroll: usize,
    horizontal_scroll: u16,
    body_height: usize,
    comments: Vec<DiffReviewComment>,
    search: String,
    mode: InputMode,
}

impl DiffReview {
    pub fn new(
        workspace_key: WorkspaceKey,
        target: WorkspaceDiffTarget,
        agent_terminal_ids: Vec<TerminalId>,
        diff: WorkspaceDiffDto,
    ) -> Self {
        let rows = build_rows(&diff);
        Self {
            workspace_key,
            target,
            agent_terminal_ids,
            diff,
            rows,
            cursor: 0,
            scroll: 0,
            horizontal_scroll: 0,
            body_height: 1,
            comments: Vec::new(),
            search: String::new(),
            mode: InputMode::Normal,
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.rows.len() - 1);
    }

    fn jump_to(&mut self, forward: bool, predicate: impl Fn(RowKind) -> bool) {
        if self.rows.is_empty() {
            return;
        }
        let indices: Box<dyn Iterator<Item = usize>> = if forward {
            Box::new((self.cursor + 1)..self.rows.len())
        } else {
            Box::new((0..self.cursor).rev())
        };
        if let Some(index) = indices
            .into_iter()
            .find(|index| predicate(self.rows[*index]))
        {
            self.cursor = index;
        }
    }

    fn find_match(&mut self, forward: bool) {
        if self.search.is_empty() || self.rows.is_empty() {
            return;
        }
        let needle = self.search.to_lowercase();
        let len = self.rows.len();
        for step in 1..=len {
            let index = if forward {
                (self.cursor + step) % len
            } else {
                (self.cursor + len - (step % len)) % len
            };
            if self.row_text(index).to_lowercase().contains(&needle) {
                self.cursor = index;
                return;
            }
        }
    }

    fn begin_comment(&mut self) {
        if matches!(
            self.rows.get(self.cursor),
            Some(RowKind::Hunk(..) | RowKind::DiffLine(..))
        ) {
            self.mode = InputMode::Comment(String::new());
        }
    }

    fn save_comment(&mut self) {
        let InputMode::Comment(input) = &self.mode else {
            return;
        };
        let body = input.trim();
        if body.is_empty() {
            return;
        }
        let Some(row) = self.rows.get(self.cursor) else {
            return;
        };
        let (file_index, hunk_index, line_index) = match *row {
            RowKind::Hunk(file, hunk) => (file, hunk, None),
            RowKind::DiffLine(file, hunk, line) => (file, hunk, Some(line)),
            _ => return,
        };
        let file = &self.diff.files[file_index];
        let hunk = &file.hunks[hunk_index];
        let line = line_index.map(|index| &hunk.lines[index]);
        let (old_line, new_line) = match line {
            Some(line) => (line.old_line, line.new_line),
            None => (
                hunk.lines.iter().find_map(|line| line.old_line),
                hunk.lines.iter().find_map(|line| line.new_line),
            ),
        };
        let context = match line_index {
            Some(index) => {
                let start = index.saturating_sub(2);
                let end = (index + 3).min(hunk.lines.len());
                hunk.lines[start..end]
                    .iter()
                    .map(|line| line.text.clone())
                    .collect()
            }
            None => hunk
                .lines
                .iter()
                .take(5)
                .map(|line| line.text.clone())
                .collect(),
        };
        self.comments.push(DiffReviewComment {
            path: file.path.clone(),
            old_line,
            new_line,
            hunk_header: hunk.header.clone(),
            referenced_line: line
                .map(|line| line.text.clone())
                .unwrap_or_else(|| hunk.header.clone()),
            context,
            body: body.to_string(),
            anchor_row: self.cursor,
        });
        self.mode = InputMode::Normal;
    }

    fn remove_comments_at_cursor(&mut self) {
        self.comments
            .retain(|comment| comment.anchor_row != self.cursor);
    }

    fn ensure_cursor_visible(&mut self) {
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + self.body_height {
            self.scroll = self.cursor + 1 - self.body_height;
        }
        let max = self.rows.len().saturating_sub(self.body_height);
        self.scroll = self.scroll.min(max);
    }

    fn row_text(&self, index: usize) -> Cow<'_, str> {
        match self.rows[index] {
            RowKind::StatusHeader => Cow::Borrowed("STATUS"),
            RowKind::Status(index) => Cow::Borrowed(&self.diff.status[index]),
            RowKind::Clean => Cow::Borrowed("clean worktree"),
            RowKind::Truncated => Cow::Borrowed(
                "diff output was truncated; review the checkout directly for omitted changes",
            ),
            RowKind::Spacer => Cow::Borrowed(""),
            RowKind::StatHeader => Cow::Borrowed("STAT"),
            RowKind::Stat(index) => Cow::Borrowed(&self.diff.stat[index]),
            RowKind::File(index) => Cow::Owned(format!("FILE {}", self.diff.files[index].path)),
            RowKind::Header(file, header) => Cow::Borrowed(&self.diff.files[file].headers[header]),
            RowKind::Hunk(file, hunk) => Cow::Borrowed(&self.diff.files[file].hunks[hunk].header),
            RowKind::DiffLine(file, hunk, line) => {
                let line = &self.diff.files[file].hunks[hunk].lines[line];
                let old = line
                    .old_line
                    .map(|line| line.to_string())
                    .unwrap_or_default();
                let new = line
                    .new_line
                    .map(|line| line.to_string())
                    .unwrap_or_default();
                Cow::Owned(format!("{old:>5} {new:>5} │ {}", line.text))
            }
        }
    }

    fn row_visual(&self, kind: RowKind) -> VisualKind {
        match kind {
            RowKind::StatusHeader | RowKind::StatHeader | RowKind::File(_) => VisualKind::File,
            RowKind::Status(_) => VisualKind::Context,
            RowKind::Truncated => VisualKind::Deletion,
            RowKind::Clean | RowKind::Spacer | RowKind::Stat(_) | RowKind::Header(..) => {
                VisualKind::Dim
            }
            RowKind::Hunk(..) => VisualKind::Hunk,
            RowKind::DiffLine(file, hunk, line) => {
                match self.diff.files[file].hunks[hunk].lines[line].kind {
                    DiffLineKindDto::Context | DiffLineKindDto::Meta => VisualKind::Context,
                    DiffLineKindDto::Addition => VisualKind::Addition,
                    DiffLineKindDto::Deletion => VisualKind::Deletion,
                }
            }
        }
    }

    fn handle_input(&mut self, event: &Event<UserEvent>) -> bool {
        if matches!(self.mode, InputMode::Normal) {
            return false;
        }
        if let Event::Paste(text) = event {
            let input = match &mut self.mode {
                InputMode::Search(input) | InputMode::Comment(input) => input,
                InputMode::Normal => return false,
            };
            input.extend(text.chars().filter(|character| !character.is_control()));
            return true;
        }
        let Event::Keyboard(key) = event else {
            return true;
        };
        match key.code {
            Key::Esc => {
                self.mode = InputMode::Normal;
            }
            Key::Enter => match &self.mode {
                InputMode::Search(input) => {
                    self.search = input.clone();
                    self.mode = InputMode::Normal;
                    self.find_match(true);
                }
                InputMode::Comment(_) => self.save_comment(),
                InputMode::Normal => {}
            },
            Key::Backspace => match &mut self.mode {
                InputMode::Search(input) | InputMode::Comment(input) => {
                    input.pop();
                }
                InputMode::Normal => {}
            },
            Key::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                match &mut self.mode {
                    InputMode::Search(input) | InputMode::Comment(input) => {
                        input.push(character);
                    }
                    InputMode::Normal => {}
                }
            }
            _ => {}
        }
        true
    }
}

impl Component for DiffReview {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let width = area.width.saturating_sub(4).clamp(24, 160).min(area.width);
        let height = area.height.saturating_sub(2).max(6).min(area.height);
        let modal = centered_rect(area, width, height);
        let title = format!(
            " Diff review · {} file{} ",
            self.diff.files.len(),
            if self.diff.files.len() == 1 { "" } else { "s" }
        );
        let inner = draw_frame(frame, modal, &title, theme);
        if inner.height < 3 || inner.width < 3 {
            return;
        }

        let body_height = inner.height.saturating_sub(2) as usize;
        self.body_height = body_height.max(1);
        self.ensure_cursor_visible();
        let body_area = Rect::new(
            inner.x,
            inner.y,
            inner.width.saturating_sub(1),
            body_height as u16,
        );
        let gutter = Rect::new(
            inner.x + inner.width.saturating_sub(1),
            inner.y,
            1,
            body_height as u16,
        );
        let input_area = Rect::new(inner.x, inner.y + body_height as u16, inner.width, 1);
        let hint_area = Rect::new(inner.x, input_area.y + 1, inner.width, 1);

        let end = (self.scroll + body_height).min(self.rows.len());
        let lines = self.rows[self.scroll..end]
            .iter()
            .enumerate()
            .map(|(visible_index, kind)| {
                let index = self.scroll + visible_index;
                let selected = index == self.cursor;
                let has_comment = self
                    .comments
                    .iter()
                    .any(|comment| comment.anchor_row == index);
                let visual = self.row_visual(*kind);
                let color = match visual {
                    VisualKind::Dim => theme.text_dim,
                    VisualKind::File | VisualKind::Hunk => theme.accent,
                    VisualKind::Context => theme.text_strong,
                    VisualKind::Addition => theme.success,
                    VisualKind::Deletion => theme.error,
                };
                let marker = match (selected, has_comment) {
                    (true, true) => "●",
                    (true, false) => "›",
                    (false, true) => "●",
                    (false, false) => " ",
                };
                let mut style = Style::default().fg(color);
                if selected {
                    style = style.bg(theme.fill);
                }
                if matches!(visual, VisualKind::File) {
                    style = style.add_modifier(Modifier::BOLD);
                }
                Line::from(Span::styled(
                    format!("{marker} {}", self.row_text(index)),
                    style,
                ))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines).scroll((0, self.horizontal_scroll)),
            body_area,
        );
        scrollbar::render_vertical(frame, gutter, self.rows.len(), body_height, self.scroll);

        let input = match &self.mode {
            InputMode::Normal if self.comments.is_empty() => {
                "c comment · / search · [/] hunks · {/} files · h/l horizontal".to_string()
            }
            InputMode::Normal => format!(
                "{} comment{} drafted · Shift-S send · x remove here",
                self.comments.len(),
                if self.comments.len() == 1 { "" } else { "s" }
            ),
            InputMode::Search(input) => format!("/{input}█"),
            InputMode::Comment(input) => format!("Comment: {input}█"),
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(input, theme.hint()))),
            input_area,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "j/k · PgUp/PgDn navigate · n/N search · Esc close",
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

impl AppComponent<Msg, UserEvent> for DiffReview {
    fn on(&mut self, event: &Event<UserEvent>) -> Option<Msg> {
        if self.handle_input(event) {
            return None;
        }
        match event {
            Event::Keyboard(key) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    Key::Down | Key::Char('j') => self.move_cursor(1),
                    Key::Up | Key::Char('k') => self.move_cursor(-1),
                    Key::PageDown => self.move_cursor(self.body_height as isize),
                    Key::PageUp => self.move_cursor(-(self.body_height as isize)),
                    Key::Home | Key::Char('g') => self.cursor = 0,
                    Key::End | Key::Char('G') => {
                        self.cursor = self.rows.len().saturating_sub(1);
                    }
                    Key::Left | Key::Char('h') => {
                        self.horizontal_scroll = self.horizontal_scroll.saturating_sub(4);
                    }
                    Key::Right | Key::Char('l') => {
                        self.horizontal_scroll = self.horizontal_scroll.saturating_add(4);
                    }
                    Key::Char(']') => {
                        self.jump_to(true, |kind| matches!(kind, RowKind::Hunk(..)));
                    }
                    Key::Char('[') => {
                        self.jump_to(false, |kind| matches!(kind, RowKind::Hunk(..)));
                    }
                    Key::Char('}') => {
                        self.jump_to(true, |kind| matches!(kind, RowKind::File(_)));
                    }
                    Key::Char('{') => {
                        self.jump_to(false, |kind| matches!(kind, RowKind::File(_)));
                    }
                    Key::Char('/') => self.mode = InputMode::Search(String::new()),
                    Key::Char('n') => self.find_match(true),
                    Key::Char('N') => self.find_match(false),
                    Key::Char('c') if !ctrl => self.begin_comment(),
                    Key::Char('x') if !ctrl => self.remove_comments_at_cursor(),
                    Key::Char('S') if !self.comments.is_empty() => {
                        return Some(Msg::DiffReviewSubmitted {
                            workspace_key: self.workspace_key.clone(),
                            target: self.target.clone(),
                            agent_terminal_ids: self.agent_terminal_ids.clone(),
                            comments: self.comments.clone(),
                        });
                    }
                    Key::Esc | Key::Char('q') => return Some(Msg::ModalDismissed),
                    Key::Char('c') if ctrl => return Some(Msg::ModalDismissed),
                    _ => {}
                }
                self.ensure_cursor_visible();
                None
            }
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollDown => self.move_cursor(WHEEL_STEP as isize),
                    MouseEventKind::ScrollUp => self.move_cursor(-(WHEEL_STEP as isize)),
                    _ => {}
                }
                self.ensure_cursor_visible();
                None
            }
            _ => None,
        }
    }
}

fn build_rows(diff: &WorkspaceDiffDto) -> Vec<RowKind> {
    let mut rows = vec![RowKind::StatusHeader];
    if diff.truncated {
        rows.push(RowKind::Truncated);
    }
    if diff.status.is_empty() {
        rows.push(RowKind::Clean);
    } else {
        rows.extend((0..diff.status.len()).map(RowKind::Status));
    }
    if !diff.stat.is_empty() {
        rows.push(RowKind::Spacer);
        rows.push(RowKind::StatHeader);
        rows.extend((0..diff.stat.len()).map(RowKind::Stat));
    }
    for (file_index, file) in diff.files.iter().enumerate() {
        rows.push(RowKind::Spacer);
        rows.push(RowKind::File(file_index));
        rows.extend((0..file.headers.len()).map(|header| RowKind::Header(file_index, header)));
        for (hunk_index, hunk) in file.hunks.iter().enumerate() {
            rows.push(RowKind::Hunk(file_index, hunk_index));
            for line_index in 0..hunk.lines.len() {
                rows.push(RowKind::DiffLine(file_index, hunk_index, line_index));
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_ipc::{DiffFileDto, DiffHunkDto, DiffLineDto};
    use tuirealm::event::KeyEvent;

    fn sample() -> WorkspaceDiffDto {
        WorkspaceDiffDto {
            status: vec![" M src/lib.rs".into()],
            stat: vec![" src/lib.rs | 2 +".into()],
            truncated: false,
            files: vec![DiffFileDto {
                old_path: Some("src/lib.rs".into()),
                path: "src/lib.rs".into(),
                headers: vec!["diff --git a/src/lib.rs b/src/lib.rs".into()],
                hunks: vec![DiffHunkDto {
                    header: "@@ -10,2 +10,3 @@ fn run()".into(),
                    old_start: 10,
                    new_start: 10,
                    lines: vec![
                        DiffLineDto {
                            kind: DiffLineKindDto::Context,
                            text: " keep();".into(),
                            old_line: Some(10),
                            new_line: Some(10),
                        },
                        DiffLineDto {
                            kind: DiffLineKindDto::Addition,
                            text: "+fix();".into(),
                            old_line: None,
                            new_line: Some(11),
                        },
                    ],
                }],
            }],
        }
    }

    fn review(diff: WorkspaceDiffDto) -> DiffReview {
        DiffReview::new(
            WorkspaceKey::new("w"),
            WorkspaceDiffTarget::Session(lazybox_core::SessionId::new()),
            vec![TerminalId(7)],
            diff,
        )
    }

    fn key(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn render(review: &mut DiffReview) -> String {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| review.view(frame, Rect::new(0, 0, 100, 24)))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_status_colored_diff_and_navigation_help() {
        let mut review = review(sample());
        let output = render(&mut review);
        assert!(output.contains("Diff review · 1 file"));
        assert!(output.contains("STATUS"));
        assert!(output.contains("FILE src/lib.rs"));
        assert!(output.contains("+fix();"));
        assert!(output.contains("PgUp/PgDn navigate"));
    }

    #[test]
    fn search_wraps_and_hunk_navigation_moves_the_cursor() {
        let mut review = review(sample());
        review.cursor = review.rows.len() - 1;
        review.mode = InputMode::Search("fix".into());
        review.handle_input(&key(Key::Enter));
        assert!(review.row_text(review.cursor).contains("fix"));
        let line = review.cursor;
        review.on(&key(Key::Char('[')));
        assert!(matches!(review.rows[review.cursor], RowKind::Hunk(..)));
        assert!(review.cursor < line);
    }

    #[test]
    fn line_comment_captures_location_and_context_for_submission() {
        let mut review = review(sample());
        review.cursor = (0..review.rows.len())
            .position(|index| review.row_text(index).contains("+fix();"))
            .expect("addition row");
        review.on(&key(Key::Char('c')));
        review.on(&key(Key::Char('r')));
        review.on(&key(Key::Char('e')));
        review.on(&key(Key::Char('n')));
        review.on(&key(Key::Char('a')));
        review.on(&key(Key::Char('m')));
        review.on(&key(Key::Char('e')));
        review.on(&key(Key::Enter));

        let Some(Msg::DiffReviewSubmitted { comments, .. }) = review.on(&key(Key::Char('S')))
        else {
            panic!("expected review submission");
        };
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].path, "src/lib.rs");
        assert_eq!(comments[0].new_line, Some(11));
        assert_eq!(comments[0].referenced_line, "+fix();");
        assert_eq!(comments[0].body, "rename");
        assert_eq!(comments[0].context, vec![" keep();", "+fix();"]);
    }

    #[test]
    fn horizontal_navigation_reveals_the_end_of_long_diff_lines() {
        let mut diff = sample();
        diff.files[0].hunks[0].lines[1].text = format!("+{}END-OF-LONG-LINE", "x".repeat(180));
        let mut review = review(diff);
        review.cursor = (0..review.rows.len())
            .position(|index| review.row_text(index).contains("END-OF-LONG-LINE"))
            .expect("long line");

        for _ in 0..30 {
            review.on(&key(Key::Char('l')));
        }

        assert!(render(&mut review).contains("END-OF-LONG-LINE"));
    }

    #[test]
    fn deletion_hunk_comment_anchors_to_an_existing_old_line() {
        let mut diff = sample();
        diff.files[0].hunks[0] = DiffHunkDto {
            header: "@@ -10,1 +10,0 @@".into(),
            old_start: 10,
            new_start: 10,
            lines: vec![DiffLineDto {
                kind: DiffLineKindDto::Deletion,
                text: "-remove();".into(),
                old_line: Some(10),
                new_line: None,
            }],
        };
        let mut review = review(diff);
        review.cursor = review
            .rows
            .iter()
            .position(|row| matches!(row, RowKind::Hunk(..)))
            .expect("hunk");
        review.mode = InputMode::Comment("remove this concern".into());
        review.save_comment();

        assert_eq!(review.comments[0].old_line, Some(10));
        assert_eq!(review.comments[0].new_line, None);
    }

    #[test]
    fn truncated_diff_is_disclosed_in_the_viewer() {
        let mut diff = sample();
        diff.truncated = true;
        let mut review = review(diff);

        assert!(render(&mut review).contains("diff output was truncated"));
    }
}
