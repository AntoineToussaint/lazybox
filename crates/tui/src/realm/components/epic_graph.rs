//! `EpicGraph` — the full-screen epic dependency-DAG modal (`E g`, #1524).
//!
//! The layout itself is a pure, ratatui-free function in
//! [`lazybox_tui_core::epic_graph::layout`]: waves become columns, members
//! stack within their wave, and the typed edges are drawn as box-drawing
//! connectors — solid for a `Blocks` dependency, dashed for a
//! `MergeAfter`-only edge. This modal renders that layout (mapping each
//! [`Tone`] to a theme color), tracks the selected member, and navigates the
//! grid: `j/k` (or `↑/↓`) move within a wave-column, `h/l` (`←/→`) move
//! across columns — both wrap. `Enter` jumps to the highlighted workspace,
//! `Esc` closes. Vertical + horizontal scroll follow the selection so it is
//! always on screen even when the graph is larger than the viewport.

use crate::realm::components::scrollable::{centered_rect, draw_frame};
use crate::realm::{Msg, UserEvent};
use lazybox_ipc::EpicSnapshot;
use lazybox_tui_core::epic_graph::{self, GraphLine, Tone};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyEvent};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;
use tuirealm::state::{State, StateValue};

/// The full-screen epic DAG modal.
pub(crate) struct EpicGraph {
    /// Epic display name, for the frame title.
    epic_name: String,
    /// The snapshot laid out on every `view` (width is known only then).
    snapshot: EpicSnapshot,
    /// Member indices (into `snapshot.members`) per wave-column, top to
    /// bottom — the navigation grid. Width-independent, computed once.
    columns: Vec<Vec<usize>>,
    /// Cursor as `(column, row-within-column)`; `None` when the epic has
    /// no members.
    cursor: Option<(usize, usize)>,
    /// Scroll offsets (cells) that keep the selection on screen.
    v_scroll: usize,
    h_scroll: usize,
}

impl EpicGraph {
    pub(crate) fn new(epic_name: impl Into<String>, snapshot: EpicSnapshot) -> Self {
        // The column grid is width-independent (it is derived from waves +
        // snapshot order), so lay out once at a nominal width just to read
        // `columns`; the real render lays out again at the true width.
        let columns = epic_graph::layout(&snapshot, 200, None).columns;
        let cursor = columns
            .iter()
            .position(|c| !c.is_empty())
            .map(|c| (c, 0usize));
        Self {
            epic_name: epic_name.into(),
            snapshot,
            columns,
            cursor,
            v_scroll: 0,
            h_scroll: 0,
        }
    }

    /// Restore the cursor after a live-snapshot rebuild (`refresh_open_epic_modal`).
    pub(crate) fn set_selected(&mut self, member: usize) {
        if let Some((c, col)) = self
            .columns
            .iter()
            .enumerate()
            .find_map(|(c, rows)| rows.iter().position(|&i| i == member).map(|r| (c, r)))
        {
            self.cursor = Some((c, col));
        }
    }

    /// The selected member index into `snapshot.members`, if any.
    fn selected_member(&self) -> Option<usize> {
        self.cursor.map(|(c, r)| self.columns[c][r])
    }

    /// Move within the current wave-column, wrapping top↔bottom.
    fn move_vertical(&mut self, down: bool) {
        let Some((c, r)) = self.cursor else { return };
        let len = self.columns[c].len();
        if len == 0 {
            return;
        }
        let next = if down {
            (r + 1) % len
        } else {
            (r + len - 1) % len
        };
        self.cursor = Some((c, next));
    }

    /// Move across wave-columns, wrapping left↔right and clamping the row
    /// into the destination column (columns can differ in height).
    fn move_horizontal(&mut self, right: bool) {
        let Some((c, r)) = self.cursor else { return };
        let n = self.columns.len();
        if n == 0 {
            return;
        }
        let next_c = if right { (c + 1) % n } else { (c + n - 1) % n };
        let last = self.columns[next_c].len().saturating_sub(1);
        self.cursor = Some((next_c, r.min(last)));
    }

