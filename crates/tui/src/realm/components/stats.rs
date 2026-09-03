//! `Stats` — the day/week usage view (`Shift-U`, #1339; deep-dive #1345).
//!
//! Where the sidebar header shows current-snapshot numbers (live agents,
//! open workspaces), this window digs into *history*: how many agent
//! sessions, prompts, merges, and turns you racked up — plus the tokens
//! and cost behind them — today or over the last week. The numbers come
//! from the daemon's persisted event accumulator, so they survive the
//! reaping of the workspaces that produced them.
//!
//! Phase 3 (#1345) turns the flat six-number card into a sectioned
//! deep-dive: Activity / Output for the active window, a Streaks section
//! and a recent-totals footer computed over the whole shipped window, and
//! the 7-day sparkline. It grew past one screen, so it scrolls with the
//! shared reader protocol.
//!
//! The daily rollup is a snapshot pushed by the daemon in reply to
//! `Command::GetStats`; the local calendar day is captured at build so
//! the day/week windows don't drift while the window is open. The
//! Today⇄Week toggle is pure client-side re-aggregation over the same
//! buckets.

use crate::realm::components::scrollable::{
    centered_rect, draw_frame, handle_scroll_key, max_scroll,
};
use crate::realm::{Msg, UserEvent};
use chrono::{Duration, NaiveDate};
use lazybox_ipc::{StatBucket, stats};
use std::collections::BTreeSet;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;
use tuirealm::state::{State, StateValue};

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
    /// Local calendar day this window was built for — the anchor the
    /// streak walk counts back from.
    today: NaiveDate,
    /// `true` = this week (last 7 days), `false` = today.
    week: bool,
    /// The daemon hasn't answered `GetStats` yet — distinguishes
    /// "loading" from a genuinely empty history.
    loading: bool,
    /// Topmost visible body line — the deep-dive outgrew one screen.
    scroll: u16,
    /// Body viewport height, cached in `view` for page jumps.
    body_height: u16,
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
            today,
            week: false,
            loading,
            scroll: 0,
            body_height: 0,
        }
    }

    /// Preserve the Today⇄Week selection across a data refresh. The daemon
    /// re-pushes the rollup after each accumulator flush (#1344), and the
    /// model repaints an open window by rebuilding it — without this the
    /// user's Week view would snap back to Today on every push.
    pub(crate) fn set_week(&mut self, week: bool) {
        self.week = week;
    }

    /// Preserve the scroll offset across that same rebuild. The deep-dive
    /// scrolls (#1345), and the post-flush push fires repeatedly while the
    /// window is open — without carrying `scroll`, a reader parked on the
    /// Streaks/Totals rows snaps back to the top on every flush. The next
    /// `view` re-clamps a now-too-large offset, so a shrunk rollup is safe.
    pub(crate) fn set_scroll(&mut self, scroll: u16) {
        self.scroll = scroll;
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

    /// Sum one metric across the whole shipped window — the "recent
    /// totals" footer, independent of the Today⇄Week tab.
    fn grand_total(&self, metric: &str) -> i64 {
        self.buckets
            .iter()
            .filter(|b| b.metric == metric)
            .map(|b| b.value)
            .sum()
    }

    /// Distinct local days that saw any activity, over the shipped window.
    /// The basis for both streak numbers and the active-day count.
    fn active_days(&self) -> BTreeSet<NaiveDate> {
        self.buckets
            .iter()
            .filter(|b| b.value > 0)
            .filter_map(|b| NaiveDate::parse_from_str(&b.day, "%Y-%m-%d").ok())
            .collect()
    }

    /// Consecutive active days ending *today* — 0 if today itself is idle,
    /// so the number only reads as "alive" while the streak is unbroken.
    fn current_streak(&self, active: &BTreeSet<NaiveDate>) -> i64 {
        let mut day = self.today;
        let mut n = 0;
        while active.contains(&day) {
            n += 1;
            day -= Duration::days(1);
        }
        n
    }

    /// The longest run of consecutive calendar days in the window. `active`
    /// is a `BTreeSet`, so iteration is date-ascending.
    fn longest_streak(&self, active: &BTreeSet<NaiveDate>) -> i64 {
        let mut best = 0;
        let mut run = 0;
        let mut prev: Option<NaiveDate> = None;
        for &d in active {
            run = match prev {
                Some(p) if d == p + Duration::days(1) => run + 1,
                _ => 1,
            };
            best = best.max(run);
            prev = Some(d);
        }
        best
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

        let header = |label: &str| {
            Line::from(Span::styled(
                label.to_string(),
                accent.add_modifier(Modifier::BOLD),
            ))
        };
        let row = |label: &str, value: String| {
            Line::from(vec![
                Span::styled(format!("  {label:<13}"), dim),
                Span::styled(value, strong),
            ])
        };

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

        // ── Activity, over the active (Today/Week) window ─────────────
        lines.push(header("Activity"));
        lines.push(row("Sessions", fmt_int(self.total(stats::SESSIONS))));
        lines.push(row("Prompts", fmt_int(self.total(stats::PROMPTS))));
        lines.push(row("Agent turns", fmt_int(self.total(stats::TURNS))));
        lines.push(Line::from(""));

        // ── Output — merges plus the tokens/cost split ────────────────
        lines.push(header("Output"));
        lines.push(row("PRs merged", fmt_int(self.total(stats::MERGED))));
        lines.push(row(
            "Tokens in",
            fmt_compact(self.total(stats::INPUT_TOKENS)),
        ));
        lines.push(row(
            "Tokens out",
            fmt_compact(self.total(stats::OUTPUT_TOKENS)),
        ));
        lines.push(row("Cost", fmt_cost(self.total(stats::COST_MICROS))));
        lines.push(Line::from(""));

        // ── Streaks — always over the shipped window, not the tab, since
        // a streak is inherently a multi-day fact. Labelled "recent"
        // because the rollup only ships a bounded window, so a streak
        // older than that is truthfully capped, not silently wrong.
        let active = self.active_days();
        lines.push(header("Streaks · recent"));
        lines.push(row("Current", fmt_days(self.current_streak(&active))));
        lines.push(row("Longest", fmt_days(self.longest_streak(&active))));
        lines.push(row("Active days", fmt_int(active.len() as i64)));
        lines.push(Line::from(""));

        // A 7-day sessions sparkline, always over the last week regardless
        // of the active tab — the trend context the single-number tabs lack.
        let series = self.session_series();
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<14}", "Sessions · 7d"), dim),
            Span::styled(sparkline(&series), accent),
            Span::styled(format!("  ({} total)", series.iter().sum::<i64>()), dim),
        ]));
        lines.push(Line::from(""));

        // ── Recent totals — cumulative over the whole shipped window,
        // the "grand total" the single-window tabs can't show. ────────
        let tokens = self.grand_total(stats::INPUT_TOKENS) + self.grand_total(stats::OUTPUT_TOKENS);
        lines.push(header("Totals · recent"));
        lines.push(row("Sessions", fmt_int(self.grand_total(stats::SESSIONS))));
        lines.push(row("Prompts", fmt_int(self.grand_total(stats::PROMPTS))));
        lines.push(row("PRs merged", fmt_int(self.grand_total(stats::MERGED))));
        lines.push(row("Tokens", fmt_compact(tokens)));
        lines.push(row("Cost", fmt_cost(self.grand_total(stats::COST_MICROS))));

        lines
    }
}

