//! The review modal behind `g v` — a near-full-screen viewer with a file
//! tree on the left and the changes on the right, side by side where the
//! pane is wide enough and unified where it is not.
//!
//! The source is the local checkout (worktree or linked), which is the
//! only diff that exists before a branch is pushed; the header names it
//! so a later PR source can never be confused for it.

use crate::components::scrollbar;
use crate::realm::components::scrollable::{centered_rect, draw_frame};
use crate::realm::{Msg, UserEvent};
use lazybox_core::WorkspaceKey;
use lazybox_ipc::{
    DiffFileDto, DiffLineKindDto, TerminalId, WorkspaceDiffDto, WorkspaceDiffTarget,
};
use std::borrow::Cow;
use std::collections::BTreeMap;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyModifiers, MouseButton, MouseEventKind};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;
use tuirealm::state::State;

const WHEEL_STEP: usize = 3;
/// Narrowest changes pane that still leaves each half of a split view
/// readable. Below it the viewer falls back to unified rather than
/// truncating both sides.
const MIN_SPLIT_WIDTH: u16 = 64;
/// Narrowest modal that can afford to spend a quarter of its width on
/// the tree.
const MIN_TREE_TOTAL: u16 = 72;
const LINE_NUMBER_WIDTH: usize = 5;

/// Where a drafted comment hangs in the diff. A row index cannot serve:
/// a split row covers up to two source lines and a unified row exactly
/// one, so the indices change under the layout toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommentAnchor {
    pub(crate) file: usize,
    pub(crate) hunk: usize,
    /// `None` when the comment hangs off the hunk header itself.
    pub(crate) line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffReviewComment {
    pub path: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub hunk_header: String,
    pub referenced_line: String,
    pub context: Vec<String>,
    pub body: String,
    pub(crate) anchor: CommentAnchor,
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
    /// One side-by-side row: the old-side and new-side line of a hunk,
    /// either of which is absent where the sides are uneven.
    DiffPair(usize, usize, Option<usize>, Option<usize>),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tree,
    Diff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputMode {
    Normal,
    Search(String),
    Comment(String),
}

/// One row of the file tree: a directory (with its collapsed chain of
/// single-child parents folded into the label) or a changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeNode {
    depth: usize,
    label: String,
    file: Option<usize>,
    added: usize,
    removed: usize,
}

