//! `WorkRollup` — the cross-workspace work readout (`E w`).
//!
//! The viewing half of the work store (#1935): one screen answering "how
//! does this multi-workspace effort stand?" from the store the MCP tools
//! and the `lazybox work …` CLI already write. Since #1946 a spawn records
//! its own hand-off, so the store fills itself and this view has rows
//! without anyone opting in.
//!
//! Plans are headers carrying their `done/total` roll-up and how many
//! workspaces they span, with the work on no plan as a section of its own
//! counted in open rows — `HeaderCount` carries why those two differ.
//! Their tasks are the rows beneath, each with a lifecycle glyph:
//!
//! - `✓` completed,
//! - `▶` underway,
//! - `?` awaiting answer — the question is the row's detail,
//! - `⏸` held — the blocker is the detail,
//! - `✗` failed — the cause is the detail,
//! - `·` pending or canceled.
//!
//! `↑/↓` (or `j/k`) move between *task* rows — a plan header is a label,
//! never a cursor stop. `Enter` jumps to the owning workspace when the
//! owner resolves to one (a task owned by `human`, by lazybox, or by a
//! workspace that has since gone is rendered plainly and does not jump).
//! Any other key closes.
//!
//! Mounting sends `Command::WorkCall` and renders a loading state until
//! the correlated `Event::WorkReport` lands; the rows are projected in the
//! model (`Model::work_rollup_entries`), which is where the owner → session
//! key lookup lives.

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

/// The lifecycle a task row renders as.
///
/// Derived from [`lazybox_ipc::work::WorkRow::lifecycle`], which is a
/// label on the wire rather than an enum. An unrecognised label maps to
/// [`WorkLifeKind::Pending`] rather than being dropped: a lifecycle this
/// build has not heard of must still show up as a row, because a listing
/// that silently omits work reads as less work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkLifeKind {
    Completed,
    Underway,
    Asking,
    Held,
    Failed,
    /// Pending, canceled, or a label this build does not know.
    Pending,
}

impl WorkLifeKind {
    /// Map the wire label. Kept beside the glyphs so a renamed label is a
    /// one-line change in the place that renders it.
    pub(crate) fn from_label(label: &str) -> Self {
        match label {
            "completed" => Self::Completed,
            "underway" => Self::Underway,
            "awaiting answer" => Self::Asking,
            "held" => Self::Held,
            "failed" => Self::Failed,
            _ => Self::Pending,
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Completed => "✓",
            Self::Underway => "▶",
            Self::Asking => "?",
            Self::Held => "⏸",
            Self::Failed => "✗",
            Self::Pending => "·",
        }
    }

    fn color(self, theme: &crate::theme::Theme) -> Color {
        match self {
            Self::Completed => theme.success,
            Self::Underway => theme.accent,
            Self::Asking | Self::Held => theme.warn,
            Self::Failed => theme.error,
            Self::Pending => theme.text_dim,
        }
    }
}

/// One pre-resolved task row.
#[derive(Debug, Clone)]
pub(crate) struct WorkTaskRow {
    pub kind: WorkLifeKind,
    pub title: String,
    /// The owner as the store renders it — a workspace key, `human`, or
    /// `lazybox`. Empty when the task is unassigned.
    pub owner: String,
    /// Who asked for it, when that is somebody other than the owner.
    /// This is the half of a hand-off that goes missing today: a spawn
    /// into another workspace records the requester on the work, and
    /// without it the view says what is happening but not who is owed
    /// the answer.
    pub requester: Option<String>,
    /// The question for `awaiting answer`, the blocker for `held`, the
    /// cause for `failed`.
    pub detail: Option<String>,
    /// Where `Enter` jumps, when the owner resolved to a live workspace.
    pub jump: Option<lazybox_core::SessionKey>,
}

/// What a group header counts.
///
/// Two shapes, because the store answers two different questions. A plan
/// carries every task on it, terminal ones included, so `done/total` is a
/// real fraction. The bucket of work on no plan does not: `work_status`
/// filters terminal rows out of it, so counting the rows that *arrived*
/// as a denominator would report `0/N` forever — every finished hand-off
/// having been dropped before the client saw it. An open count is what
/// that reply can honestly support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeaderCount {
    /// `done` of `total` tasks on a plan.
    Fraction(usize, usize),
    /// This many rows still open; no denominator exists.
    Open(usize),
}

