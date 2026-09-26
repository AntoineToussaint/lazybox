//! `ArchiveBrowser` — the surface that undoes `x x` (default `x U`, #1824).
//!
//! `x x` deletes the workspace row and writes a tombstone the poll then
//! skips, so an archived record appears in no mailbox: the Inactive mailbox
//! holds rows that still exist. This window is the only place the tombstoned
//! set is visible, and `u` / Enter is the only caller of the daemon's
//! unarchive.
//!
//! A row that absorbed other keys — a PR row standing in for the issues it
//! closes — names them on its own line (`… + <key>, <key>`), because that
//! whole set is what a restore takes back out. They are not separately
//! selectable: nothing can restore an absorbed key without the row that
//! absorbed it.

use crate::realm::components::scrollable::{centered_rect, draw_frame, max_scroll};
use crate::realm::{Msg, UserEvent};
use lazybox_ipc::ArchivedWorkspaceRecord;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;
use tuirealm::state::State;

/// Archived-workspace browser.
pub(crate) struct ArchiveBrowser {
    /// Archived rows, sorted by key (the daemon's order).
    records: Vec<ArchivedWorkspaceRecord>,
    /// Cursor into `records`.
    selected: usize,
    /// Topmost visible list line.
    scroll: u16,
    /// List viewport height, cached in `view` for scroll math.
    list_height: u16,
    /// The daemon hasn't answered `ListArchivedWorkspaces` yet —
    /// distinguishes "loading" from a genuinely empty archive.
    loading: bool,
}

impl ArchiveBrowser {
    pub(crate) fn new(records: Vec<ArchivedWorkspaceRecord>, loading: bool) -> Self {
        Self {
            records,
            selected: 0,
            scroll: 0,
            list_height: 0,
            loading,
        }
    }

    /// The record under the cursor, if any.
    fn selected_record(&self) -> Option<&ArchivedWorkspaceRecord> {
        self.records.get(self.selected)
    }

    fn move_selection(&mut self, delta: i64) {
        if self.records.is_empty() {
            return;
        }
        let cur = self.selected as i64;
        self.selected = cur
            .saturating_add(delta)
            .clamp(0, self.records.len() as i64 - 1) as usize;
        let sel = self.selected as u16;
        if sel < self.scroll {
            self.scroll = sel;
        } else if self.list_height > 0 && sel >= self.scroll + self.list_height {
            self.scroll = sel - self.list_height + 1;
        }
    }

    /// One line per archived row, naming the keys that row absorbed.
    ///
    /// Absorbed keys ride their owner's line rather than getting rows of
    /// their own, so the cursor maps straight to `records[selected]` — the
    /// unit a restore acts on.
    fn list_lines(&self, theme: &crate::theme::Theme) -> Vec<Line<'static>> {
        if self.records.is_empty() {
            let msg = if self.loading {
                "Loading the archive…"
            } else {
                "Nothing archived — `x x` on a row puts it here."
            };
            return vec![Line::from(Span::styled(
                msg,
                Style::default().fg(theme.text_dim),
            ))];
        }
        let mut lines = Vec::with_capacity(self.records.len());
        for (idx, record) in self.records.iter().enumerate() {
            let mut spans = vec![Span::styled(
                record.key.clone(),
                Style::default().fg(theme.text_strong),
            )];
            if !record.absorbed.is_empty() {
                spans.push(Span::styled(
                    format!("  + {}", record.absorbed.join(", ")),
                    Style::default().fg(theme.text_dim),
                ));
            }
            if idx == self.selected {
                for span in &mut spans {
                    span.style = span.style.add_modifier(Modifier::REVERSED);
                }
            }
            lines.push(Line::from(spans));
        }
        lines
    }
}

impl Component for ArchiveBrowser {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 96u16.min(area.width.saturating_sub(4));
        let modal_h = 24u16.min(area.height.saturating_sub(2));
        let modal = centered_rect(area, modal_w, modal_h);
        let inner = draw_frame(frame, modal, " Archived ", theme);
        if inner.height < 3 {
            return;
        }