pub struct DiffReview {
    workspace_key: WorkspaceKey,
    target: WorkspaceDiffTarget,
    agent_terminal_ids: Vec<TerminalId>,
    diff: WorkspaceDiffDto,
    rows: Vec<RowKind>,
    split: bool,
    split_preferred: bool,
    tree: Vec<TreeNode>,
    tree_cursor: usize,
    tree_scroll: usize,
    tree_height: usize,
    tree_shown: bool,
    tree_preferred: bool,
    tree_area: Rect,
    diff_area: Rect,
    focus: Focus,
    cursor: usize,
    scroll: usize,
    horizontal_scroll: usize,
    max_line_width: usize,
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
        let rows = build_rows(&diff, false);
        let tree = build_tree(&diff.files);
        let max_line_width = diff
            .files
            .iter()
            .flat_map(|file| file.hunks.iter())
            .flat_map(|hunk| hunk.lines.iter())
            .map(|line| crate::util::visual_width(&line.text))
            .max()
            .unwrap_or(0);
        let tree_cursor = tree
            .iter()
            .position(|node| node.file.is_some())
            .unwrap_or(0);
        Self {
            workspace_key,
            target,
            agent_terminal_ids,
            diff,
            rows,
            split: false,
            split_preferred: true,
            tree,
            tree_cursor,
            tree_scroll: 0,
            tree_height: 1,
            tree_shown: false,
            tree_preferred: true,
            tree_area: Rect::new(0, 0, 0, 0),
            diff_area: Rect::new(0, 0, 0, 0),
            focus: Focus::Diff,
            cursor: 0,
            scroll: 0,
            horizontal_scroll: 0,
            max_line_width,
            body_height: 1,
            comments: Vec::new(),
            search: String::new(),
            mode: InputMode::Normal,
        }
    }

    /// Rebuild the row list for the other layout, carrying the cursor to
    /// the row that covers the same source line.
    fn set_split(&mut self, split: bool) {
        if split == self.split {
            return;
        }
        let previous = std::mem::take(&mut self.rows);
        self.split = split;
        self.rows = build_rows(&self.diff, split);
        let anchor = previous
            .get(self.cursor..)
            .and_then(|rest| rest.iter().find_map(|row| row_anchor(*row)));
        self.cursor = anchor
            .and_then(|anchor| self.rows.iter().position(|row| row_covers(*row, anchor)))
            .unwrap_or_else(|| self.cursor.min(self.rows.len().saturating_sub(1)));
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

    /// The file the changes pane is parked in, so the tree can follow
    /// the cursor without a second source of truth.
    fn current_file(&self) -> Option<usize> {
        match *self.rows.get(self.cursor)? {
            RowKind::File(file)
            | RowKind::Header(file, _)
            | RowKind::Hunk(file, _)
            | RowKind::DiffLine(file, _, _)
            | RowKind::DiffPair(file, _, _, _) => Some(file),
            _ => None,
        }
    }

    fn jump_to_file(&mut self, file: usize) {
        if let Some(index) = self
            .rows
            .iter()
            .position(|row| matches!(row, RowKind::File(index) if *index == file))
        {
            self.cursor = index;
        }
    }

    /// Step the tree selection over `delta` **file** rows; directory
    /// rows are structure, never a selection target.
    fn move_tree_cursor(&mut self, delta: isize) {
        if self.tree.is_empty() || delta == 0 {
            return;
        }
        let step = if delta > 0 { 1isize } else { -1isize };
        let mut index = self.tree_cursor as isize;
        for _ in 0..delta.unsigned_abs() {
            let mut next = index + step;
            while next >= 0
                && (next as usize) < self.tree.len()
                && self.tree[next as usize].file.is_none()
            {
                next += step;
            }
            if next < 0 || next as usize >= self.tree.len() {
                break;
            }
            index = next;
        }
        self.select_tree_row(index as usize);
    }

    fn select_tree_row(&mut self, index: usize) {
        let Some(node) = self.tree.get(index) else {
            return;
        };
        let Some(file) = node.file else {
            return;
        };
        self.tree_cursor = index;
        self.jump_to_file(file);
        self.ensure_cursor_visible();
    }

    fn sync_tree_to_cursor(&mut self) {
        if let Some(file) = self.current_file()
            && let Some(index) = self.tree.iter().position(|node| node.file == Some(file))
        {
            self.tree_cursor = index;
        }
        self.ensure_tree_visible();
    }

    fn begin_comment(&mut self) {
        if matches!(
            self.rows.get(self.cursor),
            Some(RowKind::Hunk(..) | RowKind::DiffLine(..) | RowKind::DiffPair(..))
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
        let Some(anchor) = row_anchor(*row) else {
            return;
        };
        let file = &self.diff.files[anchor.file];
        let hunk = &file.hunks[anchor.hunk];
        let line = anchor.line.map(|index| &hunk.lines[index]);
        let (old_line, new_line) = match line {
            Some(line) => (line.old_line, line.new_line),
            None => (
                hunk.lines.iter().find_map(|line| line.old_line),
                hunk.lines.iter().find_map(|line| line.new_line),
            ),
        };
        let context = match anchor.line {
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
            anchor,
        });
        self.mode = InputMode::Normal;
    }

    fn remove_comments_at_cursor(&mut self) {
        let Some(row) = self.rows.get(self.cursor).copied() else {
            return;
        };
        self.comments
            .retain(|comment| !row_covers(row, comment.anchor));
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

    fn ensure_tree_visible(&mut self) {
        if self.tree_cursor < self.tree_scroll {
            self.tree_scroll = self.tree_cursor;
        } else if self.tree_cursor >= self.tree_scroll + self.tree_height {
            self.tree_scroll = self.tree_cursor + 1 - self.tree_height;
        }
        let max = self.tree.len().saturating_sub(self.tree_height);
        self.tree_scroll = self.tree_scroll.min(max);
    }

    /// Searchable text of a row — the diff content only, so a search for
    /// `10` finds the token and not every line number.
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
                Cow::Borrowed(&self.diff.files[file].hunks[hunk].lines[line].text)
            }
            RowKind::DiffPair(file, hunk, old, new) => {
                let lines = &self.diff.files[file].hunks[hunk].lines;
                match (old, new) {
                    (Some(old), Some(new)) if old == new => Cow::Borrowed(&lines[old].text),
                    (Some(old), Some(new)) => {
                        Cow::Owned(format!("{} {}", lines[old].text, lines[new].text))
                    }
                    (Some(index), None) | (None, Some(index)) => Cow::Borrowed(&lines[index].text),
                    (None, None) => Cow::Borrowed(""),
                }
            }
        }
    }

    fn line_visual(&self, file: usize, hunk: usize, line: usize) -> VisualKind {
        match self.diff.files[file].hunks[hunk].lines[line].kind {
            DiffLineKindDto::Context | DiffLineKindDto::Meta => VisualKind::Context,
            DiffLineKindDto::Addition => VisualKind::Addition,
            DiffLineKindDto::Deletion => VisualKind::Deletion,
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
            RowKind::DiffLine(file, hunk, line) => self.line_visual(file, hunk, line),
            RowKind::DiffPair(file, hunk, old, new) => match (new, old) {
                (Some(line), _) | (None, Some(line)) => self.line_visual(file, hunk, line),
                (None, None) => VisualKind::Dim,
            },
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

    fn source_label(&self) -> &'static str {
        match self.target {
            WorkspaceDiffTarget::Session(_) => "local worktree",
            WorkspaceDiffTarget::LinkedCheckout => "local checkout",
        }
    }

    fn render_tree(&self, frame: &mut Frame, area: Rect, theme: &crate::theme::Theme) {
        let end = (self.tree_scroll + area.height as usize).min(self.tree.len());
        let lines = self.tree[self.tree_scroll..end]
            .iter()
            .enumerate()
            .map(|(offset, node)| {
                let index = self.tree_scroll + offset;
                let selected = index == self.tree_cursor && node.file.is_some();
                let indent = "  ".repeat(node.depth);
                let counts = match node.file {
                    Some(_) => format!("  +{} -{}", node.added, node.removed),
                    None => String::new(),
                };
                let budget = area.width as usize;
                let label = crate::util::truncate_ellipsis(
                    &node.label,
                    budget.saturating_sub(indent.len() + crate::util::visual_width(&counts) + 1),
                )
                .into_owned();
                let mut style = Style::default().fg(match node.file {
                    Some(_) => theme.text_strong,
                    None => theme.text_dim,
                });
                if selected {
                    style = style.bg(theme.fill).fg(theme.accent);
                }
                let mut spans = vec![Span::styled(format!(" {indent}{label}"), style)];
                if !counts.is_empty() {
                    spans.push(Span::styled(counts, Style::default().fg(theme.text_dim)));
                }
                Line::from(spans)
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Cells a row leaves for the diff text itself once the marker and
    /// line-number gutters are paid for. Split halves it, which is what
    /// makes the horizontal-scroll clamp load-bearing rather than polish.
    fn text_width(&self, width: usize) -> usize {
        let rest = width.saturating_sub(2);
        if self.split {
            (rest.saturating_sub(1) / 2).saturating_sub(LINE_NUMBER_WIDTH + 1)
        } else {
            rest.saturating_sub(LINE_NUMBER_WIDTH * 2 + 4)
        }
    }

    /// Spans for one changes-pane row. `width` is the pane's usable
    /// width; the marker column is drawn here so horizontal scrolling
    /// can move the text without dragging the gutters off screen.
    fn row_line(&self, index: usize, width: usize, theme: &crate::theme::Theme) -> Line<'static> {
        let kind = self.rows[index];
        let selected = index == self.cursor && self.focus == Focus::Diff;
        let has_comment = self
            .comments
            .iter()
            .any(|comment| row_covers(kind, comment.anchor));
        let marker = match (index == self.cursor, has_comment) {
            (_, true) => "●",
            (true, false) => "›",
            (false, false) => " ",
        };
        let base = |visual: VisualKind| {
            let color = match visual {
                VisualKind::Dim => theme.text_dim,
                VisualKind::File | VisualKind::Hunk => theme.accent,
                VisualKind::Context => theme.text_strong,
                VisualKind::Addition => theme.success,
                VisualKind::Deletion => theme.error,
            };
            let mut style = Style::default().fg(color);
            if selected {
                style = style.bg(theme.fill);
            }
            if matches!(visual, VisualKind::File) {
                style = style.add_modifier(Modifier::BOLD);
            }
            style
        };
        let gutter = {
            let mut style = Style::default().fg(theme.text_dim);
            if selected {
                style = style.bg(theme.fill);
            }
            style
        };
        let mut spans = vec![Span::styled(format!("{marker} "), gutter)];
        let rest = width.saturating_sub(2);
        match kind {
            RowKind::DiffPair(file, hunk, old, new) => {
                let text_width = self.text_width(width);
                let lines = &self.diff.files[file].hunks[hunk].lines;
                for (slot, new_side) in [(old, false), (new, true)] {
                    if new_side {
                        spans.push(Span::styled("│", gutter));
                    }
                    let (number, text, visual) = match slot {
                        Some(index) => {
                            let line = &lines[index];
                            let number = if new_side {
                                line.new_line
                            } else {
                                line.old_line
                            };
                            (
                                number.map(|n| n.to_string()).unwrap_or_default(),
                                line.text.as_str(),
                                self.line_visual(file, hunk, index),
                            )
                        }
                        None => (String::new(), "", VisualKind::Dim),
                    };
                    spans.push(Span::styled(
                        format!("{number:>LINE_NUMBER_WIDTH$} "),
                        gutter,
                    ));
                    spans.push(Span::styled(
                        pad(
                            &window(text, self.horizontal_scroll, text_width),
                            text_width,
                        ),
                        base(visual),
                    ));
                }
            }
            RowKind::DiffLine(file, hunk, line) => {
                let entry = &self.diff.files[file].hunks[hunk].lines[line];
                let old = entry.old_line.map(|n| n.to_string()).unwrap_or_default();
                let new = entry.new_line.map(|n| n.to_string()).unwrap_or_default();
                spans.push(Span::styled(
                    format!("{old:>LINE_NUMBER_WIDTH$} {new:>LINE_NUMBER_WIDTH$} │ "),
                    gutter,
                ));
                let text_width = self.text_width(width);
                spans.push(Span::styled(
                    pad(
                        &window(&entry.text, self.horizontal_scroll, text_width),
                        text_width,
                    ),
                    base(self.row_visual(kind)),
                ));
            }
            _ => {
                let text = self.row_text(index).into_owned();
                spans.push(Span::styled(
                    pad(&window(&text, self.horizontal_scroll, rest), rest),
                    base(self.row_visual(kind)),
                ));
            }
        }
        Line::from(spans)
    }
}

impl Component for DiffReview {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal = centered_rect(
            area,
            area.width.saturating_sub(4),
            area.height.saturating_sub(2),
        );
        let (added, removed) = totals(&self.diff.files);
        let title = format!(
            " Review · {} · {} file{} · +{added} -{removed} ",
            self.source_label(),
            self.diff.files.len(),
            if self.diff.files.len() == 1 { "" } else { "s" }
        );
        let inner = draw_frame(frame, modal, &title, theme);
        if inner.height < 3 || inner.width < 3 {
            return;
        }

        let body_height = inner.height.saturating_sub(2) as usize;
        self.body_height = body_height.max(1);
        self.tree_height = self.body_height;

        let tree_width = if self.tree_preferred && inner.width >= MIN_TREE_TOTAL {
            (inner.width / 4).clamp(22, 40)
        } else {
            0
        };
        self.tree_shown = tree_width > 0;
        if !self.tree_shown && self.focus == Focus::Tree {
            self.focus = Focus::Diff;
        }
        let diff_x = inner.x + tree_width + u16::from(self.tree_shown);
        let diff_width = inner
            .width
            .saturating_sub(tree_width + u16::from(self.tree_shown));
        self.set_split(self.split_preferred && diff_width >= MIN_SPLIT_WIDTH);
        self.ensure_cursor_visible();
        self.sync_tree_to_cursor();

        self.tree_area = Rect::new(inner.x, inner.y, tree_width, body_height as u16);
        self.diff_area = Rect::new(
            diff_x,
            inner.y,
            diff_width.saturating_sub(1),
            body_height as u16,
        );
        let gutter = Rect::new(
            diff_x + diff_width.saturating_sub(1),
            inner.y,
            1,
            body_height as u16,
        );
        let input_area = Rect::new(inner.x, inner.y + body_height as u16, inner.width, 1);
        let hint_area = Rect::new(inner.x, input_area.y + 1, inner.width, 1);

        if self.tree_shown {
            self.render_tree(frame, self.tree_area, theme);
            let divider = Rect::new(inner.x + tree_width, inner.y, 1, body_height as u16);
            frame.render_widget(
                Paragraph::new(
                    std::iter::repeat_n(
                        Line::from(Span::styled("│", Style::default().fg(theme.chrome))),
                        body_height,
                    )
                    .collect::<Vec<_>>(),
                ),
                divider,
            );
        }

        let end = (self.scroll + body_height).min(self.rows.len());
        let width = self.diff_area.width as usize;
        self.horizontal_scroll = self
            .horizontal_scroll
            .min(self.max_line_width.saturating_sub(self.text_width(width)));
        let lines = (self.scroll..end)
            .map(|index| self.row_line(index, width, theme))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), self.diff_area);
        scrollbar::render_vertical(frame, gutter, self.rows.len(), body_height, self.scroll);

        let input = match &self.mode {
            InputMode::Normal if self.comments.is_empty() => {
                "c comment · / search · [/] hunks · {/} files · h/l scroll".to_string()
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
        let layout = if self.split {
            "s unified"
        } else if self.split_preferred {
            "split needs a wider pane"
        } else {
            "s side-by-side"
        };
        let tree_hint = match (self.tree_shown, self.focus) {
            (false, _) => "t show tree",
            (true, Focus::Tree) => "Tab focus changes · t hide tree",
            (true, Focus::Diff) => "Tab focus tree · t hide tree",
        };
        let hint =
            format!("j/k · PgUp/PgDn navigate · {tree_hint} · {layout} · n/N search · Esc close");
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                crate::util::truncate_ellipsis(&hint, hint_area.width as usize).into_owned(),
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
                let tree = self.focus == Focus::Tree;
                match key.code {
                    Key::Tab => {
                        self.focus = match (self.focus, self.tree_shown) {
                            (Focus::Diff, true) => Focus::Tree,
                            _ => Focus::Diff,
                        };
                    }
                    Key::Char('t') if !ctrl => {
                        self.tree_preferred = !self.tree_preferred;
                        if !self.tree_preferred {
                            self.focus = Focus::Diff;
                        }
                    }
                    Key::Char('s') if !ctrl => self.split_preferred = !self.split_preferred,
                    Key::Down | Key::Char('j') if tree => self.move_tree_cursor(1),
                    Key::Up | Key::Char('k') if tree => self.move_tree_cursor(-1),
                    Key::Enter | Key::Right | Key::Char('l') if tree => self.focus = Focus::Diff,
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
                self.sync_tree_to_cursor();
                None
            }
            Event::Mouse(mouse) => {
                let over_tree = self.tree_shown && mouse.column < self.tree_area.right();
                match mouse.kind {
                    MouseEventKind::ScrollDown if over_tree => self.move_tree_cursor(1),
                    MouseEventKind::ScrollUp if over_tree => self.move_tree_cursor(-1),
                    MouseEventKind::ScrollDown => self.move_cursor(WHEEL_STEP as isize),
                    MouseEventKind::ScrollUp => self.move_cursor(-(WHEEL_STEP as isize)),
                    MouseEventKind::Down(MouseButton::Left) if over_tree => {
                        let row =
                            self.tree_scroll + mouse.row.saturating_sub(self.tree_area.y) as usize;
                        self.focus = Focus::Tree;
                        self.select_tree_row(row);
                    }
                    _ => {}
                }
                self.ensure_cursor_visible();
                self.sync_tree_to_cursor();
                None
            }
            _ => None,
        }
    }
}

