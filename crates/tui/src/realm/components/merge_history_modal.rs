//! `MergeHistoryModal` — the repo merge-history ledger (#1432).
//!
//! A "what's been landing here" view: press `g h` on any repo and this
//! two-pane modal lists that repo's recently-merged PRs (newest first)
//! on the left with a live body preview on the right. `Enter` opens the
//! full scrollable markdown reader ([`super::markdown_modal::MarkdownModal`],
//! the same `d` reader used elsewhere); `o` opens the highlighted PR in
//! the browser. Repo-scoped, from `Sidebar::cursor_repo()`.
//!
//! The rows come from the daemon: mounting sends
//! `Command::FetchRepoMergeHistory` and the modal renders a "fetching…"
//! state until `Event::RepoMergeHistory` arrives and remounts it with
//! the result (or an error). Merged PRs aren't tracked by the poll, so
//! there is no offline cache to seed from — hence the loading state.
//!
//! Modal returns:
//! - [`Msg::MergeHistoryReadBody`] — `Enter` on a row: open its full body
//!   in the markdown reader.
//! - [`Msg::OpenUrl`] — `o` on a row: open its PR page in the browser
//!   (reuses the description-reader's link handler).
//! - [`Msg::ModalDismissed`] — `Esc` / `Ctrl-C`.

use crate::components::comment_render::wrap_one;
use crate::realm::{Msg, UserEvent};
use crate::theme::Theme;
use chrono::{DateTime, Utc};
use lazybox_core::Task;
use lazybox_core::time::time_ago_at;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyEvent, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tuirealm::state::State;

/// One merged-PR row, projected from a `Task` at mount so the render path
/// doesn't re-derive display strings every frame.
struct MergeRow {
    number: Option<u64>,
    title: String,
    author: String,
    merged_ago: String,
    url: String,
    body: String,
}

/// When a PR landed: a merge closes it, so `closed_at` is the merge time;
/// fall back to `updated_at` for the rare snapshot without one. Used both to
/// label rows and to order them, so the two agree.
fn merge_time(task: &Task) -> DateTime<Utc> {
    task.closed_at.unwrap_or(task.updated_at)
}

impl MergeRow {
    fn from_task(task: &Task, now: DateTime<Utc>) -> Self {
        let merged = merge_time(task);
        Self {
            number: task.id.number(),
            title: task.title.clone(),
            author: task.author.clone(),
            merged_ago: time_ago_at(&merged, now),
            url: task.url.clone(),
            body: task.body.clone().unwrap_or_default(),
        }
    }

    /// The markdown-reader title for this row, e.g. `o/repo#123`.
    fn reader_title(&self, repo: &str) -> String {
        match self.number {
            Some(n) => format!("{repo}#{n}"),
            None => repo.to_string(),
        }
    }
}

pub(crate) struct MergeHistoryModal {
    repo: String,
    /// `None` until the daemon replies — the "fetching…" state.
    rows: Option<Vec<MergeRow>>,
    /// Set when the fetch failed, so the modal shows why instead of an
    /// empty "no merges" pane.
    error: Option<String>,
    /// Cursor into `rows`; meaningless while loading / empty.
    cursor: usize,
    /// Topmost visible list row, kept in step with the cursor in `view`.
    list_scroll: usize,
}