/// Modal height for `content_lines` of body: content + chrome (two borders
/// and one hint row), floored so small content still clears the
/// `inner.height < 3` guard, then capped by the available height so the
/// modal can never exceed the terminal. The cap MUST come last — the
/// original `…min(area).max(6)` applied the floor after the cap and so
/// overflowed a terminal shorter than the floor; flipping the order keeps
/// the floor useful while holding the fits-the-area invariant (the sibling
/// readers hold it too). Above the cap a too-short terminal collapses into
/// the `inner.height < 3` guard, which renders nothing rather than a
/// clipped frame.
fn fit_height(content_lines: usize, area_height: u16) -> u16 {
    (content_lines as u16 + 3)
        .max(6)
        .min(area_height.saturating_sub(2))
}

/// A day count with singular/plural: `1 day`, `3 days`.
fn fmt_days(n: i64) -> String {
    if n == 1 {
        "1 day".to_string()
    } else {
        format!("{n} days")
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
        let lines = self.body_lines(theme);
        let modal_w = 48u16.min(area.width.saturating_sub(4));
        let modal_h = fit_height(lines.len(), area.height);
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
        self.body_height = body_area.height.max(1);

        // Clamp so a short body — or an over-scroll — can't strand blank
        // rows above the last line.
        let max = max_scroll(lines.len(), self.body_height);
        if self.scroll > max {
            self.scroll = max;
        }
        frame.render_widget(Paragraph::new(lines).scroll((self.scroll, 0)), body_area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "tab today/week · ↑/↓ scroll · esc close",
                theme.hint(),
            ))),
            hint_area,
        );
    }

    fn query(&self, _: Attribute) -> Option<QueryResult<'_>> {
        None
    }
    fn attr(&mut self, _: Attribute, _: AttrValue) {}
    /// Expose the Today⇄Week selection *and* the scroll offset so the model
    /// can carry both across a data-refresh rebuild (see [`Stats::set_week`]
    /// / [`Stats::set_scroll`]).
    fn state(&self) -> State {
        State::Vec(vec![
            StateValue::Bool(self.week),
            StateValue::U16(self.scroll),
        ])
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
        // The scroll protocol (Down/Up/j/k/PageDn/PageUp/Ctrl-d/Ctrl-u/
        // Home/g) claims its keys first; none of them overlap the toggle.
        if handle_scroll_key(&mut self.scroll, self.body_height, key) {
            return None;
        }
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
    fn renders_sectioned_deep_dive() {
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
        // Tall enough to show every section without scrolling.
        let out = render(&mut comp, 50, 40);
        assert!(out.contains("Usage Stats"), "{out}");
        // The four deep-dive sections — the window-scoped ones labelled
        // "recent" so their numbers don't read as all-time.
        assert!(out.contains("Activity"), "{out}");
        assert!(out.contains("Output"), "{out}");
        assert!(out.contains("Streaks · recent"), "{out}");
        assert!(out.contains("Totals · recent"), "{out}");
        assert!(out.contains("PRs merged"), "{out}");
        // Tokens split in/out in the active window…
        assert!(out.contains("Tokens in"), "{out}");
        assert!(out.contains("Tokens out"), "{out}");
        // …and recombined (1.2k + 800 = 2.0k) in the recent-totals footer.
        assert!(out.contains("2.0k"), "{out}");
        assert!(out.contains("$1.25"), "{out}");
        // The sparkline bars keep a gap from their label rather than
        // butting straight against the "7d".
        assert!(out.contains("Sessions · 7d ▁"), "{out}");
    }

    #[test]
    fn loading_shows_placeholder_not_zeroes() {
        let mut comp = Stats::new(vec![], today(), true);
        let out = render(&mut comp, 50, 16);
        assert!(out.contains("Loading"), "{out}");
        assert!(!out.contains("Activity"), "{out}");
    }

    /// Streaks count consecutive active calendar days: the current streak
    /// walks back from today and breaks at the first idle day, the longest
    /// is the biggest run anywhere in the window.
    #[test]
    fn streaks_count_consecutive_active_days() {
        // Active: today, -1, -2 (a live 3-day streak), then a gap at -3,
        // then a separate 2-day run at -4/-5.
        let comp = Stats::new(
            vec![
                bucket("2026-08-25", stats::SESSIONS, 1),
                bucket("2026-08-24", stats::PROMPTS, 4),
                bucket("2026-08-23", stats::MERGED, 1),
                // -3 (2026-08-22) idle — a zero bucket must not count.
                bucket("2026-08-22", stats::SESSIONS, 0),
                bucket("2026-08-21", stats::TURNS, 2),
                bucket("2026-08-20", stats::SESSIONS, 1),
            ],
            today(),
            false,
        );
        let active = comp.active_days();
        assert_eq!(active.len(), 5, "the zero-valued day is not active");
        assert_eq!(comp.current_streak(&active), 3, "today, -1, -2");
        assert_eq!(comp.longest_streak(&active), 3, "the live run is longest");
    }

    /// An idle today means no live streak, even with recent activity.
    #[test]
    fn current_streak_is_zero_when_today_is_idle() {
        let comp = Stats::new(
            vec![bucket("2026-08-24", stats::SESSIONS, 1)],
            today(),
            false,
        );
        let active = comp.active_days();
        assert_eq!(comp.current_streak(&active), 0);
        assert_eq!(comp.longest_streak(&active), 1);
    }

    /// The body outgrew one screen, so the reader scrolls — and a scroll
    /// key never leaks out as a toggle or dismiss.
    #[test]
    fn scroll_keys_move_the_body_without_toggling() {
        let mut comp = Stats::new(
            vec![bucket("2026-08-25", stats::SESSIONS, 1)],
            today(),
            false,
        );
        let _ = render(&mut comp, 50, 16);
        assert_eq!(comp.on(&key(Key::Down)), None);
        assert_eq!(comp.scroll, 1, "Down scrolls the body");
        assert!(!comp.week, "scrolling never flips the tab");
        assert_eq!(comp.on(&key(Key::Char('k'))), None);
        assert_eq!(comp.scroll, 0, "k scrolls back up");
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

    /// `state()` exposes both the Week selection and the scroll offset, and
    /// `set_week`/`set_scroll` restore them — the contract `update_stats`
    /// uses to carry the view across a post-flush rebuild (#1344/#1345).
    /// Without the scroll half, a scrolled deep-dive snaps to the top on
    /// every accumulator flush.
    #[test]
    fn state_carries_week_and_scroll_across_a_refresh() {
        use tuirealm::state::{State, StateValue};

        // A live window the user has toggled to Week and scrolled down.
        let mut live = Stats::new(
            vec![bucket("2026-08-25", stats::SESSIONS, 1)],
            today(),
            false,
        );
        let _ = render(&mut live, 50, 16);
        live.on(&key(Key::Char('w')));
        live.on(&key(Key::Down));
        live.on(&key(Key::Down));
        assert_eq!(live.scroll, 2);
        assert_eq!(
            live.state(),
            State::Vec(vec![StateValue::Bool(true), StateValue::U16(2)]),
            "state() must report the tab AND the scroll offset",
        );

        // A post-flush rebuild restores both from that reported state —
        // the exact round-trip update_stats performs.
        let mut rebuilt = Stats::new(vec![], today(), false);
        rebuilt.set_week(true);
        rebuilt.set_scroll(2);
        assert!(rebuilt.week, "week survives the rebuild");
        assert_eq!(rebuilt.scroll, 2, "scroll survives the rebuild");
    }

    #[test]
    fn fmt_helpers() {
        assert_eq!(fmt_int(12345), "12,345");
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_compact(2_300_000), "2.3M");
        assert_eq!(fmt_compact(1500), "1.5k");
        assert_eq!(fmt_compact(999), "999");
        assert_eq!(fmt_cost(1_250_000), "$1.25");
        assert_eq!(fmt_days(0), "0 days");
        assert_eq!(fmt_days(1), "1 day");
        assert_eq!(fmt_days(5), "5 days");
    }

    /// The modal height is always capped by the terminal — the floor is
    /// applied *before* the cap so it can't push the modal past the buffer
    /// on a short window, yet small content still clears the chrome guard.
    #[test]
    fn fit_height_floors_then_caps_and_never_exceeds_the_terminal() {
        // A tall terminal shows all 26 content lines + chrome, no scroll.
        assert_eq!(fit_height(26, 40), 29);
        // Content taller than the terminal clamps to the available height.
        assert_eq!(fit_height(26, 20), 18);
        // Tiny content is floored to 6 (so `inner.height` ≥ 3 and the body
        // actually renders) when the terminal has room — the loading state.
        assert_eq!(fit_height(1, 16), 6);
        // But on a terminal shorter than the floor, the CAP wins: the old
        // `.min(area).max(6)` returned 6 into a 2-row budget and overflowed;
        // floor-then-cap collapses toward the `inner.height < 3` guard.
        assert_eq!(fit_height(26, 4), 2);
        assert_eq!(fit_height(26, 3), 1);
        assert_eq!(fit_height(1, 4), 2);
        // The invariant, stated directly: modal_h ≤ area_height for every
        // content size / terminal height.
        for area_h in 0u16..60 {
            for lines in [1usize, 5, 26, 100] {
                assert!(fit_height(lines, area_h) <= area_h, "area_h={area_h}");
            }
        }
    }
}