/// The source lines a row stands for, for comment anchoring and for
/// carrying the cursor across a layout toggle.
fn row_anchor(kind: RowKind) -> Option<CommentAnchor> {
    match kind {
        RowKind::Hunk(file, hunk) => Some(CommentAnchor {
            file,
            hunk,
            line: None,
        }),
        RowKind::DiffLine(file, hunk, line) => Some(CommentAnchor {
            file,
            hunk,
            line: Some(line),
        }),
        RowKind::DiffPair(file, hunk, old, new) => new.or(old).map(|line| CommentAnchor {
            file,
            hunk,
            line: Some(line),
        }),
        _ => None,
    }
}

fn row_covers(kind: RowKind, anchor: CommentAnchor) -> bool {
    match kind {
        RowKind::Hunk(file, hunk) => (file, hunk, anchor.line) == (anchor.file, anchor.hunk, None),
        RowKind::DiffLine(file, hunk, line) => {
            (file, hunk, Some(line)) == (anchor.file, anchor.hunk, anchor.line)
        }
        RowKind::DiffPair(file, hunk, old, new) => {
            file == anchor.file
                && hunk == anchor.hunk
                && anchor.line.is_some()
                && (old == anchor.line || new == anchor.line)
        }
        _ => false,
    }
}

fn totals(files: &[DiffFileDto]) -> (usize, usize) {
    files.iter().fold((0, 0), |(added, removed), file| {
        let (a, r) = file_totals(file);
        (added + a, removed + r)
    })
}

