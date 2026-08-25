//! `Stats` — the day/week usage view (`Shift-U`, #1339).
//!
//! Where the sidebar header shows current-snapshot numbers (live agents,
//! open workspaces), this window digs into *history*: how many agent
//! sessions, prompts, merges, and turns you racked up — plus the tokens
//! and cost behind them — today or over the last week. The numbers come
//! from the daemon's persisted event accumulator, so they survive the
//! reaping of the workspaces that produced them.
//!
//! The daily rollup is a snapshot pushed by the daemon in reply to
//! `Command::GetStats`; the local calendar day is captured at build so
//! the day/week windows don't drift while the window is open. The
//! Today⇄Week toggle is pure client-side re-aggregation over the same
//! buckets.

use crate::realm::components::scrollable::{centered_rect, draw_frame};
use crate::realm::{Msg, UserEvent};
use chrono::{Duration, NaiveDate};
use lazybox_ipc::{StatBucket, stats};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;
use tuirealm::state::State;

/// Days rendered in the sparkline (this day + the six before it).
const SPARK_DAYS: i64 = 7;

/// Usage-stats window.
pub(crate) struct Stats {
    /// Daily rollup buckets pushed by the daemon.
    buckets: Vec<StatBucket>,
    /// Day strings for `today`, `today-1`, … `today-SPARK_DAYS`, computed
    /// once (index = days-ago) so aggregation never re-formats dates per
    /// bucket per redraw. Local calendar days, matching the daemon's
    /// local-day bucketing so "today/week" is the user's day, not UTC.
    days: Vec<String>,
    /// `true` = this week (last 7 days), `false` = today.
    week: bool,
    /// The daemon hasn't answered `GetStats` yet — distinguishes
    /// "loading" from a genuinely empty history.
    loading: bool,
}

impl Stats {
    pub(crate) fn new(buckets: Vec<StatBucket>, today: NaiveDate, loading: bool) -> Self {
        // Precompute today … today-SPARK_DAYS (inclusive) once.
        let days = (0..=SPARK_DAYS)
            .map(|n| (today - Duration::days(n)).format("%Y-%m-%d").to_string())
            .collect();
        Self {
            buckets,
            days,
            week: false,
            loading,
        }
    }

    /// `YYYY-MM-DD` for `n` days before today (0 = today).
    fn day_offset(&self, n: usize) -> &str {
        &self.days[n]
    }

    /// Whether `day` falls inside the active window (today, or the last
    /// 7 days for the week view).
    fn in_window(&self, day: &str) -> bool {
        if self.week {
            day > self.day_offset(SPARK_DAYS as usize) && day <= self.day_offset(0)
        } else {
            day == self.day_offset(0)
        }
    }

    /// Sum one metric across the active window.
    fn total(&self, metric: &str) -> i64 {
        self.buckets
            .iter()
            .filter(|b| b.metric == metric && self.in_window(&b.day))
            .map(|b| b.value)
            .sum()
    }

    /// Sessions per day, oldest→newest, over the sparkline window.
    fn session_series(&self) -> Vec<i64> {
        (0..SPARK_DAYS as usize)
            .rev()
            .map(|n| {
                let day = self.day_offset(n);
                self.buckets
                    .iter()
                    .filter(|b| b.metric == stats::SESSIONS && b.day == day)
                    .map(|b| b.value)
                    .sum()
            })
            .collect()
    }

    fn toggle_view(&mut self) {
        self.week = !self.week;
    }

    fn body_lines(&self, theme: &crate::theme::Theme) -> Vec<Line<'static>> {
        let dim = Style::default().fg(theme.text_dim);
        let strong = Style::default().fg(theme.text_strong);
        let accent = Style::default().fg(theme.accent);

        if self.loading {
            return vec![Line::from(Span::styled("Loading usage stats…", dim))];
        }

        // The Today | Week tab row — the active tab is accented.
        let (today_style, week_style) = if self.week {
            (dim, accent.add_modifier(Modifier::BOLD))
        } else {
            (accent.add_modifier(Modifier::BOLD), dim)
        };
        let range = if self.week {
            format!(
                "{} → {}",
                self.day_offset(SPARK_DAYS as usize - 1),
                self.day_offset(0)
            )
        } else {
            self.day_offset(0).to_string()
        };
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Today", today_style),
                Span::styled("   ", dim),
                Span::styled("Week", week_style),
                Span::styled(format!("      {range}"), dim),
            ]),
            Line::from(""),
        ];

        let tokens = self.total(stats::INPUT_TOKENS) + self.total(stats::OUTPUT_TOKENS);
        let rows: [(&str, String); 6] = [
            ("Agent sessions", fmt_int(self.total(stats::SESSIONS))),
            ("Prompts sent", fmt_int(self.total(stats::PROMPTS))),
            ("PRs merged", fmt_int(self.total(stats::MERGED))),
            ("Agent turns", fmt_int(self.total(stats::TURNS))),
            ("Tokens", fmt_compact(tokens)),
            ("Cost", fmt_cost(self.total(stats::COST_MICROS))),
        ];
        for (label, value) in rows {
            lines.push(Line::from(vec![
                Span::styled(format!("{label:<16}"), dim),
                Span::styled(value, strong),
            ]));
        }

        // A 7-day sessions sparkline, always over the last week regardless
        // of the active tab — the trend context the single-number tabs lack.
        lines.push(Line::from(""));
        let series = self.session_series();
        lines.push(Line::from(vec![
            Span::styled(format!("{:<16}", "Sessions · 7d"), dim),
            Span::styled(sparkline(&series), accent),
            Span::styled(format!("  ({} total)", series.iter().sum::<i64>()), dim),
        ]));

        lines
    }
}