/// One row of the flattened list: a plan header or a task under it.
#[derive(Debug, Clone)]
pub(crate) enum WorkRollupEntry {
    /// A plan, or the bucket of work on no plan.
    Header {
        title: String,
        /// False for the bucket of work on no plan, which is a section
        /// of this view rather than a plan somebody filed. The title
        /// counts plans, and counting the bucket as one would report a
        /// plan that does not exist.
        is_plan: bool,
        /// What this group counts — see [`HeaderCount`]. Either way the
        /// bucket of unplanned work is counted, because an agent-to-agent
        /// hand-off is recorded with no plan: that bucket is where the
        /// fleet's tracked work actually lands, and a section with no
        /// count at all reads as a leftover rather than as the thing to
        /// look at.
        count: HeaderCount,
        /// How many distinct workspaces the group's work spans.
        members: usize,
    },
    Task(WorkTaskRow),
}

/// The cross-workspace work readout.
pub(crate) struct WorkRollup {
    /// `None` until the daemon answers — the loading state.
    entries: Option<Vec<WorkRollupEntry>>,
    /// Set when the call could not be answered, so the modal says why
    /// rather than showing an empty store.
    error: Option<String>,
    /// Rows the daemon could not decode. Surfaced for the same reason the
    /// wire carries the count: a listing that hides them reads as shorter.
    undecodable: usize,
    /// Cursor into `entries`, always on a task row; `None` when there are
    /// none to stand on.
    cursor: Option<usize>,
    /// Topmost visible row, kept so the cursor stays on screen.
    scroll: usize,
    /// Body viewport height, cached in `view` for scroll math.
    body_height: usize,
}

impl WorkRollup {
    /// The pre-reply loading state, mounted immediately so the modal is up
    /// while the call is in flight.
    pub(crate) fn loading() -> Self {
        Self {
            entries: None,
            error: None,
            undecodable: 0,
            cursor: None,
            scroll: 0,
            body_height: 0,
        }
    }

    /// The resolved state, built when the correlated `Event::WorkReport`
    /// lands.
    pub(crate) fn resolved(
        entries: Vec<WorkRollupEntry>,
        undecodable: usize,
        error: Option<String>,
    ) -> Self {
        let cursor = first_task(&entries);
        Self {
            entries: Some(entries),
            error,
            undecodable,
            cursor,
            scroll: 0,
            body_height: 0,
        }
    }