fn file_totals(file: &DiffFileDto) -> (usize, usize) {
    file.hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .fold((0, 0), |(added, removed), line| match line.kind {
            DiffLineKindDto::Addition => (added + 1, removed),
            DiffLineKindDto::Deletion => (added, removed + 1),
            _ => (added, removed),
        })
}

/// The `width` cells of `text` starting `offset` cells in, padded by the
/// caller. Graphemes straddling either edge are dropped rather than
/// split across a cell boundary.
fn window(text: &str, offset: usize, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut skipped = 0usize;
    let mut used = 0usize;
    for grapheme in crate::util::graphemes(text) {
        let cells = crate::util::visual_width(grapheme);
        if skipped < offset {
            skipped += cells;
            continue;
        }
        if used + cells > width {
            break;
        }
        out.push_str(grapheme);
        used += cells;
    }
    out
}

fn pad(text: &str, width: usize) -> String {
    let used = crate::util::visual_width(text);
    let mut out = text.to_string();
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    out
}

#[derive(Default)]
struct DirNode {
    dirs: BTreeMap<String, DirNode>,
    files: BTreeMap<String, usize>,
}

fn build_tree(files: &[DiffFileDto]) -> Vec<TreeNode> {
    let mut root = DirNode::default();
    for (index, file) in files.iter().enumerate() {
        let mut node = &mut root;
        let mut parts = file.path.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_some() {
                node = node.dirs.entry(part.to_string()).or_default();
            } else {
                node.files.insert(part.to_string(), index);
            }
        }
    }
    let mut out = Vec::new();
    flatten_tree(&root, 0, files, &mut out);
    out
}