        let absorbed: usize = self.records.iter().map(|r| r.absorbed.len()).sum();
        let header = match absorbed {
            0 => format!("{} archived", self.records.len()),
            n => format!("{} archived · {n} absorbed", self.records.len()),
        };
        // Rows: 1 header + list + 1 hint.
        let list_h = inner.height.saturating_sub(2).max(1);
        self.list_height = list_h;

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(header, theme.hint()))),
            Rect { height: 1, ..inner },
        );

        let lines = self.list_lines(theme);
        let max = max_scroll(lines.len(), self.list_height);
        if self.scroll > max {
            self.scroll = max;
        }
        frame.render_widget(
            Paragraph::new(lines).scroll((self.scroll, 0)),
            Rect {
                y: inner.y + 1,
                height: list_h,
                ..inner
            },
        );

        let hint = if self.records.is_empty() {
            "esc close"
        } else {
            "j/k move · u restore · esc close"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, theme.hint()))),
            Rect {
                y: inner.y + inner.height - 1,
                height: 1,
                ..inner
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

impl AppComponent<Msg, UserEvent> for ArchiveBrowser {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        let Event::Keyboard(key) = ev else {
            return None;
        };
        match key.code {
            Key::Down | Key::Char('j') => {
                self.move_selection(1);
                None
            }
            Key::Up | Key::Char('k') => {
                self.move_selection(-1);
                None
            }
            Key::Char('u') | Key::Enter => self
                .selected_record()
                .map(|record| Msg::ArchiveRestoreRequested(record.key.clone())),
            // An action-bearing viewer must not close on a stray key —
            // only an explicit exit dismisses; everything else is inert.
            Key::Esc | Key::Char('q') => Some(Msg::ModalDismissed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::event::{KeyEvent, KeyModifiers};

    fn rec(key: &str, absorbed: &[&str]) -> ArchivedWorkspaceRecord {
        ArchivedWorkspaceRecord {
            key: key.into(),
            absorbed: absorbed.iter().map(|k| (*k).to_string()).collect(),
        }
    }

    fn key(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn render(comp: &mut ArchiveBrowser, w: u16, h: u16) -> String {
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
    fn empty_archive_says_so_rather_than_loading() {
        let mut comp = ArchiveBrowser::new(vec![], false);
        let out = render(&mut comp, 90, 14);
        assert!(out.contains("Nothing archived"), "{out}");
        assert!(out.contains("Archived"), "{out}");
    }

    #[test]
    fn loading_is_distinct_from_empty() {
        let mut comp = ArchiveBrowser::new(vec![], true);
        assert!(render(&mut comp, 90, 14).contains("Loading the archive"));
    }

    #[test]
    fn absorbed_keys_render_beside_their_owner() {
        let mut comp = ArchiveBrowser::new(
            vec![rec("github-o-r-42", &["github-o-r-40", "github-o-r-41"])],
            false,
        );
        let out = render(&mut comp, 90, 14);
        assert!(out.contains("github-o-r-42"), "{out}");
        assert!(out.contains("github-o-r-40, github-o-r-41"), "{out}");
        // The header counts the absorbed set the restore also reverses.
        assert!(out.contains("1 archived · 2 absorbed"), "{out}");
    }

    #[test]
    fn restore_names_the_selected_row_not_an_absorbed_key() {
        let mut comp = ArchiveBrowser::new(
            vec![
                rec("github-o-r-42", &["github-o-r-40"]),
                rec("github-o-r-43", &[]),
            ],
            false,
        );
        let _ = render(&mut comp, 90, 14);
        assert_eq!(
            comp.on(&key(Key::Char('u'))),
            Some(Msg::ArchiveRestoreRequested("github-o-r-42".into())),
        );
        comp.move_selection(1);
        assert_eq!(
            comp.on(&key(Key::Enter)),
            Some(Msg::ArchiveRestoreRequested("github-o-r-43".into())),
        );
    }

    #[test]
    fn an_empty_archive_has_nothing_to_restore() {
        let mut comp = ArchiveBrowser::new(vec![], false);
        assert_eq!(comp.on(&key(Key::Char('u'))), None);
        assert_eq!(comp.on(&key(Key::Enter)), None);
    }

    #[test]
    fn only_explicit_exit_dismisses() {
        let mut comp = ArchiveBrowser::new(vec![rec("github-o-r-42", &[])], false);
        assert_eq!(comp.on(&key(Key::Char('z'))), None);
        assert_eq!(comp.on(&key(Key::Esc)), Some(Msg::ModalDismissed));
    }
}