    /// How many task rows this view holds — the figure the title counts.
    fn task_count(&self) -> usize {
        self.entries
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|e| matches!(e, WorkRollupEntry::Task(_)))
            .count()
    }

    fn plan_count(&self) -> usize {
        self.entries
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|e| matches!(e, WorkRollupEntry::Header { is_plan: true, .. }))
            .count()
    }

    /// Move the cursor to the next task row in `delta`'s direction,
    /// skipping headers. A header is a label, not a stop: landing on one
    /// would make `Enter` mean nothing on a row that looks selectable.
    fn move_cursor(&mut self, delta: isize) {
        let Some(entries) = self.entries.as_deref() else {
            return;
        };
        let Some(cur) = self.cursor else {
            return;
        };
        let step = if delta < 0 { -1isize } else { 1isize };
        let mut probe = cur as isize;
        loop {
            probe += step;
            if probe < 0 || probe as usize >= entries.len() {
                // No further task row that way — stay put rather than
                // parking the cursor on a header.
                return;
            }
            if matches!(entries[probe as usize], WorkRollupEntry::Task(_)) {
                self.cursor = Some(probe as usize);
                self.follow_cursor();
                return;
            }
        }
    }

    /// Clamp `scroll` so the cursor row is inside the viewport. A cursor
    /// on the first task of a plan pulls its header into view too, so a
    /// row never reads as belonging to whatever header happens to be
    /// above it on screen.
    fn follow_cursor(&mut self) {
        let Some(cur) = self.cursor else {
            return;
        };
        if self.body_height == 0 {
            return;
        }
        let anchor = self.header_above(cur).unwrap_or(cur);
        if anchor < self.scroll {
            self.scroll = anchor;
        } else if cur >= self.scroll + self.body_height {
            self.scroll = cur + 1 - self.body_height;
        }
    }

    /// The header immediately above `row`, when `row` is the first task
    /// under it.
    fn header_above(&self, row: usize) -> Option<usize> {
        let entries = self.entries.as_deref()?;
        let prev = row.checked_sub(1)?;
        matches!(entries.get(prev)?, WorkRollupEntry::Header { .. }).then_some(prev)
    }

    /// The highlighted row's jump target, if it has one.
    fn selected_jump(&self) -> Option<lazybox_core::SessionKey> {
        match self.entries.as_deref()?.get(self.cursor?)? {
            WorkRollupEntry::Task(task) => task.jump.clone(),
            WorkRollupEntry::Header { .. } => None,
        }
    }

    /// A group header: `▰▰▱  2/5  shim hardening   3 workspaces`, or
    /// `     2 open  on no plan` for the bucket with no denominator.
    fn header_line(
        title: &str,
        count: HeaderCount,
        members: usize,
        theme: &crate::theme::Theme,
    ) -> Line<'static> {
        let strong = Style::default()
            .fg(theme.text_strong)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(theme.text_dim);
        let mut spans = Vec::new();
        match count {
            HeaderCount::Fraction(done, total) => {
                spans.push(Span::styled(
                    format!("{}  ", meter(done, total)),
                    Style::default().fg(if done == total && total > 0 {
                        theme.success
                    } else {
                        theme.accent
                    }),
                ));
                spans.push(Span::styled(format!("{done}/{total}  "), dim));
            }
            // Padded to the width a fraction takes, so the titles line up
            // down the view instead of one section hanging left.
            HeaderCount::Open(open) => spans.push(Span::styled(format!("{open:>7} open  "), dim)),
        }
        spans.push(Span::styled(title.to_string(), strong));
        if members > 1 {
            spans.push(Span::styled(format!("   {members} workspaces"), dim));
        }
        Line::from(spans)
    }

    /// One task row, indented under its header.
    fn task_line(task: &WorkTaskRow, selected: bool, theme: &crate::theme::Theme) -> Line<'static> {
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
        let caret = if selected { "  ▸ " } else { "    " };
        let mut spans = vec![
            Span::styled(caret.to_string(), base),
            Span::styled(
                format!("{} ", task.kind.glyph()),
                base.fg(task.kind.color(theme)),
            ),
            Span::styled(task.title.clone(), base),
        ];
        if !task.owner.is_empty() {
            spans.push(Span::styled(format!("  {}", task.owner), dim));
        }
        if let Some(requester) = &task.requester {
            spans.push(Span::styled(format!("  ← {requester}"), dim));
        }
        if let Some(detail) = &task.detail {
            spans.push(Span::styled(
                format!("  {detail}"),
                base.fg(task.kind.color(theme)),
            ));
        }
        Line::from(spans)
    }
}

/// The index of the first task row, if there is one.
fn first_task(entries: &[WorkRollupEntry]) -> Option<usize> {
    entries
        .iter()
        .position(|e| matches!(e, WorkRollupEntry::Task(_)))
}

/// A five-cell progress bar. `done > total` cannot come off the wire, but
/// is clamped rather than trusted: a bar wider than its track would push
/// the rest of the line out of the frame.
fn meter(done: usize, total: usize) -> String {
    const CELLS: usize = 5;
    if total == 0 {
        return "▱".repeat(CELLS);
    }
    let filled = (done.min(total) * CELLS).div_ceil(total).min(CELLS);
    format!("{}{}", "▰".repeat(filled), "▱".repeat(CELLS - filled))
}