fn flatten_tree(node: &DirNode, depth: usize, files: &[DiffFileDto], out: &mut Vec<TreeNode>) {
    for (name, child) in &node.dirs {
        // A directory that only ever contains one directory is folded
        // into its child's label, so a deep source tree doesn't spend
        // half the pane on indentation.
        let mut label = name.clone();
        let mut deepest = child;
        while deepest.files.is_empty() && deepest.dirs.len() == 1 {
            let Some((name, only)) = deepest.dirs.iter().next() else {
                break;
            };
            label.push('/');
            label.push_str(name);
            deepest = only;
        }
        out.push(TreeNode {
            depth,
            label: format!("{label}/"),
            file: None,
            added: 0,
            removed: 0,
        });
        flatten_tree(deepest, depth + 1, files, out);
    }
    for (name, index) in &node.files {
        let (added, removed) = file_totals(&files[*index]);
        out.push(TreeNode {
            depth,
            label: name.clone(),
            file: Some(*index),
            added,
            removed,
        });
    }
}

fn build_rows(diff: &WorkspaceDiffDto, split: bool) -> Vec<RowKind> {
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
            if split {
                push_paired_lines(file_index, hunk_index, &hunk.lines, &mut rows);
            } else {
                for line_index in 0..hunk.lines.len() {
                    rows.push(RowKind::DiffLine(file_index, hunk_index, line_index));
                }
            }
        }
    }
    rows
}

