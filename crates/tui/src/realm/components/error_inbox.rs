//! `ErrorInbox` — the durable, structured error viewer (default
//! `Shift-E`, #831).
//!
//! Where the messages log (`Shift-M`) is the session-only stream of
//! footer notices, this window renders the daemon's *persisted* error
//! store: every error-severity event, deduplicated into counted
//! classes that survive restart. Rows are sorted by frequency (what
//! actually hurts), the selected row's full record — humanized message,
//! raw diagnostic, workspace/operation context, first/last-seen window
//! — shows in a detail panel, and the viewer closes the loop with three
//! actions: file a pre-filled GitHub issue, route the class to an
//! agent, or export the (filtered) set as JSONL.
//!
//! The records are a snapshot pushed by the daemon in reply to
//! `Command::ListErrors`; `now` is captured at build so relative ages
//! don't drift while the window is open.

use crate::realm::components::scrollable::{centered_rect, draw_frame, max_scroll};
use crate::realm::{Msg, UserEvent};
use chrono::{DateTime, Utc};
use lazybox_core::time::time_ago_at;
use lazybox_ipc::ErrorInboxRecord;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;
use tuirealm::state::State;

/// Durable error-inbox window.
pub(crate) struct ErrorInbox {
    /// Deduplicated records, sorted most-frequent-first.
    records: Vec<ErrorInboxRecord>,
    /// Reference instant for relative-time rendering.
    now: DateTime<Utc>,
    /// Cursor into the *filtered* view.
    selected: usize,
    /// Topmost visible list line.
    scroll: u16,
    /// List viewport height, cached in `view` for scroll math.
    list_height: u16,
    /// Active `source` filter, or `None` for all sources.
    source_filter: Option<String>,
    /// The daemon hasn't answered `ListErrors` yet — distinguishes
    /// "loading" from a genuinely empty inbox.
    loading: bool,
}

impl ErrorInbox {
    pub(crate) fn new(
        mut records: Vec<ErrorInboxRecord>,
        now: DateTime<Utc>,
        loading: bool,
    ) -> Self {
        sort_by_frequency(&mut records);
        Self {
            records,
            now,
            selected: 0,
            scroll: 0,
            list_height: 0,
            source_filter: None,
            loading,
        }
    }

    /// Records passing the active source filter, in display order.
    fn filtered(&self) -> Vec<&ErrorInboxRecord> {
        self.records
            .iter()
            .filter(|r| self.source_filter.as_ref().is_none_or(|s| &r.source == s))
            .collect()
    }

    /// The record under the cursor, if any.
    fn selected_record(&self) -> Option<ErrorInboxRecord> {
        self.filtered().get(self.selected).map(|r| (*r).clone())
    }

    /// Distinct sources in first-seen display order, for the `f` cycle.
    fn sources(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for r in &self.records {
            if !seen.contains(&r.source) {
                seen.push(r.source.clone());
            }
        }
        seen
    }

    /// Cycle the source filter: All → each distinct source → All.
    fn cycle_filter(&mut self) {
        let sources = self.sources();
        let next = match &self.source_filter {
            None => sources.first().cloned(),
            Some(cur) => {
                let idx = sources.iter().position(|s| s == cur);
                match idx {
                    Some(i) if i + 1 < sources.len() => Some(sources[i + 1].clone()),
                    _ => None,
                }
            }
        };
        self.source_filter = next;
        self.selected = 0;
        self.scroll = 0;
    }

    fn move_selection(&mut self, delta: i64) {
        let len = self.filtered().len();
        if len == 0 {
            return;
        }
        let cur = self.selected as i64;
        self.selected = cur.saturating_add(delta).clamp(0, len as i64 - 1) as usize;
        // Keep the cursor inside the viewport.
        let sel = self.selected as u16;
        if sel < self.scroll {
            self.scroll = sel;
        } else if self.list_height > 0 && sel >= self.scroll + self.list_height {
            self.scroll = sel - self.list_height + 1;
        }
    }