    /// Map a layout [`Tone`] to a concrete style. Monochrome-leaning per the
    /// house taste: red is reserved for a failed node, the rest lean on
    /// accent / dim; the selected node gets the fill band.
    fn tone_style(tone: Tone, theme: &crate::theme::Theme) -> Style {
        match tone {
            Tone::Edge => Style::default().fg(theme.text_dim),
            Tone::MergeAfterEdge => Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::DIM),
            Tone::NodeDone => Style::default().fg(theme.success),
            Tone::NodeFailed => Style::default().fg(theme.error),
            Tone::NodeAsking => Style::default().fg(theme.warn),
            Tone::NodeActive => Style::default().fg(theme.text_strong),
            Tone::NodeHeld => Style::default().fg(theme.warn),
            Tone::NodeWaiting => Style::default().fg(theme.text_dim),
            Tone::Selected => Style::default()
                .fg(theme.text_strong)
                .bg(theme.fill)
                .add_modifier(Modifier::BOLD),
        }
    }

    /// Explode a run-length line into per-cell `(char, tone)` for windowed
    /// horizontal scrolling.
    fn line_cells(line: &GraphLine) -> Vec<(char, Tone)> {
        let mut cells = Vec::new();
        for span in line {
            for ch in span.text.chars() {
                cells.push((ch, span.tone));
            }
        }
        cells
    }

    /// Regroup a `[h_scroll, h_scroll+width)` window of cells back into
    /// same-tone spans, padding short rows with blanks.
    fn window_line(
        cells: &[(char, Tone)],
        h_scroll: usize,
        width: usize,
        theme: &crate::theme::Theme,
    ) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut cur = String::new();
        let mut cur_tone: Option<Tone> = None;
        for x in h_scroll..h_scroll + width {
            let (ch, tone) = cells.get(x).copied().unwrap_or((' ', Tone::Edge));
            match cur_tone {
                Some(t) if t == tone => cur.push(ch),
                Some(t) => {
                    spans.push(Span::styled(
                        std::mem::take(&mut cur),
                        Self::tone_style(t, theme),
                    ));
                    cur.push(ch);
                    cur_tone = Some(tone);
                }
                None => {
                    cur.push(ch);
                    cur_tone = Some(tone);
                }
            }
        }
        if let Some(t) = cur_tone {
            spans.push(Span::styled(cur, Self::tone_style(t, theme)));
        }
        Line::from(spans)
    }
}

impl Component for EpicGraph {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        // Near-full-screen: this is the one modal that wants the room.
        let modal_w = area.width.saturating_sub(4);
        let modal_h = area.height.saturating_sub(2);
        let modal = centered_rect(area, modal_w, modal_h);
        let title = format!(" Epic graph · {} ", self.epic_name);
        let inner = draw_frame(frame, modal, &title, theme);
        if inner.height < 2 {
            return;
        }