/// Group an integer with thousands separators: `12345` → `12,345`.
fn fmt_int(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// Compact large counts: `1500` → `1.5k`, `2_300_000` → `2.3M`.
fn fmt_compact(n: i64) -> String {
    match n {
        _ if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        _ if n >= 1_000 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => n.to_string(),
    }
}

/// USD micros (millionths of a dollar) → `$1.23`.
fn fmt_cost(micros: i64) -> String {
    format!("${:.2}", micros as f64 / 1_000_000.0)
}

/// A unicode block sparkline. An all-zero series renders as flat lows so
/// an empty week still reads as a baseline, not a blank gap.
fn sparkline(values: &[i64]) -> String {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = values.iter().copied().max().unwrap_or(0);
    values
        .iter()
        .map(|&v| {
            if max <= 0 {
                BLOCKS[0]
            } else {
                let idx = ((v as f64 / max as f64) * (BLOCKS.len() - 1) as f64).round() as usize;
                BLOCKS[idx.min(BLOCKS.len() - 1)]
            }
        })
        .collect()
}

impl Component for Stats {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 48u16.min(area.width.saturating_sub(4));
        let modal_h = 16u16.min(area.height.saturating_sub(2));
        let modal = centered_rect(area, modal_w, modal_h);
        let inner = draw_frame(frame, modal, " Usage Stats ", theme);
        if inner.height < 3 {
            return;
        }

        let hint_area = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        let body_area = Rect {
            height: inner.height - 1,
            ..inner
        };

        frame.render_widget(Paragraph::new(self.body_lines(theme)), body_area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "tab today/week · esc close",
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

impl AppComponent<Msg, UserEvent> for Stats {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        let Event::Keyboard(key) = ev else {
            return None;
        };
        match key.code {
            Key::Tab | Key::Left | Key::Right | Key::Char('h') | Key::Char('l') => {
                self.toggle_view();
                None
            }
            Key::Char('d') => {
                self.week = false;
                None
            }
            Key::Char('w') => {
                self.week = true;
                None
            }
            Key::Esc | Key::Char('q') => Some(Msg::ModalDismissed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::event::{KeyEvent, KeyModifiers};

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, 25).unwrap()
    }

    fn bucket(day: &str, metric: &str, value: i64) -> StatBucket {
        StatBucket {
            day: day.into(),
            metric: metric.into(),
            value,
        }
    }

    fn key(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn render(comp: &mut Stats, w: u16, h: u16) -> String {
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
    fn today_totals_only_today_week_totals_the_window() {
        // 2 sessions today, 3 sessions six days ago (inside week window),
        // 1 session eight days ago (outside).
        let comp = Stats::new(
            vec![
                bucket("2026-08-25", stats::SESSIONS, 2),
                bucket("2026-08-19", stats::SESSIONS, 3),
                bucket("2026-08-17", stats::SESSIONS, 1),
            ],
            today(),
            false,
        );
        assert_eq!(comp.total(stats::SESSIONS), 2, "today = only 2026-08-25");
        let mut week = comp;
        week.toggle_view();
        assert_eq!(week.total(stats::SESSIONS), 5, "week = today + 6 days back");
    }

    #[test]
    fn renders_labelled_totals_and_cost() {
        let mut comp = Stats::new(
            vec![
                bucket("2026-08-25", stats::SESSIONS, 3),
                bucket("2026-08-25", stats::MERGED, 2),
                bucket("2026-08-25", stats::INPUT_TOKENS, 1200),
                bucket("2026-08-25", stats::OUTPUT_TOKENS, 800),
                bucket("2026-08-25", stats::COST_MICROS, 1_250_000),
            ],
            today(),
            false,
        );
        let out = render(&mut comp, 50, 16);
        assert!(out.contains("Usage Stats"), "{out}");
        assert!(out.contains("Agent sessions"), "{out}");
        assert!(out.contains("PRs merged"), "{out}");
        // input + output tokens combine, compacted.
        assert!(out.contains("2.0k"), "{out}");
        assert!(out.contains("$1.25"), "{out}");
    }

    #[test]
    fn loading_shows_placeholder_not_zeroes() {
        let mut comp = Stats::new(vec![], today(), true);
        let out = render(&mut comp, 50, 16);
        assert!(out.contains("Loading"), "{out}");
        assert!(!out.contains("Agent sessions"), "{out}");
    }

    #[test]
    fn tab_toggles_today_and_week_without_closing() {
        let mut comp = Stats::new(vec![], today(), false);
        assert!(!comp.week);
        assert_eq!(comp.on(&key(Key::Tab)), None);
        assert!(comp.week, "tab switched to week");
        assert_eq!(comp.on(&key(Key::Char('d'))), None);
        assert!(!comp.week, "d returns to today");
        assert_eq!(comp.on(&key(Key::Esc)), Some(Msg::ModalDismissed));
    }

    #[test]
    fn fmt_helpers() {
        assert_eq!(fmt_int(12345), "12,345");
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_compact(2_300_000), "2.3M");
        assert_eq!(fmt_compact(1500), "1.5k");
        assert_eq!(fmt_compact(999), "999");
        assert_eq!(fmt_cost(1_250_000), "$1.25");
    }
}