impl Component for WorkRollup {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 96u16.min(area.width.saturating_sub(4));
        let modal_h = 26u16.min(area.height.saturating_sub(2));
        let modal = centered_rect(area, modal_w, modal_h);
        let title = match self.entries.as_deref() {
            None => " Work · reading… ".to_string(),
            Some(_) => {
                let plans = self.plan_count();
                let tasks = self.task_count();
                format!(" Work · {plans} plans · {tasks} tasks ")
            }
        };
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

        let dim = Style::default().fg(theme.text_dim);
        let lines: Vec<Line<'static>> = match (&self.error, self.entries.as_deref()) {
            (Some(error), _) => vec![Line::from(Span::styled(
                format!("  {error}"),
                Style::default().fg(theme.error),
            ))],
            (None, None) => vec![Line::from(Span::styled("  Reading the work store…", dim))],
            (None, Some([])) => vec![
                Line::from(Span::styled("  Nothing in the work store yet.", dim)),
                Line::from(Span::styled("", dim)),
                Line::from(Span::styled(
                    "  A spawn into another workspace records its hand-off here,",
                    dim,
                )),
                Line::from(Span::styled(
                    "  and `create_work` / `lazybox work add` file one directly.",
                    dim,
                )),
            ],
            (None, Some(entries)) => {
                let max = max_scroll(entries.len(), self.body_height as u16) as usize;
                if self.scroll > max {
                    self.scroll = max;
                }
                entries
                    .iter()
                    .enumerate()
                    .skip(self.scroll)
                    .take(self.body_height)
                    .map(|(i, entry)| match entry {
                        WorkRollupEntry::Header {
                            title,
                            count,
                            members,
                            ..
                        } => Self::header_line(title, *count, *members, theme),
                        WorkRollupEntry::Task(task) => {
                            Self::task_line(task, Some(i) == self.cursor, theme)
                        }
                    })
                    .collect()
            }
        };
        frame.render_widget(Paragraph::new(lines), body_area);

        let hint = if self.undecodable > 0 {
            // Named on the hint line rather than as a row: it is a fact
            // about the read, not a unit of work.
            format!(
                "{} row(s) this build could not decode · ↑/↓ navigate · Enter jump · any other key to close",
                self.undecodable
            )
        } else {
            "↑/↓ navigate · Enter jump · any other key to close".to_string()
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

impl AppComponent<Msg, UserEvent> for WorkRollup {
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
            } => match self.selected_jump() {
                Some(key) => Some(Msg::EpicJumpToWorkspace(key)),
                // Enter on a row owned by a human, by lazybox, or by a
                // workspace that is gone closes rather than jumping
                // somewhere arbitrary.
                None => Some(Msg::ModalDismissed),
            },
            // Ctrl-C and any other key close.
            _ => Some(Msg::ModalDismissed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::event::KeyModifiers;

    fn ke(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn task(
        kind: WorkLifeKind,
        title: &str,
        owner: &str,
        detail: Option<&str>,
        jump: bool,
    ) -> WorkRollupEntry {
        task_from(kind, title, owner, None, detail, jump)
    }

    fn task_from(
        kind: WorkLifeKind,
        title: &str,
        owner: &str,
        requester: Option<&str>,
        detail: Option<&str>,
        jump: bool,
    ) -> WorkRollupEntry {
        WorkRollupEntry::Task(WorkTaskRow {
            kind,
            title: title.to_string(),
            owner: owner.to_string(),
            requester: requester.map(str::to_string),
            detail: detail.map(str::to_string),
            jump: jump.then(|| lazybox_core::SessionKey::new(owner.to_string())),
        })
    }

    /// A plan with three tasks, then the unplanned bucket with one.
    fn entries() -> Vec<WorkRollupEntry> {
        vec![
            WorkRollupEntry::Header {
                title: "shim hardening".to_string(),
                is_plan: true,
                count: HeaderCount::Fraction(1, 3),
                members: 2,
            },
            task(
                WorkLifeKind::Completed,
                "bound the daemon dial",
                "o/r#1944",
                None,
                true,
            ),
            task_from(
                WorkLifeKind::Asking,
                "exec budget",
                "o/r#1945",
                Some("o/r#1944"),
                Some("which bound wins?"),
                true,
            ),
            task(
                WorkLifeKind::Held,
                "the view",
                "o/r#1946",
                Some("needs #1945"),
                true,
            ),
            WorkRollupEntry::Header {
                title: "on no plan".to_string(),
                is_plan: false,
                count: HeaderCount::Open(1),
                members: 1,
            },
            task(WorkLifeKind::Underway, "a loose end", "human", None, false),
        ]
    }

    fn render(comp: &mut WorkRollup, w: u16, h: u16) -> String {
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
    fn the_rollup_lists_plans_with_their_tasks_and_what_holds_them() {
        let mut comp = WorkRollup::resolved(entries(), 0, None);
        let out = render(&mut comp, 96, 20);
        // Title counts plans and tasks — the unplanned bucket has a
        // roll-up of its own but is not a plan somebody filed.
        assert!(out.contains("Work · 1 plans · 4 tasks"), "{out}");
        // The requester is on the row, so the view says who is owed the
        // answer and not only who is working.
        assert!(out.contains("← o/r#1944"), "requester: {out}");
        // The plan header carries its fraction and member count.
        assert!(out.contains("1/3"), "roll-up fraction: {out}");
        assert!(out.contains("shim hardening"), "{out}");
        assert!(out.contains("2 workspaces"), "member count: {out}");
        // Every lifecycle glyph in the fixture renders.
        assert!(out.contains('✓'), "completed glyph: {out}");
        assert!(out.contains('?'), "asking glyph: {out}");
        assert!(out.contains('⏸'), "held glyph: {out}");
        assert!(out.contains('▶'), "underway glyph: {out}");
        // The detail is the point of an asking / held row: it says what
        // the work is actually stuck on.
        assert!(out.contains("which bound wins?"), "{out}");
        assert!(out.contains("needs #1945"), "{out}");
        // Owners are named, so the view answers "who has this".
        assert!(out.contains("o/r#1944"), "{out}");
        // The unplanned bucket is its own section, counted as open rows
        // rather than as a fraction it cannot honestly have.
        assert!(out.contains("on no plan"), "{out}");
        assert!(out.contains("1 open"), "open count: {out}");
    }

    #[test]
    fn the_cursor_only_ever_stands_on_a_task_row() {
        let mut comp = WorkRollup::resolved(entries(), 0, None);
        // Starts on the first task, not on the header above it.
        assert_eq!(comp.cursor, Some(1));
        // Walking down crosses the unplanned header without stopping on it.
        comp.on(&ke(Key::Down));
        assert_eq!(comp.cursor, Some(2));
        comp.on(&ke(Key::Down));
        assert_eq!(comp.cursor, Some(3));
        comp.on(&ke(Key::Down));
        assert_eq!(comp.cursor, Some(5), "skipped the header at 4");
        // Past the end stays put.
        comp.on(&ke(Key::Down));
        assert_eq!(comp.cursor, Some(5));
        // And back up again, skipping the header the other way.
        comp.on(&ke(Key::Up));
        assert_eq!(comp.cursor, Some(3));
        for _ in 0..10 {
            comp.on(&ke(Key::Up));
        }
        assert_eq!(comp.cursor, Some(1), "clamped at the first task");
    }

    #[test]
    fn enter_jumps_to_the_owning_workspace() {
        let mut comp = WorkRollup::resolved(entries(), 0, None);
        comp.on(&ke(Key::Down));
        assert_eq!(
            comp.on(&ke(Key::Enter)),
            Some(Msg::EpicJumpToWorkspace(lazybox_core::SessionKey::new(
                "o/r#1945"
            ))),
        );
    }

    #[test]
    fn enter_on_a_row_with_no_workspace_closes_instead_of_jumping() {
        let mut comp = WorkRollup::resolved(entries(), 0, None);
        // Walk to the `human`-owned row, which resolved to no workspace.
        for _ in 0..4 {
            comp.on(&ke(Key::Down));
        }
        assert_eq!(comp.cursor, Some(5));
        assert_eq!(comp.on(&ke(Key::Enter)), Some(Msg::ModalDismissed));
    }

    #[test]
    fn the_loading_state_says_so_and_enter_is_safe() {
        let mut comp = WorkRollup::loading();
        let out = render(&mut comp, 80, 12);
        assert!(out.contains("reading…"), "title: {out}");
        assert!(out.contains("Reading the work store"), "{out}");
        // No rows to stand on, so Enter closes rather than panicking.
        assert_eq!(comp.cursor, None);
        assert_eq!(comp.on(&ke(Key::Enter)), Some(Msg::ModalDismissed));
        assert_eq!(comp.on(&ke(Key::Down)), None);
    }

    #[test]
    fn an_empty_store_explains_how_rows_get_there() {
        let mut comp = WorkRollup::resolved(Vec::new(), 0, None);
        let out = render(&mut comp, 80, 14);
        assert!(out.contains("Nothing in the work store yet"), "{out}");
        // The empty state names where rows come from, so an empty view
        // isn't read as a broken one.
        assert!(out.contains("records its hand-off"), "{out}");
    }

    #[test]
    fn a_failed_read_shows_the_reason_not_an_empty_store() {
        let mut comp = WorkRollup::resolved(
            Vec::new(),
            0,
            Some("the work store could not be read: locked".to_string()),
        );
        let out = render(&mut comp, 80, 12);
        assert!(out.contains("could not be read: locked"), "{out}");
        assert!(
            !out.contains("Nothing in the work store yet"),
            "an unreadable store must not read as an empty one: {out}"
        );
    }

    #[test]
    fn undecodable_rows_are_named_rather_than_hidden() {
        let mut comp = WorkRollup::resolved(entries(), 2, None);
        let out = render(&mut comp, 96, 20);
        assert!(
            out.contains("2 row(s) this build could not decode"),
            "{out}"
        );
    }

    /// A lifecycle this build has not heard of must still be a row.
    #[test]
    fn an_unknown_lifecycle_label_still_renders() {
        assert_eq!(
            WorkLifeKind::from_label("something-new"),
            WorkLifeKind::Pending,
        );
        let mut comp = WorkRollup::resolved(
            vec![task(
                WorkLifeKind::from_label("something-new"),
                "from a newer daemon",
                "ws",
                None,
                false,
            )],
            0,
            None,
        );
        let out = render(&mut comp, 80, 10);
        assert!(out.contains("from a newer daemon"), "{out}");
        assert!(out.contains("1 tasks"), "counted: {out}");
    }

    /// Every label the store can emit maps to a distinct glyph except the
    /// two that are deliberately the same quiet dot.
    #[test]
    fn every_stored_lifecycle_label_is_mapped() {
        use WorkLifeKind::*;
        for (label, want) in [
            ("pending", Pending),
            ("underway", Underway),
            ("awaiting answer", Asking),
            ("held", Held),
            ("completed", Completed),
            ("failed", Failed),
            ("canceled", Pending),
        ] {
            assert_eq!(
                WorkLifeKind::from_label(label),
                want,
                "`{label}` — keep this in step with lazybox_core::work::Lifecycle::label",
            );
        }
    }

    #[test]
    fn the_meter_fills_and_clamps() {
        assert_eq!(meter(0, 3), "▱▱▱▱▱");
        assert_eq!(meter(3, 3), "▰▰▰▰▰");
        // An empty plan has no filled cell — it is unstarted, not done.
        assert_eq!(meter(0, 0), "▱▱▱▱▱");
        // Partial work always shows at least one filled cell, so "started"
        // is visible at a glance.
        assert_eq!(meter(1, 100), "▰▱▱▱▱");
        // Nonsense off the wire is clamped rather than overflowing the bar.
        assert_eq!(meter(9, 3), "▰▰▰▰▰");
    }

    /// Scrolling to a plan's first task pulls its header into view, so a
    /// row is never read under the wrong plan.
    #[test]
    fn the_header_follows_its_first_task_into_view() {
        let mut comp = WorkRollup::resolved(entries(), 0, None);
        comp.body_height = 2;
        comp.scroll = 4;
        comp.cursor = Some(1);
        comp.follow_cursor();
        assert_eq!(comp.scroll, 0, "the header at 0 came with its task at 1");
    }
}
