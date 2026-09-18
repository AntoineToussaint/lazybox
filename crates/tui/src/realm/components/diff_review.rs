//! The review modal behind `g v` — a near-full-screen viewer with a file
//! tree on the left and the changes on the right, side by side where the
//! pane is wide enough and unified where it is not.
//!
//! Two sources feed it, and `p` switches between them. The pull
//! request's diff is what reviewers see and the only document a GitHub
//! comment can anchor to; the local checkout is the only diff that
//! exists before a branch is pushed. They are different documents — the
//! PR carries other people's commits and none of your unpushed work —
//! so the header names the one on screen and says how far the checkout
//! has drifted from it.

use crate::components::scrollbar;
use crate::realm::components::scrollable::{centered_rect, draw_frame};
use crate::realm::{Msg, UserEvent};
use lazybox_core::WorkspaceKey;
use lazybox_ipc::{
    CommitComparisonDto, DiffFileDto, DiffLineKindDto, DiffSideDto, ReviewCommentDto,
    ReviewVerdictDto, TerminalId, WorkspaceDiffDivergenceDto, WorkspaceDiffDto,
    WorkspaceDiffTarget,
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
    /// The one-line notice that the checkout no longer matches the pull
    /// request on screen. Present only when it has something to say.
    Divergence,
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
    /// A statement about the diff rather than a line of it: truncation,
    /// divergence. Reserved for facts that change how the diff should
    /// be read.
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tree,
    Diff,
}

/// Which half of a side-by-side row the cursor addresses. The two
/// halves are independently commentable source lines, so a row alone
/// does not identify what `c` annotates or what `x` clears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputMode {
    Normal,
    Search(String),
    Comment(String),
    /// Composing the body of a GitHub review. Required: GitHub refuses
    /// a `COMMENT` or `REQUEST_CHANGES` review without one.
    ReviewSummary(String),
    /// Choosing what the review says about the PR as a whole. The
    /// keypress that picks a verdict is also the confirmation — posting
    /// to GitHub is public and must not ride a single stray `S`.
    ReviewVerdict(String),
    /// The review is with the daemon and GitHub has not answered yet.
    /// The viewer stays mounted through this: a 422 on a stale
    /// `commit_id`, a 403, a 502 — any refusal — must leave every
    /// drafted comment exactly where the reviewer left it, because
    /// there is nowhere else they exist.
    Submitting,
}