impl MergeHistoryModal {
    /// The pre-reply loading state: mounted immediately so the user sees
    /// the modal open while the fetch is in flight.
    pub(crate) fn loading(repo: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            rows: None,
            error: None,
            cursor: 0,
            list_scroll: 0,
        }
    }

    /// The resolved state, built when `Event::RepoMergeHistory` lands.
    pub(crate) fn resolved(
        repo: impl Into<String>,
        entries: &[Task],
        error: Option<String>,
        now: DateTime<Utc>,
    ) -> Self {
        // The fetch orders by `updated`, but a merged PR's `updated` bumps on
        // later comments — so order by merge time here, keeping display order
        // consistent with each row's "merged X ago" label and the newest
        // landing on top regardless of post-merge chatter.
        let mut ordered: Vec<&Task> = entries.iter().collect();
        ordered.sort_by(|a, b| merge_time(b).cmp(&merge_time(a)));
        Self {
            repo: repo.into(),
            rows: Some(
                ordered
                    .iter()
                    .map(|t| MergeRow::from_task(t, now))
                    .collect(),
            ),
            error,
            cursor: 0,
            list_scroll: 0,
        }
    }

    fn selected(&self) -> Option<&MergeRow> {
        self.rows.as_ref()?.get(self.cursor)
    }

    fn move_cursor(&mut self, delta: i64) {
        let Some(rows) = self.rows.as_ref() else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        let last = rows.len() as i64 - 1;
        self.cursor = (self.cursor as i64 + delta).clamp(0, last) as usize;
    }

    pub(crate) fn on_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == Key::Char('c') {
            return Some(Msg::ModalDismissed);
        }
        match key.code {
            Key::Esc => Some(Msg::ModalDismissed),
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
                if let Some(rows) = self.rows.as_ref() {
                    self.cursor = rows.len().saturating_sub(1);
                }
                None
            }
            Key::Enter => self.selected().map(|row| Msg::MergeHistoryReadBody {
                title: row.reader_title(&self.repo),
                body: row.body.clone(),
            }),
            Key::Char('o') => self.selected().map(|row| Msg::OpenUrl(row.url.clone())),
            _ => None,
        }
    }

    /// Render the left list, scrolling to keep the cursor in view.
    fn render_list(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, rows: &[MergeRow]) {
        let h = area.height.max(1) as usize;
        if self.cursor < self.list_scroll {
            self.list_scroll = self.cursor;
        } else if self.cursor >= self.list_scroll + h {
            self.list_scroll = self.cursor + 1 - h;
        }
        let max_scroll = rows.len().saturating_sub(h);
        if self.list_scroll > max_scroll {
            self.list_scroll = max_scroll;
        }

        let mut lines: Vec<Line> = Vec::with_capacity(h);
        for (i, row) in rows.iter().enumerate().skip(self.list_scroll).take(h) {
            let is_cursor = i == self.cursor;
            let bg = |s: Style| if is_cursor { s.bg(theme.fill) } else { s };
            let caret = if is_cursor { "▸ " } else { "  " };
            let number = match row.number {
                Some(n) => format!("#{n} "),
                None => String::new(),
            };
            let mut title = Style::default().fg(theme.text_strong);
            if is_cursor {
                title = title.add_modifier(Modifier::BOLD);
            }
            lines.push(Line::from(vec![
                Span::styled(
                    caret.to_string(),
                    bg(Style::default().fg(theme.text_strong)),
                ),
                Span::styled(number, bg(Style::default().fg(theme.accent))),
                Span::styled(row.title.clone(), bg(title)),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Render the right preview: the highlighted PR's author + merge time
    /// and its full wrapped body.
    fn render_preview(&self, frame: &mut Frame, area: Rect, theme: &Theme, row: &MergeRow) {
        let mut lines: Vec<Line> = Vec::new();
        let heading = match row.number {
            Some(n) => format!("#{n}  {}", row.title),
            None => row.title.clone(),
        };
        lines.extend(wrap_one(
            Line::from(Span::styled(
                heading,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            area.width,
        ));
        let mut meta: Vec<Span> = Vec::new();
        if !row.author.is_empty() {
            meta.push(Span::styled(
                format!("@{}", row.author),
                Style::default().fg(theme.text_dim),
            ));
            meta.push(Span::styled("  ·  ", Style::default().fg(theme.text_dim)));
        }
        meta.push(Span::styled(
            format!("merged {}", row.merged_ago),
            Style::default().fg(theme.text_dim).italic(),
        ));
        lines.push(Line::from(meta));
        lines.push(Line::raw(""));

        let body = if row.body.trim().is_empty() {
            vec![Line::from(Span::styled(
                "(no description)",
                Style::default().fg(theme.text_dim).italic(),
            ))]
        } else {
            row.body
                .lines()
                .flat_map(|raw| {
                    wrap_one(
                        Line::from(Span::styled(
                            raw.to_string(),
                            Style::default().fg(theme.text_dim),
                        )),
                        area.width,
                    )
                })
                .collect()
        };
        lines.extend(body);
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn placeholder(&self, theme: &Theme) -> (String, Color) {
        match (&self.rows, &self.error) {
            (_, Some(err)) => (format!("  {err}"), theme.error),
            (None, None) => ("  fetching recent merges…".to_string(), theme.text_dim),
            (Some(rows), None) if rows.is_empty() => (
                "  no recent merges in this repo".to_string(),
                theme.text_dim,
            ),
            _ => (String::new(), theme.text_dim),
        }
    }
}

impl Component for MergeHistoryModal {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 96u16.min(area.width.saturating_sub(4));
        let modal_h = 28u16.min(area.height.saturating_sub(4));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(
                format!(" {} · merge history ", self.repo),
                theme.modal_title(),
            ))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        if inner.height < 4 || inner.width < 8 {
            return;
        }

        // Body area = everything but the last (help) line.
        let main = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height - 1,
        };

        // A loaded, non-empty list draws two panes; loading / empty / error
        // states fill the body with a single placeholder line.
        let has_rows = self.rows.as_ref().is_some_and(|r| !r.is_empty());
        if has_rows {
            let rows = std::mem::take(&mut self.rows).unwrap_or_default();
            let list_w = if main.width >= 56 {
                (main.width / 2).clamp(24, 48)
            } else {
                main.width
            };
            let list_rect = Rect {
                width: list_w,
                ..main
            };
            self.render_list(frame, list_rect, theme, &rows);
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
                if let Some(row) = rows.get(self.cursor) {
                    let preview_rect = Rect {
                        x: div_x + 2,
                        width: main.width - list_w - 2,
                        ..main
                    };
                    self.render_preview(frame, preview_rect, theme, row);
                }
            }
            self.rows = Some(rows);
        } else {
            let (msg, color) = self.placeholder(theme);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    msg,
                    Style::default().fg(color).italic(),
                ))),
                main,
            );
        }

        // Help line.
        let mut help = vec![
            Span::styled("↑↓", Style::default().fg(theme.accent).bold()),
            Span::raw(" navigate  "),
            Span::styled("Enter", Style::default().fg(theme.success).bold()),
            Span::raw(" read  "),
            Span::styled("o", Style::default().fg(theme.accent).bold()),
            Span::raw(" open in browser  "),
            Span::styled("Esc", Style::default().fg(theme.error).bold()),
            Span::raw(" close"),
        ];
        if !has_rows {
            // Navigation/read are inert without rows; keep only close.
            help = vec![
                Span::styled("Esc", Style::default().fg(theme.error).bold()),
                Span::raw(" close"),
            ];
        }
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

impl AppComponent<Msg, UserEvent> for MergeHistoryModal {
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

    fn task(number: u64, title: &str, author: &str, body: Option<&str>) -> Task {
        Task {
            id: lazybox_core::TaskId {
                source: "github".into(),
                key: format!("o/r#{number}"),
            },
            title: title.into(),
            body: body.map(str::to_string),
            state: lazybox_core::TaskState::Closed,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/o/r/pull/{number}"),
            repo: Some("o/r".into()),
            branch: Some("b".into()),
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: Some(Utc::now()),
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            author: author.into(),
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
            priority: None,
            state_label: None,
        }
    }

    fn ke(code: Key) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn render(comp: &mut MergeHistoryModal, w: u16, h: u16) -> String {
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
    fn loading_state_shows_fetching_and_no_navigation() {
        let mut m = MergeHistoryModal::loading("o/r");
        // No rows: nav keys are inert (no panic), Enter/o produce nothing.
        assert!(m.on_key(&ke(Key::Down)).is_none());
        assert!(m.on_key(&ke(Key::Enter)).is_none());
        assert!(m.on_key(&ke(Key::Char('o'))).is_none());
        let screen = render(&mut m, 96, 20);
        assert!(screen.contains("merge history"), "title: {screen}");
        assert!(screen.contains("fetching"), "loading text: {screen}");
    }

    #[test]
    fn empty_result_shows_no_merges() {
        let mut m = MergeHistoryModal::resolved("o/r", &[], None, Utc::now());
        let screen = render(&mut m, 96, 20);
        assert!(screen.contains("no recent merges"), "{screen}");
    }

    #[test]
    fn error_result_shows_reason() {
        let mut m = MergeHistoryModal::resolved(
            "o/r",
            &[],
            Some("github credentials: not signed in".into()),
            Utc::now(),
        );
        let screen = render(&mut m, 96, 20);
        assert!(screen.contains("not signed in"), "{screen}");
    }

    #[test]
    fn enter_opens_selected_body() {
        let now = Utc::now();
        // Explicit merge times so display order is deterministic (#10 on top).
        let mut first = task(10, "first", "alice", Some("first body"));
        first.closed_at = Some(now);
        let mut second = task(20, "second", "bob", Some("second body"));
        second.closed_at = Some(now - chrono::Duration::days(1));
        let mut m = MergeHistoryModal::resolved("o/r", &[first, second], None, now);
        m.on_key(&ke(Key::Down));
        match m.on_key(&ke(Key::Enter)) {
            Some(Msg::MergeHistoryReadBody { title, body }) => {
                assert_eq!(title, "o/r#20");
                assert_eq!(body, "second body");
            }
            other => panic!("expected read-body, got {other:?}"),
        }
    }

    #[test]
    fn o_opens_selected_url() {
        let mut m = MergeHistoryModal::resolved(
            "o/r",
            &[task(10, "first", "alice", None)],
            None,
            Utc::now(),
        );
        match m.on_key(&ke(Key::Char('o'))) {
            Some(Msg::OpenUrl(url)) => {
                assert_eq!(url, "https://github.com/o/r/pull/10");
            }
            other => panic!("expected open-url, got {other:?}"),
        }
    }

    #[test]
    fn rows_are_ordered_newest_merged_first() {
        let now = Utc::now();
        let mut older = task(1, "older", "a", None);
        older.closed_at = Some(now - chrono::Duration::days(10));
        let mut newer = task(2, "newer", "b", None);
        newer.closed_at = Some(now - chrono::Duration::days(1));
        // Supplied oldest-first (as an `updated`-sorted fetch could); the top
        // row must still be the most-recently-merged PR (#2).
        let mut m = MergeHistoryModal::resolved("o/r", &[older, newer], None, now);
        match m.on_key(&ke(Key::Enter)) {
            Some(Msg::MergeHistoryReadBody { title, .. }) => assert_eq!(title, "o/r#2"),
            other => panic!("expected the newest-merged row on top, got {other:?}"),
        }
    }

    #[test]
    fn esc_dismisses() {
        let mut m = MergeHistoryModal::loading("o/r");
        assert!(matches!(m.on_key(&ke(Key::Esc)), Some(Msg::ModalDismissed)));
    }

    #[test]
    fn cursor_clamps_at_bounds() {
        let mut m = MergeHistoryModal::resolved(
            "o/r",
            &[task(1, "a", "x", None), task(2, "b", "y", None)],
            None,
            Utc::now(),
        );
        // Up at top stays at 0.
        m.on_key(&ke(Key::Up));
        assert_eq!(m.cursor, 0);
        // Down past the end clamps to the last row.
        m.on_key(&ke(Key::Down));
        m.on_key(&ke(Key::Down));
        m.on_key(&ke(Key::Down));
        assert_eq!(m.cursor, 1);
    }

    #[test]
    fn render_lists_rows_and_previews_body() {
        let mut m = MergeHistoryModal::resolved(
            "o/r",
            &[task(
                1364,
                "Space-tier cost metering",
                "antoine",
                Some("the body here"),
            )],
            None,
            Utc::now(),
        );
        let screen = render(&mut m, 96, 20);
        assert!(screen.contains("#1364"), "row number: {screen}");
        assert!(screen.contains("Space-tier"), "row title: {screen}");
        assert!(screen.contains("@antoine"), "author in preview: {screen}");
        assert!(screen.contains("the body here"), "body preview: {screen}");
    }
}