    /// One summary row per record: `×N  ✗ message  · source · 2m ago`.
    fn list_lines(&self, theme: &crate::theme::Theme) -> Vec<Line<'static>> {
        let filtered = self.filtered();
        if filtered.is_empty() {
            let msg = if self.loading {
                "Loading errors…"
            } else if self.source_filter.is_some() {
                "No errors from this source."
            } else {
                "No errors recorded — the inbox is clear."
            };
            return vec![Line::from(Span::styled(
                msg,
                Style::default().fg(theme.text_dim),
            ))];
        }
        filtered
            .iter()
            .enumerate()
            .map(|(i, r)| self.summary_line(i, r, theme))
            .collect()
    }

    fn summary_line(
        &self,
        idx: usize,
        r: &ErrorInboxRecord,
        theme: &crate::theme::Theme,
    ) -> Line<'static> {
        let color = severity_color(&r.severity, theme);
        let count = format!("×{:<4}", r.count);
        let ago = time_ago_at(&r.last_seen, self.now);
        let mut spans = vec![
            Span::styled(count, Style::default().fg(theme.text_dim)),
            Span::styled("✗ ", Style::default().fg(color)),
            Span::styled(r.message.clone(), Style::default().fg(theme.text_strong)),
            Span::styled(
                format!("  ·  {}  ·  {ago}", r.source),
                Style::default().fg(theme.text_dim),
            ),
        ];
        if idx == self.selected {
            // Highlight the cursor row.
            for span in &mut spans {
                span.style = span.style.add_modifier(Modifier::REVERSED);
            }
        }
        Line::from(spans)
    }

    /// Detail lines for the selected record.
    fn detail_lines(&self, theme: &crate::theme::Theme) -> Vec<Line<'static>> {
        let Some(r) = self.selected_record() else {
            return Vec::new();
        };
        let dim = Style::default().fg(theme.text_dim);
        let strong = Style::default().fg(theme.text_strong);
        let mut ctx = format!("{} · {}", r.severity, r.operation.as_deref().unwrap_or("—"),);
        if let Some(ws) = &r.workspace_key {
            ctx.push_str(" · ");
            ctx.push_str(ws);
        }
        vec![
            Line::from(vec![Span::styled(ctx, dim)]),
            Line::from(vec![Span::styled(
                format!(
                    "×{} · first {} · last {}",
                    r.count,
                    time_ago_at(&r.first_seen, self.now),
                    time_ago_at(&r.last_seen, self.now),
                ),
                dim,
            )]),
            Line::from(vec![
                Span::styled("raw  ", dim),
                Span::styled(r.raw.clone(), strong),
            ]),
        ]
    }
}

/// Sort records most-frequent-first, breaking ties by most-recent.
fn sort_by_frequency(records: &mut [ErrorInboxRecord]) {
    records.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| b.last_seen.cmp(&a.last_seen))
    });
}

fn severity_color(severity: &str, theme: &crate::theme::Theme) -> Color {
    match severity {
        "retryable" | "exhausted" => theme.warn,
        _ => theme.error,
    }
}

impl Component for ErrorInbox {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 110u16.min(area.width.saturating_sub(4));
        let modal_h = 28u16.min(area.height.saturating_sub(2));
        let modal = centered_rect(area, modal_w, modal_h);
        let inner = draw_frame(frame, modal, " Error Inbox ", theme);
        if inner.height < 4 {
            return;
        }

        // Count the *visible* set so an active source filter's header
        // agrees with the list under it — otherwise "N classes · M errors"
        // reports the whole store while the list shows a subset.
        let visible = self.filtered();
        let total: u64 = visible.iter().map(|r| r.count).sum();
        let header = format!(
            "{} classes · {total} errors{}",
            visible.len(),
            match &self.source_filter {
                Some(s) => format!("  ·  source: {s}"),
                None => String::new(),
            },
        );
        let detail = self.detail_lines(theme);
        let detail_h = if detail.is_empty() {
            0
        } else {
            detail.len() as u16 + 1
        };
        // Rows: 1 header + list + detail + 1 hint.
        let list_h = inner.height.saturating_sub(2 + detail_h).max(1);
        self.list_height = list_h;