/// Attribute the model sets to release the viewer from its in-flight
/// state when GitHub refused the review, so the drafted comments
/// become editable and re-submittable.
pub const REVIEW_IN_FLIGHT: &str = "review-in-flight";

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
    /// The row list for the layout that is not currently active, kept so
    /// a width change across the split threshold does not rebuild every
    /// row of a large diff inside the render path.
    idle_rows: Option<Vec<RowKind>>,
    split: bool,
    split_preferred: bool,
    tree: Vec<TreeNode>,
    tree_scroll: usize,
    tree_height: usize,
    tree_shown: bool,
    tree_preferred: bool,
    tree_area: Rect,
    diff_area: Rect,
    focus: Focus,
    cursor: usize,
    side: Side,
    scroll: usize,
    horizontal_scroll: usize,
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
        let pull_request = matches!(target, WorkspaceDiffTarget::PullRequest);
        let rows = build_rows(&diff, false, pull_request);
        let tree = build_tree(&diff.files);
        Self {
            workspace_key,
            target,
            agent_terminal_ids,
            diff,
            rows,
            idle_rows: None,
            split: false,
            split_preferred: true,
            tree,
            tree_scroll: 0,
            tree_height: 1,
            tree_shown: false,
            tree_preferred: true,
            tree_area: Rect::new(0, 0, 0, 0),
            diff_area: Rect::new(0, 0, 0, 0),
            focus: Focus::Diff,
            cursor: 0,
            side: Side::New,
            scroll: 0,
            horizontal_scroll: 0,
            body_height: 1,
            comments: Vec::new(),
            search: String::new(),
            mode: InputMode::Normal,
        }
    }

    /// Swap in the other layout's row list, carrying the cursor to the
    /// row standing for the same place in the diff.
    ///
    /// The two layouts differ only inside hunks, so every other row kind
    /// is either unique (`File` / `Header` / `Hunk`) or sits at the same
    /// index in both. Scanning forward to the first row that can be
    /// located exactly and subtracting the steps taken therefore lands
    /// on the same row rather than dragging the cursor down into the
    /// first hunk, which a forward scan for a *line* row would do.
    fn set_split(&mut self, split: bool) {
        if split == self.split {
            return;
        }
        let next = self
            .idle_rows
            .take()
            .unwrap_or_else(|| build_rows(&self.diff, split, self.is_pull_request()));
        let previous = std::mem::replace(&mut self.rows, next);
        self.split = split;
        self.cursor = previous[self.cursor..]
            .iter()
            .enumerate()
            .find_map(|(steps, row)| Some((steps, self.locate(*row)?)))
            .map(|(steps, index)| index.saturating_sub(steps))
            .unwrap_or_else(|| self.cursor.min(self.rows.len().saturating_sub(1)));
        self.idle_rows = Some(previous);
    }

    /// The index in the active row list of the row standing for `row`,
    /// when `row` is one the two layouts agree on or a line row whose
    /// source line the active layout also shows.
    fn locate(&self, row: RowKind) -> Option<usize> {
        match row {
            RowKind::File(..) | RowKind::Header(..) | RowKind::Hunk(..) => {
                self.rows.iter().position(|candidate| *candidate == row)
            }
            RowKind::DiffLine(..) | RowKind::DiffPair(..) => {
                let anchor = row_anchor(row)?;
                self.rows
                    .iter()
                    .position(|candidate| row_covers(*candidate, anchor))
            }
            _ => None,
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

    /// The file the changes pane is parked in, so the tree can follow
    /// the cursor without a second source of truth.
    fn current_file(&self) -> Option<usize> {
        row_file(*self.rows.get(self.cursor)?)
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

    /// The highlighted tree row, **derived** from the file the changes
    /// pane is parked in. Storing it separately let the two panes drift
    /// apart whenever the diff cursor sat on a row that belongs to no
    /// file (the status/stat header block), because the sync step had
    /// nothing to write and silently left the old highlight standing.
    fn selected_tree_row(&self) -> Option<usize> {
        let file = self.tree_file()?;
        self.tree.iter().position(|node| node.file == Some(file))
    }

    /// The file the tree points at: the cursor's own, or — when the
    /// cursor sits on a row belonging to no file, such as the status
    /// block or a spacer — the nearest one it would reach, looking
    /// forward first. Total in the cursor, so the highlight cannot
    /// survive the pane moving away from it.
    fn tree_file(&self) -> Option<usize> {
        self.current_file().or_else(|| {
            self.rows[self.cursor..]
                .iter()
                .chain(self.rows[..self.cursor].iter().rev())
                .find_map(|row| row_file(*row))
        })
    }

    fn first_tree_file(&self) -> Option<usize> {
        self.tree.iter().position(|node| node.file.is_some())
    }

    /// Step the tree selection over `delta` **file** rows; directory
    /// rows are structure, never a selection target.
    fn move_tree_cursor(&mut self, delta: isize) {
        if self.tree.is_empty() || delta == 0 {
            return;
        }
        // No file is on screen (the cursor is up in the status block),
        // so there is no row to step from — enter the tree at its first
        // file instead of stepping from a stale highlight.
        let Some(base) = self.selected_tree_row() else {
            if let Some(index) = self.first_tree_file() {
                self.select_tree_row(index);
            }
            return;
        };
        let step = if delta > 0 { 1isize } else { -1isize };
        let mut index = base as isize;
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
        self.jump_to_file(file);
        self.ensure_cursor_visible();
        self.ensure_tree_visible();
    }

    /// The half of `row` the cursor actually addresses: the preferred
    /// side when that row has one, otherwise the side it does have.
    fn effective_side(&self, row: RowKind) -> Side {
        let RowKind::DiffPair(_, _, old, new) = row else {
            return self.side;
        };
        match self.side {
            Side::Old if old.is_none() => Side::New,
            Side::New if new.is_none() => Side::Old,
            side => side,
        }
    }

    /// The exact source line the cursor addresses — the one `c`
    /// annotates and `x` clears. A side-by-side row shows up to two
    /// independent lines, so the row alone does not identify it.
    fn cursor_anchor(&self) -> Option<CommentAnchor> {
        let row = *self.rows.get(self.cursor)?;
        let RowKind::DiffPair(file, hunk, old, new) = row else {
            return row_anchor(row);
        };
        let line = match self.effective_side(row) {
            Side::Old => old,
            Side::New => new,
        }?;
        Some(CommentAnchor {
            file,
            hunk,
            line: Some(line),
        })
    }

    /// Move the cursor between the halves of a side-by-side row.
    /// Returns whether it moved — when it did not, the caller scrolls
    /// horizontally instead, so the outer edge of each half continues
    /// into the text rather than dead-ending.
    fn move_side(&mut self, forward: bool) -> bool {
        let Some(RowKind::DiffPair(_, _, Some(_), Some(_))) = self.rows.get(self.cursor).copied()
        else {
            return false;
        };
        let target = if forward { Side::New } else { Side::Old };
        if self.side == target {
            return false;
        }
        self.side = target;
        true
    }

    fn has_comment_on(&self, file: usize, hunk: usize, line: usize) -> bool {
        self.comments.iter().any(|comment| {
            comment.anchor
                == CommentAnchor {
                    file,
                    hunk,
                    line: Some(line),
                }
        })
    }

    /// The source-line numbers an anchor resolves to. A hunk-header
    /// anchor borrows the first numbered line of its hunk.
    fn anchor_lines(&self, anchor: CommentAnchor) -> (Option<u32>, Option<u32>) {
        let hunk = &self.diff.files[anchor.file].hunks[anchor.hunk];
        match anchor.line.map(|index| &hunk.lines[index]) {
            Some(line) => (line.old_line, line.new_line),
            None => (
                hunk.lines.iter().find_map(|line| line.old_line),
                hunk.lines.iter().find_map(|line| line.new_line),
            ),
        }
    }

    /// Where GitHub would hang a comment written at `anchor`: a line
    /// number in the pull request's diff and the side it sits on. An
    /// addition exists only on the right, a deletion only on the left,
    /// and a context line on both — where the right wins, because the
    /// post-image is the file as merged and what GitHub's own UI
    /// anchors a comment on a context line to.
    ///
    /// `None` for a line GitHub cannot address at all — the `\ No
    /// newline at end of file` marker carries no number on either side.
    fn github_anchor(&self, anchor: CommentAnchor) -> Option<(u32, DiffSideDto)> {
        match self.anchor_lines(anchor) {
            (_, Some(new)) => Some((new, DiffSideDto::Right)),
            (Some(old), None) => Some((old, DiffSideDto::Left)),
            (None, None) => None,
        }
    }

    /// Open the comment input only where a comment can actually land.
    /// Sharing the predicate with `save_comment` is what stops an input
    /// opening on a row whose save would silently drop what was typed.
    ///
    /// On the pull-request source the bar is higher: a line GitHub
    /// cannot address is one the review would have to drop at submit
    /// time, and refusing at the input is the only refusal that costs
    /// the reviewer nothing.
    fn begin_comment(&mut self) {
        let Some(anchor) = self.cursor_anchor() else {
            return;
        };
        if self.is_pull_request() && self.github_anchor(anchor).is_none() {
            return;
        }
        self.mode = InputMode::Comment(String::new());
    }

    /// The drafted comments as GitHub review comments. Every one of
    /// them anchors — `begin_comment` refuses the rows that would not.
    fn review_comments(&self) -> Vec<ReviewCommentDto> {
        self.comments
            .iter()
            .filter_map(|comment| {
                let (line, side) = self.github_anchor(comment.anchor)?;
                Some(ReviewCommentDto {
                    path: comment.path.clone(),
                    line,
                    side,
                    body: comment.body.clone(),
                })
            })
            .collect()
    }

    fn save_comment(&mut self) {
        let InputMode::Comment(input) = &self.mode else {
            return;
        };
        let body = input.trim();
        if body.is_empty() {
            return;
        }
        let Some(anchor) = self.cursor_anchor() else {
            return;
        };
        let (old_line, new_line) = self.anchor_lines(anchor);
        let file = &self.diff.files[anchor.file];
        let hunk = &file.hunks[anchor.hunk];
        let line = anchor.line.map(|index| &hunk.lines[index]);
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

    /// Clear the comments on the line the cursor addresses — not every
    /// line the row happens to display. A side-by-side row covers a
    /// deletion *and* the addition that replaced it, so clearing by row
    /// threw away a second, independently written comment with no
    /// prompt and no way to get it back.
    fn remove_comments_at_cursor(&mut self) {
        let Some(anchor) = self.cursor_anchor() else {
            return;
        };
        self.comments.retain(|comment| comment.anchor != anchor);
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
        if let Some(selected) = self.selected_tree_row() {
            if selected < self.tree_scroll {
                self.tree_scroll = selected;
            } else if selected >= self.tree_scroll + self.tree_height {
                self.tree_scroll = selected + 1 - self.tree_height;
            }
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
            RowKind::Divergence => {
                Cow::Owned(divergence_notice(self.diff.divergence.as_ref()).unwrap_or_default())
            }
            RowKind::Truncated if self.is_pull_request() => Cow::Borrowed(
                "this pull request is too large to read in full; the rest is on GitHub",
            ),
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
                Cow::Owned(numbered(line.old_line, line.new_line, &line.text))
            }
            RowKind::DiffPair(file, hunk, old, new) => {
                let lines = &self.diff.files[file].hunks[hunk].lines;
                match (old, new) {
                    (Some(old), Some(new)) if old == new => Cow::Owned(numbered(
                        lines[old].old_line,
                        lines[new].new_line,
                        &lines[old].text,
                    )),
                    (Some(old), Some(new)) => Cow::Owned(format!(
                        "{} {}",
                        numbered(lines[old].old_line, None, &lines[old].text),
                        numbered(None, lines[new].new_line, &lines[new].text),
                    )),
                    (Some(index), None) => {
                        Cow::Owned(numbered(lines[index].old_line, None, &lines[index].text))
                    }
                    (None, Some(index)) => {
                        Cow::Owned(numbered(None, lines[index].new_line, &lines[index].text))
                    }
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
            RowKind::Divergence | RowKind::Truncated => VisualKind::Warning,
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

    /// Drive the open prompt. Returns whether the event belonged to it
    /// — and, when a verdict was picked, the review to submit, because
    /// that is the one prompt whose completion leaves the component.
    fn handle_input(&mut self, event: &Event<UserEvent>) -> (bool, Option<Msg>) {
        if matches!(self.mode, InputMode::Normal) {
            return (false, None);
        }
        if let Event::Paste(text) = event {
            let input = match &mut self.mode {
                InputMode::Search(input)
                | InputMode::Comment(input)
                | InputMode::ReviewSummary(input) => input,
                InputMode::Normal | InputMode::ReviewVerdict(_) | InputMode::Submitting => {
                    return (true, None);
                }
            };
            input.extend(text.chars().filter(|character| !character.is_control()));
            return (true, None);
        }
        let Event::Keyboard(key) = event else {
            return (true, None);
        };
        // The verdict prompt is a choice, not a field: every key means
        // something other than "type that character".
        if let InputMode::ReviewVerdict(summary) = &self.mode {
            let verdict = match key.code {
                Key::Char('c') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(ReviewVerdictDto::Comment)
                }
                Key::Char('a') => Some(ReviewVerdictDto::Approve),
                Key::Char('r') => Some(ReviewVerdictDto::RequestChanges),
                _ => None,
            };
            let Some(verdict) = verdict else {
                if matches!(key.code, Key::Esc) {
                    self.mode = InputMode::Normal;
                }
                return (true, None);
            };
            let summary = summary.trim().to_string();
            let message = self.review_head().map(|head_sha| Msg::DiffReviewPosted {
                workspace_key: self.workspace_key.clone(),
                head_sha: head_sha.to_string(),
                summary,
                verdict,
                comments: self.review_comments(),
            });
            // Hold the comments — and the viewer — until GitHub
            // answers. Dropping them here is what turned a refused
            // review into an unrecoverable loss of everything typed.
            self.mode = match message {
                Some(_) => InputMode::Submitting,
                None => InputMode::Normal,
            };
            return (true, message);
        }
        // A submit already in flight owns the viewer until the daemon
        // answers — dropping back to Normal would let the next
        // `Shift-S` post the same review a second time. Esc still
        // leaves, so a daemon that never replies cannot strand the
        // reviewer in a modal that accepts no key at all; that exit
        // discards the comments, but only because they asked it to.
        if matches!(self.mode, InputMode::Submitting) {
            let leaving = matches!(key.code, Key::Esc)
                || (matches!(key.code, Key::Char('c'))
                    && key.modifiers.contains(KeyModifiers::CONTROL));
            return (true, leaving.then_some(Msg::ModalDismissed));
        }
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
                // An empty body is not a review GitHub will take, so
                // Enter on one holds the prompt open rather than
                // advancing to a verdict that cannot be submitted.
                InputMode::ReviewSummary(summary) if !summary.trim().is_empty() => {
                    self.mode = InputMode::ReviewVerdict(summary.clone());
                }
                InputMode::ReviewSummary(_)
                | InputMode::Normal
                | InputMode::ReviewVerdict(_)
                | InputMode::Submitting => {}
            },
            Key::Backspace => match &mut self.mode {
                InputMode::Search(input)
                | InputMode::Comment(input)
                | InputMode::ReviewSummary(input) => {
                    input.pop();
                }
                InputMode::Normal | InputMode::ReviewVerdict(_) | InputMode::Submitting => {}
            },
            Key::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                match &mut self.mode {
                    InputMode::Search(input)
                    | InputMode::Comment(input)
                    | InputMode::ReviewSummary(input) => {
                        input.push(character);
                    }
                    InputMode::Normal | InputMode::ReviewVerdict(_) | InputMode::Submitting => {}
                }
            }
            _ => {}
        }
        (true, None)
    }

    fn source_label(&self) -> &'static str {
        match self.target {
            WorkspaceDiffTarget::Session(_) => "local worktree",
            WorkspaceDiffTarget::LinkedCheckout => "local checkout",
            WorkspaceDiffTarget::PullRequest => "pull request",
        }
    }

    fn is_pull_request(&self) -> bool {
        matches!(self.target, WorkspaceDiffTarget::PullRequest)
    }

    /// The commit a review raised here pins its comments to. Always
    /// present on a pull-request diff — it is what the daemon read the
    /// diff at.
    fn review_head(&self) -> Option<&str> {
        self.diff.head_sha.as_deref()
    }

    fn render_tree(&self, frame: &mut Frame, area: Rect, theme: &crate::theme::Theme) {
        let selected_row = self.selected_tree_row();
        let end = (self.tree_scroll + area.height as usize).min(self.tree.len());
        let lines = self.tree[self.tree_scroll..end]
            .iter()
            .enumerate()
            .map(|(offset, node)| {
                let index = self.tree_scroll + offset;
                let selected = Some(index) == selected_row;
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

    /// Display width of the diff text a row shows, per half for a
    /// side-by-side row. Drives the horizontal-scroll ceiling.
    fn row_content_width(&self, index: usize) -> usize {
        match self.rows[index] {
            RowKind::DiffLine(file, hunk, line) => {
                crate::util::visual_width(&self.diff.files[file].hunks[hunk].lines[line].text)
            }
            RowKind::DiffPair(file, hunk, old, new) => {
                let lines = &self.diff.files[file].hunks[hunk].lines;
                [old, new]
                    .into_iter()
                    .flatten()
                    .map(|line| crate::util::visual_width(&lines[line].text))
                    .max()
                    .unwrap_or(0)
            }
            _ => crate::util::visual_width(&self.row_text(index)),
        }
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
        // A hunk-level comment has no line gutter to live in, so it
        // keeps the row marker. Line comments mark their own gutter,
        // which is what makes two comments on one side-by-side row
        // separately visible instead of collapsing into one dot.
        let hunk_comment = row_anchor(kind).is_some_and(|anchor| {
            anchor.line.is_none() && self.comments.iter().any(|c| c.anchor == anchor)
        });
        let marker = match (index == self.cursor, hunk_comment) {
            (_, true) => "●",
            (true, false) => "›",
            (false, false) => " ",
        };
        let base = |visual: VisualKind| {
            let mut style = Style::default().fg(visual_color(visual, theme));
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
                let focused = self.effective_side(kind);
                for (slot, side) in [(old, Side::Old), (new, Side::New)] {
                    if side == Side::New {
                        spans.push(Span::styled("│", gutter));
                    }
                    // Only the addressed half carries the selection, so
                    // `c` and `x` can never act on a line the cursor is
                    // not visibly sitting on.
                    let half_selected = selected && side == focused;
                    let (number, text, visual, dot) = match slot {
                        Some(index) => {
                            let line = &lines[index];
                            let number = if side == Side::New {
                                line.new_line
                            } else {
                                line.old_line
                            };
                            let dot = if self.has_comment_on(file, hunk, index) {
                                "●"
                            } else {
                                " "
                            };
                            (
                                number.map(|n| n.to_string()).unwrap_or_default(),
                                line.text.as_str(),
                                self.line_visual(file, hunk, index),
                                dot,
                            )
                        }
                        None => (String::new(), "", VisualKind::Dim, " "),
                    };
                    let mut half_gutter = Style::default().fg(theme.text_dim);
                    let mut half_text = Style::default().fg(visual_color(visual, theme));
                    if half_selected {
                        half_gutter = half_gutter.bg(theme.fill);
                        half_text = half_text.bg(theme.fill);
                    }
                    spans.push(Span::styled(
                        format!("{number:>LINE_NUMBER_WIDTH$}{dot}"),
                        half_gutter,
                    ));
                    spans.push(Span::styled(
                        pad(
                            &window(text, self.horizontal_scroll, text_width),
                            text_width,
                        ),
                        half_text,
                    ));
                }
            }
            RowKind::DiffLine(file, hunk, line) => {
                let entry = &self.diff.files[file].hunks[hunk].lines[line];
                let old = entry.old_line.map(|n| n.to_string()).unwrap_or_default();
                let new = entry.new_line.map(|n| n.to_string()).unwrap_or_default();
                let dot = if self.has_comment_on(file, hunk, line) {
                    "●"
                } else {
                    " "
                };
                spans.push(Span::styled(
                    format!("{old:>LINE_NUMBER_WIDTH$} {new:>LINE_NUMBER_WIDTH$}{dot}│ "),
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
        self.ensure_tree_visible();

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
        // Clamp against the line under the CURSOR, not the widest line
        // in the diff: a lockfile's 3000-column line otherwise licensed
        // scrolling a neighbouring 40-column file clean out of view.
        // Sharing a viewport with a long line is enough to do it, so a
        // viewport-wide maximum does not close this either.
        let reachable = self
            .rows
            .get(self.cursor)
            .map(|_| self.row_content_width(self.cursor))
            .unwrap_or(0);
        self.horizontal_scroll = self
            .horizontal_scroll
            .min(reachable.saturating_sub(self.text_width(width)));
        let lines = (self.scroll..end)
            .map(|index| self.row_line(index, width, theme))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), self.diff_area);
        scrollbar::render_vertical(frame, gutter, self.rows.len(), body_height, self.scroll);

        let input = match &self.mode {
            InputMode::Normal if self.comments.is_empty() => {
                "c comment · / search · [/] hunks · {/} files · h/l side then scroll".to_string()
            }
            InputMode::Normal => format!(
                "{} comment{} drafted · Shift-S {} · x remove here",
                self.comments.len(),
                if self.comments.len() == 1 { "" } else { "s" },
                if self.is_pull_request() {
                    "submit as a GitHub review"
                } else {
                    "send to the agent"
                },
            ),
            InputMode::Search(input) => format!("/{input}█"),
            InputMode::Comment(input) => format!("Comment: {input}█"),
            InputMode::ReviewSummary(input) => format!("Review summary: {input}█"),
            InputMode::ReviewVerdict(_) => format!(
                "Post {} comment{} to GitHub — c comment · a approve · r request changes · Esc cancel",
                self.comments.len(),
                if self.comments.len() == 1 { "" } else { "s" },
            ),
            InputMode::Submitting => format!(
                "submitting {} comment{} to GitHub…",
                self.comments.len(),
                if self.comments.len() == 1 { "" } else { "s" },
            ),
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
        // Name the source `p` would move TO, not the one already on
        // screen — the title bar says where you are.
        let source_hint = match (self.is_pull_request(), self.comments.is_empty()) {
            (_, false) => "p blocked by drafted comments",
            (true, true) => "p read the local diff",
            (false, true) => "p read the PR diff",
        };
        let hint = format!(
            "j/k · PgUp/PgDn navigate · {tree_hint} · {layout} · {source_hint} · n/N search · Esc close"
        );
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

    /// The daemon's answer to a submitted review, routed back through
    /// the model: clearing the flag releases the viewer so the drafted
    /// comments can be corrected and sent again.
    fn attr(&mut self, attribute: Attribute, value: AttrValue) {
        if attribute == Attribute::Custom(REVIEW_IN_FLIGHT)
            && value == AttrValue::Flag(false)
            && matches!(self.mode, InputMode::Submitting)
        {
            self.mode = InputMode::Normal;
        }
    }

    fn state(&self) -> State {
        State::None
    }

    fn perform(&mut self, _: Cmd) -> CmdResult {
        CmdResult::NoChange
    }
}

impl AppComponent<Msg, UserEvent> for DiffReview {
    fn on(&mut self, event: &Event<UserEvent>) -> Option<Msg> {
        let (consumed, message) = self.handle_input(event);
        if consumed {
            return message;
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
                    // Paging and jumping inside the tree must move the
                    // TREE. Falling through to the changes-pane arms let
                    // the diff cursor land on a row that belongs to no
                    // file, leaving the tree highlighting a file that
                    // was no longer on screen.
                    Key::PageDown if tree => self.move_tree_cursor(self.tree_height as isize),
                    Key::PageUp if tree => self.move_tree_cursor(-(self.tree_height as isize)),
                    Key::Home | Key::Char('g') if tree => {
                        if let Some(index) = self.first_tree_file() {
                            self.select_tree_row(index);
                        }
                    }
                    Key::End | Key::Char('G') if tree => {
                        self.move_tree_cursor(self.tree.len() as isize);
                    }
                    Key::Enter | Key::Right | Key::Char('l') if tree => self.focus = Focus::Diff,
                    Key::Down | Key::Char('j') => self.move_cursor(1),
                    Key::Up | Key::Char('k') => self.move_cursor(-1),
                    Key::PageDown => self.move_cursor(self.body_height as isize),
                    Key::PageUp => self.move_cursor(-(self.body_height as isize)),
                    Key::Home | Key::Char('g') => self.cursor = 0,
                    Key::End | Key::Char('G') => {
                        self.cursor = self.rows.len().saturating_sub(1);
                    }
                    // On a side-by-side row the halves are the first
                    // stop; past the outer edge the same key scrolls, so
                    // a long line stays reachable from either half.
                    Key::Left | Key::Char('h') => {
                        if !self.move_side(false) {
                            self.horizontal_scroll = self.horizontal_scroll.saturating_sub(4);
                        }
                    }
                    Key::Right | Key::Char('l') => {
                        if !self.move_side(true) {
                            self.horizontal_scroll = self.horizontal_scroll.saturating_add(4);
                        }
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
                    // Switching source rebuilds the viewer from the
                    // other document, and a drafted comment's anchor
                    // does not survive the trip — a worktree line has
                    // no counterpart on GitHub, and vice versa. Refuse
                    // rather than silently reinterpret or drop them.
                    Key::Char('p') if !ctrl && self.comments.is_empty() => {
                        return Some(Msg::DiffReviewSourceSwitched {
                            workspace_key: self.workspace_key.clone(),
                            showing: self.target.clone(),
                        });
                    }
                    // One key, two verbs, chosen by the source on
                    // screen: the local diff's comments can only go to
                    // the agent working in that checkout, and the PR's
                    // can only go to GitHub.
                    Key::Char('S') if !self.comments.is_empty() && self.is_pull_request() => {
                        // Composing first, rather than posting on the
                        // keypress: publishing to a PR is public and
                        // must not ride a stray `S`.
                        self.mode = InputMode::ReviewSummary(String::new());
                    }
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
                self.ensure_tree_visible();
                None
            }
            Event::Mouse(mouse) => {
                // The full rect, not just the right edge: the modal is
                // inset from the screen, so a bare `column <` test also
                // claimed clicks to the LEFT of the modal's own border,
                // and an unbounded row claimed the hint line below the
                // tree — both selecting a file nobody pointed at.
                let over_tree = self.tree_shown
                    && (self.tree_area.x..self.tree_area.right()).contains(&mouse.column)
                    && (self.tree_area.y..self.tree_area.bottom()).contains(&mouse.row);
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
                self.ensure_tree_visible();
                None
            }
            _ => None,
        }
    }
}

/// The source lines a row stands for, for comment anchoring and for
/// carrying the cursor across a layout toggle.
fn row_file(kind: RowKind) -> Option<usize> {
    match kind {
        RowKind::File(file)
        | RowKind::Header(file, _)
        | RowKind::Hunk(file, _)
        | RowKind::DiffLine(file, _, _)
        | RowKind::DiffPair(file, _, _, _) => Some(file),
        RowKind::StatusHeader
        | RowKind::Status(_)
        | RowKind::Clean
        | RowKind::Divergence
        | RowKind::Truncated
        | RowKind::Spacer
        | RowKind::StatHeader
        | RowKind::Stat(_) => None,
    }
}

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
        RowKind::StatusHeader
        | RowKind::Status(_)
        | RowKind::Clean
        | RowKind::Divergence
        | RowKind::Truncated
        | RowKind::Spacer
        | RowKind::StatHeader
        | RowKind::Stat(_)
        | RowKind::File(_)
        | RowKind::Header(..) => None,
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
        RowKind::StatusHeader
        | RowKind::Status(_)
        | RowKind::Clean
        | RowKind::Divergence
        | RowKind::Truncated
        | RowKind::Spacer
        | RowKind::StatHeader
        | RowKind::Stat(_)
        | RowKind::File(_)
        | RowKind::Header(..) => false,
    }
}

/// A diff line as the search sees it: the line-number gutter followed
/// by the text, so `/` can still find a line by its number.
fn numbered(old: Option<u32>, new: Option<u32>, text: &str) -> String {
    let old = old.map(|n| n.to_string()).unwrap_or_default();
    let new = new.map(|n| n.to_string()).unwrap_or_default();
    format!("{old:>LINE_NUMBER_WIDTH$} {new:>LINE_NUMBER_WIDTH$} │ {text}")
}

fn visual_color(visual: VisualKind, theme: &crate::theme::Theme) -> Color {
    match visual {
        VisualKind::Dim => theme.text_dim,
        VisualKind::File | VisualKind::Hunk => theme.accent,
        VisualKind::Context => theme.text_strong,
        VisualKind::Addition => theme.success,
        VisualKind::Deletion => theme.error,
        VisualKind::Warning => theme.warn,
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

/// What to say about a checkout that has drifted from the pull request
/// on screen. `None` when it has not: an in-sync checkout earns no row.
///
/// The counts are the whole point — "your checkout differs" is a shrug,
/// "2 commits of yours are not in this PR" is a reason to stop.
fn divergence_notice(divergence: Option<&WorkspaceDiffDivergenceDto>) -> Option<String> {
    let divergence = divergence?;
    // Proven drift and admitted ignorance are different claims and get
    // different words. Labelling "I could not check" as DIVERGED is
    // what turns the warning into noise a reviewer learns to skip —
    // and the warning only works if it is rare.
    let mut drift = Vec::new();
    let mut unknown = Vec::new();
    match divergence.commits {
        CommitComparisonDto::Counted(spread) => {
            if spread.local_only > 0 {
                drift.push(format!(
                    "{} local commit{} not in this PR",
                    spread.local_only,
                    if spread.local_only == 1 { "" } else { "s" },
                ));
            }
            if spread.pr_only > 0 {
                drift.push(format!(
                    "{} PR commit{} not checked out",
                    spread.pr_only,
                    if spread.pr_only == 1 { "" } else { "s" },
                ));
            }
        }
        // The everyday case when reviewing someone else's branch: the
        // commit was never fetched. Saying "unrelated commit" here
        // accused a perfectly ordinary checkout of being wrong.
        CommitComparisonDto::ReferenceAbsent => {
            unknown.push("this PR's head commit isn't in your checkout — fetch to compare".into());
        }
        CommitComparisonDto::Unknown => unknown.push("couldn't compare commits".into()),
    }
    match divergence.dirty_files {
        Some(files) if files > 0 => drift.push(format!(
            "{files} uncommitted file{}",
            if files == 1 { "" } else { "s" },
        )),
        Some(_) => {}
        None => unknown.push("couldn't read the checkout's status".into()),
    }
    if drift.is_empty() && unknown.is_empty() {
        return None;
    }
    let label = if drift.is_empty() {
        "CHECKOUT"
    } else {
        "DIVERGED"
    };
    Some(format!(
        "{label} — {}",
        drift
            .into_iter()
            .chain(unknown)
            .collect::<Vec<_>>()
            .join(" · ")
    ))
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

/// `pull_request` comes from the target the viewer was mounted with,
/// the same answer `is_pull_request()` gives the render path. Inferring
/// it here from the payload instead left two ways to ask one question,
/// free to disagree.
fn build_rows(diff: &WorkspaceDiffDto, split: bool, pull_request: bool) -> Vec<RowKind> {
    // A pull request has no working tree, so its diff opens on the
    // divergence notice instead of a porcelain status block — "clean
    // worktree" under a PR diff would be answering a question nobody
    // asked with a fact about somewhere else.
    let local = !pull_request;
    let mut rows = Vec::new();
    if local {
        rows.push(RowKind::StatusHeader);
    }
    if diff.truncated {
        rows.push(RowKind::Truncated);
    }
    if divergence_notice(diff.divergence.as_ref()).is_some() {
        rows.push(RowKind::Divergence);
    }
    if local {
        if diff.status.is_empty() {
            rows.push(RowKind::Clean);
        } else {
            rows.extend((0..diff.status.len()).map(RowKind::Status));
        }
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
            head_sha: None,
            divergence: None,
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
            head_sha: None,
            divergence: None,
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
            head_sha: None,
            divergence: None,
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
        let rows = build_rows(&diff, true, false);
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
            head_sha: None,
            divergence: None,
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
        assert_eq!(
            review.tree[review.selected_tree_row().expect("a file is selected")].label,
            "b.rs"
        );

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

    /// A replacement hunk: one deletion and the addition that replaced
    /// it land on the same side-by-side row, plus a second file so the
    /// horizontal-scroll ceiling has a long line to be tempted by.
    fn replacement() -> WorkspaceDiffDto {
        WorkspaceDiffDto {
            status: vec![" M src/lib.rs".into()],
            stat: vec![" src/lib.rs | 2 +-".into()],
            truncated: false,
            head_sha: None,
            divergence: None,
            files: vec![
                file(
                    "src/lib.rs",
                    vec![
                        line(DiffLineKindDto::Context, " keep();", Some(1), Some(1)),
                        line(DiffLineKindDto::Deletion, "-old();", Some(2), None),
                        line(DiffLineKindDto::Addition, "+new();", None, Some(2)),
                    ],
                ),
                file(
                    "src/other.rs",
                    vec![line(DiffLineKindDto::Addition, "+z", None, Some(1))],
                ),
            ],
        }
    }

    fn comment_on(review: &mut DiffReview, needle: &str, body: &str) {
        review.cursor = (0..review.rows.len())
            .position(|index| review.row_text(index).contains(needle))
            .unwrap_or_else(|| panic!("no row for {needle}"));
        review.mode = InputMode::Comment(body.into());
        review.save_comment();
    }

    #[test]
    fn clearing_a_comment_spares_the_other_half_of_a_side_by_side_row() {
        let mut review = review(replacement());
        render(&mut review);
        review.on(&key(Key::Char('s')));
        render(&mut review);
        assert!(
            !review.split,
            "drafted in unified so both sides are addressable"
        );
        comment_on(&mut review, "-old();", "why was this dropped");
        comment_on(&mut review, "+new();", "name this better");
        assert_eq!(review.comments.len(), 2);

        review.on(&key(Key::Char('s')));
        render(&mut review);
        assert!(review.split, "both comments now share one row");
        review.on(&key(Key::Char('x')));

        // `x` clears the addressed line only — the other half's comment
        // is somebody's typed prose and there is no undo.
        assert_eq!(
            review
                .comments
                .iter()
                .map(|comment| comment.body.as_str())
                .collect::<Vec<_>>(),
            vec!["why was this dropped"],
        );
    }

    #[test]
    fn commenting_on_a_split_row_anchors_to_the_half_under_the_cursor() {
        let mut review = review(replacement());
        render(&mut review);
        assert!(review.split);
        review.cursor = (0..review.rows.len())
            .position(|index| review.row_text(index).contains("-old();"))
            .expect("replacement row");

        review.on(&key(Key::Char('h')));
        review.mode = InputMode::Comment("this deletion was wrong".into());
        review.save_comment();
        let deletion = review.comments.pop().expect("comment");
        assert_eq!(deletion.referenced_line, "-old();");
        assert_eq!((deletion.old_line, deletion.new_line), (Some(2), None));

        review.on(&key(Key::Char('l')));
        review.mode = InputMode::Comment("and this replacement too".into());
        review.save_comment();
        let addition = review.comments.pop().expect("comment");
        assert_eq!(addition.referenced_line, "+new();");
        assert_eq!((addition.old_line, addition.new_line), (None, Some(2)));
    }

    #[test]
    fn both_comments_on_one_split_row_are_separately_visible() {
        let mut review = review(replacement());
        render(&mut review);
        review.on(&key(Key::Char('s')));
        render(&mut review);
        comment_on(&mut review, "-old();", "a");
        comment_on(&mut review, "+new();", "b");
        review.on(&key(Key::Char('s')));
        let output = render(&mut review);

        let row = output
            .lines()
            .find(|line| line.contains("-old();"))
            .expect("replacement row");
        assert_eq!(
            row.matches('\u{25cf}').count(),
            2,
            "one marker per commented line, not one per row: {row}"
        );
    }

    #[test]
    fn a_half_with_no_line_falls_back_to_the_side_that_has_one() {
        let mut review = review(replacement());
        render(&mut review);
        // Park on the addition-only row of the second file with the
        // old side preferred; `c` must still find a real line.
        review.side = Side::Old;
        review.cursor = (0..review.rows.len())
            .position(|index| review.row_text(index).contains("+z"))
            .expect("addition-only row");
        review.mode = InputMode::Comment("note".into());
        review.save_comment();

        assert_eq!(review.comments[0].referenced_line, "+z");
        assert_eq!(review.comments[0].new_line, Some(1));
    }

    #[test]
    fn the_tree_never_highlights_a_file_the_changes_pane_left() {
        let mut review = review(replacement());
        render(&mut review);
        review.on(&key(Key::Tab));
        review.on(&key(Key::Char('j')));
        assert_eq!(review.current_file(), Some(1));

        // `g` in tree focus belongs to the TREE; it used to drive the
        // diff cursor up into the status block, stranding the tree's
        // highlight on a file that was no longer shown.
        review.on(&key(Key::Char('g')));
        assert_eq!(review.current_file(), Some(0));
        assert_eq!(
            review.tree[review.selected_tree_row().expect("selection")].label,
            "lib.rs"
        );

        // And wherever the diff cursor goes the tree follows it, even
        // onto rows that belong to no file: it points at the file the
        // pane would reach, never at the one it came from.
        review.focus = Focus::Diff;
        review.on(&key(Key::Char('}')));
        assert_eq!(review.current_file(), Some(1));
        review.cursor = 0;
        assert_eq!(
            review.current_file(),
            None,
            "the status header owns no file"
        );
        assert_eq!(
            review.tree[review.selected_tree_row().expect("selection")].label,
            "lib.rs",
            "the highlight follows the cursor rather than stranding on other.rs",
        );
    }

    #[test]
    fn the_tree_ignores_mouse_events_outside_its_own_rect() {
        use tuirealm::event::MouseEvent;

        let files = (0..40)
            .map(|index| {
                file(
                    &format!("src/f{index:02}.rs"),
                    vec![line(DiffLineKindDto::Addition, "+a", None, Some(1))],
                )
            })
            .collect();
        let mut review = review(WorkspaceDiffDto {
            status: Vec::new(),
            stat: Vec::new(),
            truncated: false,
            head_sha: None,
            divergence: None,
            files,
        });
        render(&mut review);
        assert!(review.tree_shown);
        let click = |column, row| {
            Event::<UserEvent>::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };

        let before = review.selected_tree_row();

        // Left of the modal's own border.
        review.on(&click(0, review.tree_area.y + 3));
        assert_eq!(review.focus, Focus::Diff);
        assert_eq!(review.selected_tree_row(), before);

        // The hint line below the tree body.
        let below = review.tree_area.y + review.tree_area.height + 1;
        review.on(&click(review.tree_area.x + 2, below));
        assert_eq!(review.focus, Focus::Diff);
        assert_eq!(review.selected_tree_row(), before);

        // Inside the rect still selects the row that was pointed at:
        // row 0 is the `src/` directory, so offset 3 is `f02.rs`.
        review.on(&click(review.tree_area.x + 2, review.tree_area.y + 3));
        assert_eq!(review.focus, Focus::Tree);
        assert_eq!(review.tree[3].label, "f02.rs");
        assert_eq!(review.current_file(), Some(2));
    }

    #[test]
    fn horizontal_scroll_stops_at_the_widest_line_on_screen() {
        let mut diff = replacement();
        diff.files[1].hunks[0].lines[0].text = format!("+{}", "q".repeat(3000));
        let mut review = review(diff);
        render(&mut review);
        review.cursor = (0..review.rows.len())
            .position(|index| review.row_text(index).contains("keep();"))
            .expect("short line");

        for _ in 0..400 {
            review.on(&key(Key::Char('l')));
        }
        let output = render(&mut review);

        // The 3000-column line lives in a file that is not on screen; it
        // must not license scrolling this one out of view.
        assert!(
            output.contains("keep();"),
            "short line scrolled out of view: horizontal_scroll={}",
            review.horizontal_scroll
        );
    }

    #[test]
    fn toggling_the_layout_leaves_a_header_row_where_it_was() {
        let mut review = review(replacement());
        render(&mut review);
        for row in [0, 1] {
            review.cursor = row;
            let before = review.row_text(row).into_owned();
            review.on(&key(Key::Char('s')));
            render(&mut review);
            assert_eq!(review.row_text(review.cursor), before, "row {row} moved");
            review.on(&key(Key::Char('s')));
            render(&mut review);
            assert_eq!(
                review.row_text(review.cursor),
                before,
                "row {row} moved back"
            );
        }
    }

    #[test]
    fn toggling_the_layout_does_not_rebuild_the_row_list_twice() {
        let mut review = review(replacement());
        render(&mut review);
        let split_rows = review.rows.clone();
        review.on(&key(Key::Char('s')));
        render(&mut review);
        let unified_rows = review.rows.clone();
        review.on(&key(Key::Char('s')));
        render(&mut review);

        assert_eq!(
            review.rows, split_rows,
            "the cached layout comes back intact"
        );
        assert_eq!(
            review.idle_rows.as_ref(),
            Some(&unified_rows),
            "the layout that stepped aside is retained, not rebuilt per frame",
        );
    }

    #[test]
    fn search_still_finds_a_line_by_its_number() {
        let diff = WorkspaceDiffDto {
            status: Vec::new(),
            stat: Vec::new(),
            truncated: false,
            head_sha: None,
            divergence: None,
            files: vec![file(
                "src/lib.rs",
                vec![
                    line(DiffLineKindDto::Context, " keep();", Some(417), Some(417)),
                    line(DiffLineKindDto::Addition, "+fix();", None, Some(418)),
                ],
            )],
        };
        let mut review = review(diff);
        render(&mut review);
        review.mode = InputMode::Search("418".into());
        review.handle_input(&key(Key::Enter));

        assert!(
            review.row_text(review.cursor).contains("fix();"),
            "the line-number gutter is searchable; landed on {:?}",
            review.row_text(review.cursor),
        );
    }

    fn pull_request_review(diff: WorkspaceDiffDto) -> DiffReview {
        DiffReview::new(
            WorkspaceKey::new("w"),
            WorkspaceDiffTarget::PullRequest,
            Vec::new(),
            diff,
        )
    }

    /// The PR diff is a different document from the worktree's, so the
    /// header has to say which one is on screen — reading one and
    /// merging the other is the mistake this whole source split exists
    /// to prevent.
    #[test]
    fn the_header_names_the_pull_request_as_the_source() {
        let mut diff = sample();
        diff.head_sha = Some("f00dcafe".into());
        let mut review = pull_request_review(diff);

        assert!(
            render(&mut review).contains("Review · pull request ·"),
            "the PR source must be named in the title"
        );
    }

    /// A checkout that has drifted from the PR is the case that causes
    /// real mistakes, so it earns a row of its own — with the counts,
    /// because "your checkout differs" is a shrug.
    #[test]
    fn a_diverged_checkout_is_called_out_with_its_counts() {
        let mut diff = sample();
        diff.head_sha = Some("f00dcafe".into());
        diff.divergence = Some(WorkspaceDiffDivergenceDto {
            dirty_files: Some(2),
            commits: CommitComparisonDto::Counted(lazybox_ipc::CommitSpreadDto {
                local_only: 1,
                pr_only: 3,
            }),
        });
        let mut review = pull_request_review(diff);

        // Wide enough that the whole notice fits: the assertion is
        // about what it says, not about where the pane clips it.
        let rendered = render_sized(&mut review, 200, 30);
        assert!(
            rendered.contains("1 local commit not in this PR"),
            "unpushed work must be named: {rendered}"
        );
        assert!(
            rendered.contains("3 PR commits not checked out"),
            "commits only on the PR must be named: {rendered}"
        );
        assert!(
            rendered.contains("2 uncommitted files"),
            "a dirty worktree must be named: {rendered}"
        );
    }

    /// An in-sync checkout earns no row — a notice that fires every
    /// time is one nobody reads when it matters.
    #[test]
    fn an_in_sync_checkout_gets_no_divergence_row() {
        let mut diff = sample();
        diff.head_sha = Some("f00dcafe".into());
        diff.divergence = Some(WorkspaceDiffDivergenceDto {
            dirty_files: Some(0),
            commits: CommitComparisonDto::Counted(lazybox_ipc::CommitSpreadDto {
                local_only: 0,
                pr_only: 0,
            }),
        });
        let mut review = pull_request_review(diff);

        assert!(!render(&mut review).contains("DIVERGED"));
        assert!(!review.rows.contains(&RowKind::Divergence));
    }

    /// An unfetched PR head is the ordinary case when reviewing someone
    /// else's branch. Calling it an "unrelated commit" fired the
    /// warning on every such review — and a warning that fires every
    /// time is one nobody reads on the day it matters.
    #[test]
    fn an_unfetched_pr_head_says_to_fetch_rather_than_accusing_the_checkout() {
        let mut diff = sample();
        diff.head_sha = Some("f00dcafe".into());
        diff.divergence = Some(WorkspaceDiffDivergenceDto {
            dirty_files: Some(0),
            commits: CommitComparisonDto::ReferenceAbsent,
        });
        let mut review = pull_request_review(diff);

        let rendered = render_sized(&mut review, 200, 30);
        assert!(
            rendered.contains("this PR's head commit isn't in your checkout — fetch to compare"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("unrelated"),
            "an unfetched commit is not unrelated history: {rendered}"
        );
        assert!(
            rendered.contains("CHECKOUT —"),
            "not knowing is not the same claim as having diverged: {rendered}"
        );
    }

    /// A status probe that could not run must never render as clean.
    /// Reporting an unreadable worktree as having nothing uncommitted
    /// is the reassurance a reviewer acts on right before merging over
    /// their own unsaved work.
    #[test]
    fn an_unreadable_status_is_admitted_not_reported_as_clean() {
        let mut diff = sample();
        diff.head_sha = Some("f00dcafe".into());
        diff.divergence = Some(WorkspaceDiffDivergenceDto {
            dirty_files: None,
            commits: CommitComparisonDto::Counted(lazybox_ipc::CommitSpreadDto {
                local_only: 0,
                pr_only: 0,
            }),
        });
        let mut review = pull_request_review(diff);

        assert!(
            render_sized(&mut review, 200, 30).contains("couldn't read the checkout's status"),
            "silence here reads as a clean worktree"
        );
    }

    /// A PR has no working tree, so the porcelain status block is
    /// answering a question nobody asked — and "clean worktree" under a
    /// PR diff is an answer about somewhere else entirely.
    #[test]
    fn the_pull_request_view_drops_the_worktree_status_block() {
        let mut diff = sample();
        diff.head_sha = Some("f00dcafe".into());
        diff.status = Vec::new();
        let mut review = pull_request_review(diff);

        let rendered = render(&mut review);
        assert!(!rendered.contains("clean worktree"), "{rendered}");
        assert!(!rendered.contains("STATUS"), "{rendered}");
    }

    /// `p` asks the model for the other document. It never mutates the
    /// viewer in place — the PR's diff lives on GitHub and the
    /// worktree's on disk, and neither is derivable from the other.
    #[test]
    fn p_asks_for_the_other_source() {
        let mut diff = sample();
        diff.head_sha = Some("f00dcafe".into());
        let mut review = pull_request_review(diff);
        render(&mut review);

        assert!(matches!(
            review.on(&key(Key::Char('p'))),
            Some(Msg::DiffReviewSourceSwitched {
                showing: WorkspaceDiffTarget::PullRequest,
                ..
            })
        ));
    }

    /// A drafted comment anchors into the document it was written on; a
    /// worktree line has no counterpart on GitHub and vice versa. So
    /// the switch is refused rather than silently dropping or
    /// reinterpreting what was typed.
    #[test]
    fn p_is_refused_while_comments_are_drafted() {
        let mut diff = sample();
        diff.head_sha = Some("f00dcafe".into());
        let mut review = pull_request_review(diff);
        render(&mut review);
        comment_on(&mut review, "fix();", "this needs a test");

        assert!(review.on(&key(Key::Char('p'))).is_none());
        assert!(
            render(&mut review).contains("p blocked by drafted comments"),
            "the refusal must say why"
        );
    }

    /// `Shift-S` is one key with two verbs, chosen by the source: the
    /// local diff's comments can only reach the agent working in it,
    /// and the PR's can only reach GitHub. On the PR it opens the
    /// review prompts rather than posting on the keypress — publishing
    /// must not ride a single stray `S`.
    #[test]
    fn the_pull_request_send_composes_a_review_instead_of_prompting_an_agent() {
        let mut diff = sample();
        diff.head_sha = Some("f00dcafe".into());
        let mut review = pull_request_review(diff);
        render(&mut review);
        comment_on(&mut review, "fix();", "this needs a test");

        assert!(review.on(&key(Key::Char('S'))).is_none());
        assert!(matches!(review.mode, InputMode::ReviewSummary(_)));
        assert!(render(&mut review).contains("Review summary:"));
    }

    /// The whole batch becomes one review, anchored the way GitHub
    /// anchors comments: an added line on the RIGHT at its new number,
    /// a deleted one on the LEFT at its old number.
    #[test]
    fn a_submitted_review_carries_every_comment_with_its_github_anchor() {
        let diff = WorkspaceDiffDto {
            status: Vec::new(),
            stat: Vec::new(),
            truncated: false,
            head_sha: Some("f00dcafe".into()),
            divergence: None,
            files: vec![file(
                "src/lib.rs",
                vec![
                    line(DiffLineKindDto::Deletion, "-gone();", Some(41), None),
                    line(DiffLineKindDto::Addition, "+fix();", None, Some(41)),
                ],
            )],
        };
        let mut review = pull_request_review(diff);
        render(&mut review);
        // Side by side, the deletion and its replacement share a row,
        // so each comment is written on the half the cursor addresses.
        review.cursor = (0..review.rows.len())
            .position(|index| review.row_text(index).contains("-gone();"))
            .expect("the replacement row");
        review.on(&key(Key::Char('h')));
        review.mode = InputMode::Comment("why remove this?".into());
        review.save_comment();
        review.on(&key(Key::Char('l')));
        review.mode = InputMode::Comment("drops the error".into());
        review.save_comment();

        review.mode = InputMode::ReviewSummary("two nits".into());
        review.handle_input(&key(Key::Enter));
        assert!(matches!(review.mode, InputMode::ReviewVerdict(_)));
        let submitted = review.handle_input(&key(Key::Char('r'))).1;

        let Some(Msg::DiffReviewPosted {
            head_sha,
            summary,
            verdict,
            comments,
            ..
        }) = submitted
        else {
            panic!("the verdict keypress must submit, got {submitted:?}");
        };
        assert_eq!(head_sha, "f00dcafe");
        assert_eq!(summary, "two nits");
        assert_eq!(verdict, ReviewVerdictDto::RequestChanges);
        assert_eq!(
            comments,
            vec![
                ReviewCommentDto {
                    path: "src/lib.rs".into(),
                    line: 41,
                    side: DiffSideDto::Left,
                    body: "why remove this?".into(),
                },
                ReviewCommentDto {
                    path: "src/lib.rs".into(),
                    line: 41,
                    side: DiffSideDto::Right,
                    body: "drops the error".into(),
                },
            ]
        );
        // Still mounted and still holding the comments until GitHub
        // answers — see `a_refused_review_keeps_every_drafted_comment`.
        assert!(matches!(review.mode, InputMode::Submitting));
        assert_eq!(review.comments.len(), 2);
    }

    /// A refused review must leave every drafted comment exactly where
    /// it was. The viewer is the only place they exist, so closing it
    /// on submit — before GitHub had even answered — turned a 422 on a
    /// stale `commit_id` into the silent destruction of everything the
    /// reviewer had written.
    #[test]
    fn a_refused_review_keeps_every_drafted_comment() {
        let mut diff = sample();
        diff.head_sha = Some("f00dcafe".into());
        let mut review = pull_request_review(diff);
        render(&mut review);
        comment_on(&mut review, "fix();", "this needs a test");
        review.mode = InputMode::ReviewSummary("a nit".into());
        review.handle_input(&key(Key::Enter));
        assert!(review.handle_input(&key(Key::Char('c'))).1.is_some());

        // In flight: the viewer is held, and keys that would discard or
        // re-send the review are refused while the request is out.
        assert!(matches!(review.mode, InputMode::Submitting));
        assert!(render(&mut review).contains("submitting 1 comment to GitHub…"));
        assert!(
            review.on(&key(Key::Char('S'))).is_none(),
            "a second Shift-S must not post the review twice"
        );
        assert!(matches!(review.mode, InputMode::Submitting));

        // GitHub refused. The model releases the viewer; the comments
        // are still here and still editable.
        review.attr(Attribute::Custom(REVIEW_IN_FLIGHT), AttrValue::Flag(false));
        assert!(matches!(review.mode, InputMode::Normal));
        assert_eq!(review.comments.len(), 1);
        assert_eq!(review.comments[0].body, "this needs a test");
        assert!(review.on(&key(Key::Char('S'))).is_none());
        assert!(
            matches!(review.mode, InputMode::ReviewSummary(_)),
            "the released viewer can compose and send the review again"
        );
    }

    /// Esc is the one key that still works in flight. Swallowing every
    /// key would strand the reviewer in a modal with no exit if the
    /// reply never came — a daemon restart is enough.
    #[test]
    fn esc_can_still_leave_a_review_that_is_in_flight() {
        let mut diff = sample();
        diff.head_sha = Some("f00dcafe".into());
        let mut review = pull_request_review(diff);
        render(&mut review);
        comment_on(&mut review, "fix();", "this needs a test");
        review.mode = InputMode::Submitting;

        assert!(matches!(
            review.on(&key(Key::Esc)),
            Some(Msg::ModalDismissed)
        ));
    }

    /// GitHub refuses a comment or request-changes review with no body,
    /// so Enter on an empty summary holds the prompt open rather than
    /// walking the reviewer to a verdict that cannot be submitted.
    #[test]
    fn an_empty_review_summary_does_not_advance_to_a_verdict() {
        let mut diff = sample();
        diff.head_sha = Some("f00dcafe".into());
        let mut review = pull_request_review(diff);
        render(&mut review);
        comment_on(&mut review, "fix();", "this needs a test");

        review.mode = InputMode::ReviewSummary("   ".into());
        review.handle_input(&key(Key::Enter));

        assert!(matches!(review.mode, InputMode::ReviewSummary(_)));
    }

    /// Esc at the verdict abandons the review without posting. The
    /// comments survive — the reviewer backed out of publishing, not
    /// out of the work.
    #[test]
    fn escaping_the_verdict_posts_nothing_and_keeps_the_comments() {
        let mut diff = sample();
        diff.head_sha = Some("f00dcafe".into());
        let mut review = pull_request_review(diff);
        render(&mut review);
        comment_on(&mut review, "fix();", "this needs a test");
        review.mode = InputMode::ReviewVerdict("a nit".into());

        assert!(review.handle_input(&key(Key::Esc)).1.is_none());
        assert!(matches!(review.mode, InputMode::Normal));
        assert_eq!(review.comments.len(), 1);
    }

    /// `\ No newline at end of file` carries no line number on either
    /// side, so GitHub cannot address it. Refusing the input is the
    /// only refusal that costs the reviewer nothing — refusing at
    /// submit time would throw away what they had already typed.
    #[test]
    fn a_line_github_cannot_address_refuses_the_comment_input() {
        let diff = WorkspaceDiffDto {
            status: Vec::new(),
            stat: Vec::new(),
            truncated: false,
            head_sha: Some("f00dcafe".into()),
            divergence: None,
            files: vec![file(
                "src/lib.rs",
                vec![
                    line(DiffLineKindDto::Addition, "+fix();", None, Some(41)),
                    line(
                        DiffLineKindDto::Meta,
                        "\\ No newline at end of file",
                        None,
                        None,
                    ),
                ],
            )],
        };
        let mut on_github = pull_request_review(diff);
        render(&mut on_github);
        on_github.cursor = (0..on_github.rows.len())
            .position(|index| on_github.row_text(index).contains("No newline"))
            .expect("the meta row");

        on_github.begin_comment();
        assert!(matches!(on_github.mode, InputMode::Normal));

        // The same row on the local source still takes a comment: it
        // goes to an agent that can read the worktree, not to GitHub.
        let mut local = review(sample());
        render(&mut local);
        local.cursor = (0..local.rows.len())
            .position(|index| local.row_text(index).contains("fix();"))
            .expect("the added row");
        local.begin_comment();
        assert!(matches!(local.mode, InputMode::Comment(_)));
    }
}