        let body = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height - 1,
        };
        let hint = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };

        if self.snapshot.members.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "  This epic has no members yet.",
                    Style::default().fg(theme.text_dim),
                ))),
                body,
            );
            return;
        }

        let dag = epic_graph::layout(&self.snapshot, body.width, self.selected_member());
        let rows: Vec<Vec<(char, Tone)>> = dag.lines.iter().map(Self::line_cells).collect();

        // Locate the selected node (the only Selected-toned cells) so the
        // scroll can follow it.
        let mut sel_pos: Option<(usize, usize)> = None;
        'outer: for (y, row) in rows.iter().enumerate() {
            for (x, (_, tone)) in row.iter().enumerate() {
                if *tone == Tone::Selected {
                    sel_pos = Some((x, y));
                    break 'outer;
                }
            }
        }

        let vh = body.height as usize;
        let vw = body.width as usize;
        // Clamp scroll to content, then pull the selection into view.
        let content_h = rows.len();
        let max_v = content_h.saturating_sub(vh);
        self.v_scroll = self.v_scroll.min(max_v);
        if let Some((sx, sy)) = sel_pos {
            if sy < self.v_scroll {
                self.v_scroll = sy;
            } else if vh > 0 && sy >= self.v_scroll + vh {
                self.v_scroll = sy + 1 - vh;
            }
            if sx < self.h_scroll {
                self.h_scroll = sx;
            } else if vw > 0 && sx >= self.h_scroll + vw {
                self.h_scroll = sx + 1 - vw;
            }
        }

        let lines: Vec<Line<'static>> = rows
            .iter()
            .skip(self.v_scroll)
            .take(vh)
            .map(|cells| Self::window_line(cells, self.h_scroll, vw, theme))
            .collect();
        frame.render_widget(Paragraph::new(lines), body);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "j/k within wave · h/l across · Enter jump · Esc close",
                theme.hint(),
            ))),
            hint,
        );
    }

    fn query(&self, _: Attribute) -> Option<QueryResult<'_>> {
        None
    }
    fn attr(&mut self, _: Attribute, _: AttrValue) {}
    fn state(&self) -> State {
        // Expose the selected member index so `refresh_open_epic_modal` can
        // carry the cursor across a live-snapshot rebuild.
        State::Single(StateValue::Usize(self.selected_member().unwrap_or(0)))
    }
    fn perform(&mut self, _: Cmd) -> CmdResult {
        CmdResult::NoChange
    }
}