        let header_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        };
        let list_area = Rect {
            x: inner.x,
            y: inner.y + 1,
            width: inner.width,
            height: list_h,
        };
        let detail_area = Rect {
            x: inner.x,
            y: inner.y + 1 + list_h,
            width: inner.width,
            height: detail_h,
        };
        let hint_area = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(header, theme.hint()))),
            header_area,
        );

        let lines = self.list_lines(theme);
        let max = max_scroll(lines.len(), self.list_height);
        if self.scroll > max {
            self.scroll = max;
        }
        frame.render_widget(Paragraph::new(lines).scroll((self.scroll, 0)), list_area);

        if detail_h > 0 {
            // A dim rule above the detail block.
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "─".repeat(detail_area.width as usize),
                    Style::default().fg(theme.text_dim),
                ))),
                Rect {
                    height: 1,
                    ..detail_area
                },
            );
            frame.render_widget(
                Paragraph::new(detail),
                Rect {
                    y: detail_area.y + 1,
                    height: detail_area.height.saturating_sub(1),
                    ..detail_area
                },
            );
        }

        let hint = if self.filtered().is_empty() {
            "f filter · esc close"
        } else {
            "j/k move · i issue · a → agent · x export · d delete · c clear · f filter · esc close"
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

impl AppComponent<Msg, UserEvent> for ErrorInbox {
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
            Key::Char('f') => {
                self.cycle_filter();
                None
            }
            Key::Char('i') => self.selected_record().map(Msg::ErrorInboxFileIssue),
            Key::Char('a') => self.selected_record().map(Msg::ErrorInboxRouteToAgent),
            Key::Char('x') => {
                let set: Vec<ErrorInboxRecord> = self.filtered().into_iter().cloned().collect();
                if set.is_empty() {
                    None
                } else {
                    Some(Msg::ErrorInboxExport(set))
                }
            }
            Key::Char('d') => self
                .selected_record()
                .map(|r| Msg::ErrorInboxDeleteRequested(r.dedupe_key)),
            Key::Char('c') if !self.records.is_empty() => Some(Msg::ErrorInboxClearRequested),
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
    use chrono::TimeZone;
    use tuirealm::event::{KeyEvent, KeyModifiers};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 5, 12, 0, 0).unwrap()
    }

    fn rec(dedupe: &str, source: &str, count: u64, secs_ago: i64) -> ErrorInboxRecord {
        ErrorInboxRecord {
            dedupe_key: dedupe.into(),
            source: source.into(),
            severity: "permanent".into(),
            operation: Some("merge".into()),
            workspace_key: Some("github:o/r#1".into()),
            message: format!("✗ {dedupe}"),
            raw: format!("raw {dedupe}"),
            count,
            first_seen: now() - chrono::Duration::seconds(secs_ago + 100),
            last_seen: now() - chrono::Duration::seconds(secs_ago),
        }
    }

    fn key(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn render(comp: &mut ErrorInbox, w: u16, h: u16) -> String {
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
    fn sorts_most_frequent_first() {
        let comp = ErrorInbox::new(
            vec![rec("a", "github", 3, 10), rec("b", "linear", 50, 20)],
            now(),
            false,
        );
        assert_eq!(comp.records[0].dedupe_key, "b", "×50 sorts above ×3");
    }

    #[test]
    fn empty_inbox_shows_clear_placeholder_not_loading() {
        let mut comp = ErrorInbox::new(vec![], now(), false);
        let out = render(&mut comp, 100, 16);
        assert!(out.contains("inbox is clear"), "{out}");
        assert!(out.contains("Error Inbox"), "{out}");
    }

    #[test]
    fn renders_count_source_and_detail_for_selected() {
        let mut comp = ErrorInbox::new(vec![rec("rate limited", "github", 500, 30)], now(), false);
        let out = render(&mut comp, 100, 20);
        assert!(out.contains("×500"), "{out}");
        assert!(out.contains("1 classes · 500 errors"), "{out}");
        // Detail panel shows the raw diagnostic + workspace context.
        assert!(out.contains("raw rate limited"), "{out}");
        assert!(out.contains("github:o/r#1"), "{out}");
    }

    #[test]
    fn header_counts_reflect_active_filter() {
        let mut comp = ErrorInbox::new(
            vec![rec("a", "github", 3, 10), rec("b", "linear", 50, 20)],
            now(),
            false,
        );
        // Unfiltered: both classes, 53 total.
        assert!(render(&mut comp, 100, 18).contains("2 classes · 53 errors"));
        // First cycle lands on the most-frequent source (linear, ×50,
        // sorted first). The header must agree with the one-row list, not
        // keep reporting the whole store.
        comp.cycle_filter();
        assert_eq!(comp.source_filter.as_deref(), Some("linear"));
        let out = render(&mut comp, 100, 18);
        assert!(out.contains("1 classes · 50 errors"), "{out}");
        assert!(out.contains("source: linear"), "{out}");
    }

    #[test]
    fn filter_cycles_through_sources_and_back() {
        let mut comp = ErrorInbox::new(
            vec![rec("a", "github", 3, 10), rec("b", "linear", 2, 20)],
            now(),
            false,
        );
        assert_eq!(comp.filtered().len(), 2);
        comp.cycle_filter();
        assert_eq!(comp.source_filter.as_deref(), Some("github"));
        assert_eq!(comp.filtered().len(), 1);
        comp.cycle_filter();
        assert_eq!(comp.source_filter.as_deref(), Some("linear"));
        comp.cycle_filter();
        assert_eq!(comp.source_filter, None, "wraps back to all");
    }

    #[test]
    fn action_keys_emit_messages_for_the_selected_row() {
        let mut comp = ErrorInbox::new(vec![rec("boom", "github", 1, 5)], now(), false);
        let _ = render(&mut comp, 100, 18);
        assert!(matches!(
            comp.on(&key(Key::Char('i'))),
            Some(Msg::ErrorInboxFileIssue(_))
        ));
        assert!(matches!(
            comp.on(&key(Key::Char('a'))),
            Some(Msg::ErrorInboxRouteToAgent(_))
        ));
        assert_eq!(
            comp.on(&key(Key::Char('d'))),
            Some(Msg::ErrorInboxDeleteRequested("boom".into()))
        );
        assert_eq!(
            comp.on(&key(Key::Char('c'))),
            Some(Msg::ErrorInboxClearRequested)
        );
        assert!(matches!(
            comp.on(&key(Key::Char('x'))),
            Some(Msg::ErrorInboxExport(_))
        ));
    }

    #[test]
    fn only_explicit_exit_dismisses() {
        let mut comp = ErrorInbox::new(vec![rec("boom", "github", 1, 5)], now(), false);
        // A stray unbound key must not close an action-bearing viewer.
        assert_eq!(comp.on(&key(Key::Char('z'))), None);
        assert_eq!(comp.on(&key(Key::Enter)), None);
        assert_eq!(comp.on(&key(Key::Esc)), Some(Msg::ModalDismissed));
    }
}