/// Lay a hunk's lines out in two columns: a run of deletions is matched
/// positionally against the run of additions that replaced it, and the
/// longer run spills into rows with one side empty.
fn push_paired_lines(
    file: usize,
    hunk: usize,
    lines: &[lazybox_ipc::DiffLineDto],
    rows: &mut Vec<RowKind>,
) {
    let mut removed: Vec<usize> = Vec::new();
    let mut added: Vec<usize> = Vec::new();
    let flush = |removed: &mut Vec<usize>, added: &mut Vec<usize>, rows: &mut Vec<RowKind>| {
        for index in 0..removed.len().max(added.len()) {
            rows.push(RowKind::DiffPair(
                file,
                hunk,
                removed.get(index).copied(),
                added.get(index).copied(),
            ));
        }
        removed.clear();
        added.clear();
    };
    for (index, line) in lines.iter().enumerate() {
        match line.kind {
            DiffLineKindDto::Deletion => {
                if !added.is_empty() {
                    flush(&mut removed, &mut added, rows);
                }
                removed.push(index);
            }
            DiffLineKindDto::Addition => added.push(index),
            DiffLineKindDto::Context => {
                flush(&mut removed, &mut added, rows);
                rows.push(RowKind::DiffPair(file, hunk, Some(index), Some(index)));
            }
            // `\ No newline at end of file` belongs to whichever side
            // it trails; the old side is where git prints it.
            DiffLineKindDto::Meta => {
                flush(&mut removed, &mut added, rows);
                rows.push(RowKind::DiffPair(file, hunk, Some(index), None));
            }
        }
    }
    flush(&mut removed, &mut added, rows);
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

    fn file(path: &str, lines: Vec<DiffLineDto>) -> DiffFileDto {
        DiffFileDto {
            old_path: Some(path.into()),
            path: path.into(),
            headers: vec![format!("diff --git a/{path} b/{path}")],
            hunks: vec![DiffHunkDto {
                header: format!("@@ -1,1 +1,1 @@ {path}"),
                old_start: 1,
                new_start: 1,
                lines,
            }],
        }
    }

    fn line(kind: DiffLineKindDto, text: &str, old: Option<u32>, new: Option<u32>) -> DiffLineDto {
        DiffLineDto {
            kind,
            text: text.into(),
            old_line: old,
            new_line: new,
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

    fn render_sized(review: &mut DiffReview, width: u16, height: u16) -> String {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| review.view(frame, Rect::new(0, 0, width, height)))
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

    fn render(review: &mut DiffReview) -> String {
        render_sized(review, 120, 24)
    }

    #[test]
    fn renders_status_colored_diff_and_navigation_help() {
        let mut review = review(sample());
        let output = render(&mut review);
        assert!(output.contains("Review · local worktree · 1 file · +1 -0"));
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
        for character in "rename".chars() {
            review.on(&key(Key::Char(character)));
        }
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

        for _ in 0..50 {
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
            lines: vec![line(
                DiffLineKindDto::Deletion,
                "-remove();",
                Some(10),
                None,
            )],
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

    #[test]
    fn file_tree_folds_single_child_directories_and_carries_change_counts() {
        let diff = WorkspaceDiffDto {
            status: Vec::new(),
            stat: Vec::new(),
            truncated: false,
            files: vec![
                file(
                    "crates/tui/src/a.rs",
                    vec![line(DiffLineKindDto::Addition, "+a", None, Some(1))],
                ),
                file(
                    "crates/tui/src/b.rs",
                    vec![
                        line(DiffLineKindDto::Deletion, "-b", Some(1), None),
                        line(DiffLineKindDto::Addition, "+b2", None, Some(1)),
                    ],
                ),
                file(
                    "README.md",
                    vec![line(DiffLineKindDto::Addition, "+doc", None, Some(1))],
                ),
            ],
        };
        let tree = build_tree(&diff.files);

        assert_eq!(
            tree.iter()
                .map(|node| (node.depth, node.label.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (0, "crates/tui/src/"),
                (1, "a.rs"),
                (1, "b.rs"),
                (0, "README.md"),
            ]
        );
        let b = tree.iter().find(|node| node.label == "b.rs").expect("b.rs");
        assert_eq!((b.added, b.removed), (1, 1));

        let mut review = review(diff);
        let output = render(&mut review);
        assert!(output.contains("crates/tui/src/"), "{output}");
        assert!(output.contains("+1 -1"), "{output}");
    }

    #[test]
    fn side_by_side_pairs_a_deletion_with_the_addition_that_replaced_it() {
        let diff = WorkspaceDiffDto {
            status: Vec::new(),
            stat: Vec::new(),
            truncated: false,
            files: vec![file(
                "src/lib.rs",
                vec![
                    line(DiffLineKindDto::Context, " keep();", Some(1), Some(1)),
                    line(DiffLineKindDto::Deletion, "-old();", Some(2), None),
                    line(DiffLineKindDto::Addition, "+new();", None, Some(2)),
                    line(DiffLineKindDto::Addition, "+extra();", None, Some(3)),
                ],
            )],
        };
        let rows = build_rows(&diff, true);
        let pairs = rows
            .iter()
            .filter_map(|row| match row {
                RowKind::DiffPair(_, _, old, new) => Some((*old, *new)),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            pairs,
            vec![(Some(0), Some(0)), (Some(1), Some(2)), (None, Some(3))]
        );

        let mut review = review(diff);
        let output = render_sized(&mut review, 120, 24);
        let replacement = output
            .lines()
            .find(|line| line.contains("-old();"))
            .expect("replacement row");
        assert!(
            replacement.contains("+new();"),
            "deletion and its replacement share a row: {replacement}"
        );
    }

    #[test]
    fn a_narrow_changes_pane_falls_back_to_unified() {
        let mut review = review(sample());
        render_sized(&mut review, 120, 24);
        assert!(review.split, "a wide modal splits");

        let output = render_sized(&mut review, 60, 24);
        assert!(!review.split, "a narrow modal falls back to unified");
        assert!(output.contains("+fix();"), "{output}");
    }

    #[test]
    fn toggling_the_layout_keeps_the_cursor_on_the_same_source_line() {
        let mut review = review(sample());
        render(&mut review);
        assert!(review.split);
        review.cursor = (0..review.rows.len())
            .position(|index| review.row_text(index).contains("+fix();"))
            .expect("addition row");

        review.on(&key(Key::Char('s')));
        render(&mut review);

        assert!(!review.split);
        assert!(review.row_text(review.cursor).contains("+fix();"));
    }

    #[test]
    fn a_drafted_comment_stays_on_its_line_across_a_layout_toggle() {
        let mut review = review(sample());
        render(&mut review);
        review.cursor = (0..review.rows.len())
            .position(|index| review.row_text(index).contains("+fix();"))
            .expect("addition row");
        review.mode = InputMode::Comment("rename".into());
        review.save_comment();

        review.on(&key(Key::Char('s')));
        let output = render(&mut review);

        assert_eq!(review.comments.len(), 1);
        let marked = output
            .lines()
            .find(|line| line.contains("+fix();"))
            .expect("addition row");
        assert!(
            marked.contains('●'),
            "comment marker follows the line: {marked}"
        );
    }

    #[test]
    fn the_tree_selection_moves_the_changes_pane_to_that_file() {
        let diff = WorkspaceDiffDto {
            status: Vec::new(),
            stat: Vec::new(),
            truncated: false,
            files: vec![
                file(
                    "src/a.rs",
                    vec![line(DiffLineKindDto::Addition, "+a", None, Some(1))],
                ),
                file(
                    "src/b.rs",
                    vec![line(DiffLineKindDto::Addition, "+b", None, Some(1))],
                ),
            ],
        };
        let mut review = review(diff);
        render(&mut review);
        assert!(review.tree_shown);

        review.on(&key(Key::Tab));
        assert_eq!(review.focus, Focus::Tree);
        review.on(&key(Key::Char('j')));

        assert_eq!(review.current_file(), Some(1));
        assert_eq!(review.tree[review.tree_cursor].label, "b.rs");

        review.on(&key(Key::Char('l')));
        assert_eq!(review.focus, Focus::Diff);
    }

    #[test]
    fn hiding_the_tree_returns_focus_and_width_to_the_changes_pane() {
        let mut review = review(sample());
        render(&mut review);
        let split_width = review.diff_area.width;

        review.on(&key(Key::Char('t')));
        render(&mut review);

        assert!(!review.tree_shown);
        assert_eq!(review.focus, Focus::Diff);
        assert!(review.diff_area.width > split_width);
    }
}