impl AppComponent<Msg, UserEvent> for EpicGraph {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        let Event::Keyboard(key) = ev else {
            return None;
        };
        match key {
            KeyEvent {
                code: Key::Down | Key::Char('j'),
                ..
            } => {
                self.move_vertical(true);
                None
            }
            KeyEvent {
                code: Key::Up | Key::Char('k'),
                ..
            } => {
                self.move_vertical(false);
                None
            }
            KeyEvent {
                code: Key::Right | Key::Char('l'),
                ..
            } => {
                self.move_horizontal(true);
                None
            }
            KeyEvent {
                code: Key::Left | Key::Char('h'),
                ..
            } => {
                self.move_horizontal(false);
                None
            }
            KeyEvent {
                code: Key::Enter, ..
            } => self
                .selected_member()
                .and_then(|i| self.snapshot.members.get(i))
                .map(|m| Msg::EpicJumpToWorkspace(lazybox_core::SessionKey::from(&m.key)))
                .or(Some(Msg::ModalDismissed)),
            KeyEvent {
                code: Key::Esc, ..
            } => Some(Msg::ModalDismissed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_core::WorkspaceKey;
    use lazybox_ipc::{EpicMember, EpicMemberStatus};
    use tuirealm::event::{KeyModifiers, KeyEvent as Ke};

    fn member(k: &str, wave: u16) -> EpicMember {
        EpicMember {
            key: WorkspaceKey(format!("github:{k}")),
            wave,
            status: EpicMemberStatus::Ready,
            blocked_by: vec![],
            external_blockers: vec![],
            blockers: vec![],
        }
    }

    /// 6-node fixture: waves 0/1/2 hold 1/2/3 members → columns
    /// `[[0],[1,2],[3,4,5]]`.
    fn snapshot() -> EpicSnapshot {
        EpicSnapshot {
            key: "epic".into(),
            name: "Epic".into(),
            members: vec![
                member("o/r#1", 0),
                member("o/r#2", 1),
                member("o/r#3", 1),
                member("o/r#4", 2),
                member("o/r#5", 2),
                member("o/r#6", 2),
            ],
            done: 0,
            total: 6,
            ready: 6,
            blocked: 0,
            asking: 0,
            failing: 0,
            blockers_needing_operator: 0,
            cycle: false,
            critical_path: vec![],
            edges: vec![],
            merge_order: vec![],
            computed_at: 0,
        }
    }

    fn ke(code: Key) -> Event<UserEvent> {
        Event::Keyboard(Ke::new(code, KeyModifiers::NONE))
    }

    fn sess(k: &str) -> lazybox_core::SessionKey {
        lazybox_core::SessionKey::from(&WorkspaceKey(format!("github:{k}")))
    }

    fn render(comp: &mut EpicGraph, w: u16, h: u16) -> String {
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
    fn dag_modal_navigation_wraps_and_selects() {
        let mut comp = EpicGraph::new("Epic", snapshot());
        // Cursor starts on the first non-empty column, top row → member 0.
        assert_eq!(comp.selected_member(), Some(0));

        // Horizontal walk wraps left↔right across the three columns.
        assert_eq!(comp.on(&ke(Key::Char('l'))), None);
        assert_eq!(comp.selected_member(), Some(1)); // col1 row0 = #2
        assert_eq!(comp.on(&ke(Key::Char('l'))), None);
        assert_eq!(comp.selected_member(), Some(3)); // col2 row0 = #4
        assert_eq!(comp.on(&ke(Key::Char('l'))), None);
        assert_eq!(comp.selected_member(), Some(0)); // wrapped back to col0

        // Vertical walk wraps within a column, and Down from the single-row
        // column 0 stays put.
        assert_eq!(comp.on(&ke(Key::Char('j'))), None);
        assert_eq!(comp.selected_member(), Some(0));

        // Into the tall column and cycle it.
        comp.on(&ke(Key::Char('l')));
        comp.on(&ke(Key::Char('l'))); // col2 row0 = #4
        assert_eq!(comp.selected_member(), Some(3));
        assert_eq!(comp.on(&ke(Key::Char('j'))), None);
        assert_eq!(comp.selected_member(), Some(4)); // #5
        assert_eq!(comp.on(&ke(Key::Char('j'))), None);
        assert_eq!(comp.selected_member(), Some(5)); // #6
        assert_eq!(comp.on(&ke(Key::Char('j'))), None);
        assert_eq!(comp.selected_member(), Some(3)); // wrapped to top
        assert_eq!(comp.on(&ke(Key::Char('k'))), None);
        assert_eq!(comp.selected_member(), Some(5)); // wrapped to bottom

        // Moving left from a deep row clamps into the shorter column.
        assert_eq!(comp.on(&ke(Key::Char('h'))), None);
        assert_eq!(comp.selected_member(), Some(2)); // col1 has rows 0,1 → clamp to 1 = #3

        // Enter jumps to the highlighted workspace.
        assert_eq!(
            comp.on(&ke(Key::Enter)),
            Some(Msg::EpicJumpToWorkspace(sess("o/r#3")))
        );
        // Esc closes.
        assert_eq!(comp.on(&ke(Key::Esc)), Some(Msg::ModalDismissed));
    }

    #[test]
    fn renders_nodes_and_hint() {
        let mut comp = EpicGraph::new("Orchestration", snapshot());
        let out = render(&mut comp, 100, 24);
        assert!(out.contains("Epic graph · Orchestration"), "{out}");
        assert!(out.contains("o/r#1"), "wave-0 node drawn: {out}");
        assert!(out.contains("o/r#6"), "wave-2 node drawn: {out}");
        assert!(out.contains("Enter jump"), "hint line: {out}");
    }

    #[test]
    fn empty_epic_is_safe() {
        let mut snap = snapshot();
        snap.members.clear();
        let mut comp = EpicGraph::new("Epic", snap);
        assert_eq!(comp.selected_member(), None);
        // Enter on an empty graph closes rather than jumping.
        assert_eq!(comp.on(&ke(Key::Enter)), Some(Msg::ModalDismissed));
        let out = render(&mut comp, 80, 12);
        assert!(out.contains("no members yet"), "{out}");
    }
}
