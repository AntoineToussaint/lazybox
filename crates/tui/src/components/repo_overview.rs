//! Repo / Space **overview** — the right-pane home shown when the
//! sidebar cursor rests on a group header (a `RepoHeader` or a
//! `SpaceHeader`) rather than a workspace (issue #1442).
//!
//! On a header row the sidebar has no selected `Workspace`, so the
//! right pane used to paint a dead placeholder (`(no session
//! selected)` + an empty Activity section). This module fills that
//! space with a *group home*: at-a-glance counts, a compact roster of
//! the group's tracked PRs / issues, and a hint bar of the chords that
//! already act on the group (spawn on main, new workspace, browse
//! issues, merge history, open in browser).
//!
//! The type is plain data. The **builder** ([`build_repo_overview`] /
//! [`build_space_overview`]) is pure over borrowed sidebar state so it
//! unit-tests without a `Frame`; the **line builder**
//! ([`RepoOverview::lines`]) turns it into ratatui `Line`s plus a
//! roster hit-map the pane stores for click-to-select. Actual
//! `Paragraph` rendering + click geometry live in the `RightPane`
//! (`components/right_pane`), which owns this state.

use std::collections::{BTreeMap, HashMap};

use lazybox_core::{Project, ProjectKey, SessionKey, TaskState, Workspace};
use lazybox_ipc::AgentState;
use tuirealm::ratatui::style::{Modifier, Style};
use tuirealm::ratatui::text::{Line, Span};

use crate::components::visible_rows::group_label;

/// Whether the overview describes a single repo or an aggregate Space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewKind {
    Repo,
    Space,
}

/// At-a-glance rollup over a set of workspaces. Every field is a plain
/// count so the renderer never re-walks the workspace list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverviewCounts {
    pub workspaces: usize,
    pub open_prs: usize,
    pub open_issues: usize,
    pub with_agent: usize,
    pub failing_ci: usize,
    pub unread: usize,
}

impl OverviewCounts {
    fn add_workspace(&mut self, w: &Workspace, agent: Option<&AgentState>) {
        self.workspaces += 1;
        if let Some(task) = w.primary_task() {
            let live = !matches!(task.state, TaskState::Closed | TaskState::Merged);
            if task.is_pr() {
                if live {
                    self.open_prs += 1;
                }
            } else if live {
                self.open_issues += 1;
            }
            if matches!(
                task.ci,
                lazybox_core::CiStatus::Failure | lazybox_core::CiStatus::Mixed
            ) {
                self.failing_ci += 1;
            }
        }
        // "With a live agent" means the agent process exists — every
        // state except the terminal `Exited`. `Idle` counts (a freshly
        // launched agent sitting at a ready composer is running); only a
        // dead process doesn't.
        if agent.is_some_and(|s| !matches!(s, AgentState::Exited { .. })) {
            self.with_agent += 1;
        }
        self.unread += w.unread_count();
    }
}

/// One roster line: a tracked PR / issue in the group.
#[derive(Debug, Clone)]
pub struct RosterRow {
    /// Session key so a click can move the sidebar cursor onto it.
    pub key: SessionKey,
    pub title: String,
    pub number: Option<u64>,
    pub is_pr: bool,
    pub state: Option<TaskState>,
    pub agent: Option<AgentState>,
    pub unread: usize,
    /// Repo group label — used only in the Space rollup ordering.
    pub repo: String,
}

/// Per-repo mini-rollup shown under a Space header.
#[derive(Debug, Clone)]
pub struct RepoRollupRow {
    pub repo: String,
    pub counts: OverviewCounts,
}

/// The full overview payload the pane renders.
#[derive(Debug, Clone)]
pub struct RepoOverview {
    pub kind: OverviewKind,
    /// `owner/repo` for a repo, the Space name for a Space.
    pub title: String,
    pub counts: OverviewCounts,
    /// Capped, sorted roster. `roster_total` is the full length so the
    /// renderer can print a `+N more` trailer.
    pub roster: Vec<RosterRow>,
    pub roster_total: usize,
    /// Per-repo rollup — only populated for a Space.
    pub rollup: Vec<RepoRollupRow>,
}

/// How many roster rows the overview keeps; the rest collapse into a
/// `+N more · browse` trailer so the panel stays skimmable (the full
/// browsable list is the issue-browser modal, #1436).
pub const ROSTER_CAP: usize = 12;

/// Sort key that floats the rows most worth acting on to the top:
/// agents asking for input first, taskless rows last, everything else
/// in the middle ordered by unread count. Pure so both builders share
/// one order.
fn roster_rank(row: &RosterRow) -> (u8, usize) {
    let tier = match row.agent {
        Some(AgentState::InputNeeded) => 0,
        _ if row.state.is_none() => 5,
        _ => 3,
    };
    (tier, usize::MAX - row.unread)
}

fn roster_row(
    key: &SessionKey,
    w: &Workspace,
    repo: String,
    agent: Option<AgentState>,
) -> RosterRow {
    let task = w.primary_task();
    RosterRow {
        key: key.clone(),
        title: task
            .map(|t| t.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| w.name.clone()),
        number: task.and_then(|t| t.id.number()),
        is_pr: task.map(|t| t.is_pr()).unwrap_or(false),
        state: task.map(|t| t.state),
        agent,
        unread: w.unread_count(),
        repo,
    }
}

/// Sort a roster in place by [`roster_rank`], then by number descending
/// so newer items lead within a tier.
fn sort_roster(roster: &mut [RosterRow]) {
    roster.sort_by(|a, b| {
        roster_rank(a)
            .cmp(&roster_rank(b))
            .then_with(|| b.number.cmp(&a.number))
            .then_with(|| a.title.cmp(&b.title))
    });
}

/// Build the overview for a single repo group. `repo` is a sidebar
/// group label (the `RepoHeader` string). Members are every tracked
/// workspace whose [`group_label`] matches.
pub fn build_repo_overview(
    repo: &str,
    workspaces: &HashMap<SessionKey, Workspace>,
    projects: &BTreeMap<ProjectKey, Project>,
    agents: &HashMap<SessionKey, AgentState>,
) -> RepoOverview {
    let mut counts = OverviewCounts::default();
    let mut roster: Vec<RosterRow> = Vec::new();
    for (key, w) in workspaces {
        if group_label(w, projects, workspaces) != repo {
            continue;
        }
        let agent = agents.get(key).cloned();
        counts.add_workspace(w, agent.as_ref());
        roster.push(roster_row(key, w, repo.to_string(), agent));
    }
    sort_roster(&mut roster);
    let roster_total = roster.len();
    roster.truncate(ROSTER_CAP);
    RepoOverview {
        kind: OverviewKind::Repo,
        title: repo.to_string(),
        counts,
        roster,
        roster_total,
        rollup: Vec::new(),
    }
}

/// Build the aggregate overview for a Space — every repo group whose
/// `space_of` resolves to `space`. Produces the same top-level counts
/// plus a per-repo rollup.
pub fn build_space_overview(
    space: &str,
    workspaces: &HashMap<SessionKey, Workspace>,
    projects: &BTreeMap<ProjectKey, Project>,
    spaces: &[lazybox_config::SpaceConfig],
    agents: &HashMap<SessionKey, AgentState>,
) -> RepoOverview {
    let mut counts = OverviewCounts::default();
    let mut roster: Vec<RosterRow> = Vec::new();
    let mut per_repo: BTreeMap<String, OverviewCounts> = BTreeMap::new();
    for (key, w) in workspaces {
        let repo = group_label(w, projects, workspaces);
        if lazybox_tui_core::inbox::space_of(&repo, spaces) != space {
            continue;
        }
        let agent = agents.get(key).cloned();
        counts.add_workspace(w, agent.as_ref());
        per_repo
            .entry(repo.clone())
            .or_default()
            .add_workspace(w, agent.as_ref());
        roster.push(roster_row(key, w, repo, agent));
    }
    sort_roster(&mut roster);
    let roster_total = roster.len();
    roster.truncate(ROSTER_CAP);
    // Rollup ordered by workspace count desc, then name — the busiest
    // repo leads.
    let mut rollup: Vec<RepoRollupRow> = per_repo
        .into_iter()
        .map(|(repo, counts)| RepoRollupRow { repo, counts })
        .collect();
    rollup.sort_by(|a, b| {
        b.counts
            .workspaces
            .cmp(&a.counts.workspaces)
            .then_with(|| a.repo.cmp(&b.repo))
    });
    RepoOverview {
        kind: OverviewKind::Space,
        title: space.to_string(),
        counts,
        roster,
        roster_total,
        rollup,
    }
}

/// Coarse agent badge for a roster row — a compact `(glyph, style)`
/// mirroring the terminal tab badge's language, or `None` when there's
/// nothing worth a chip (idle / no agent).
fn agent_chip(state: &AgentState, theme: &crate::theme::Theme) -> Option<(&'static str, Style)> {
    match state {
        AgentState::InputNeeded => Some((
            "● asking",
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        )),
        AgentState::Working => Some(("· working", Style::default().fg(theme.accent))),
        AgentState::Done => Some((
            "✓ done",
            Style::default()
                .fg(theme.success)
                .add_modifier(Modifier::BOLD),
        )),
        AgentState::LimitReached => Some((
            "⏳ limited",
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        )),
        AgentState::CreditExhausted => Some((
            "¢ no credit",
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        )),
        AgentState::AwaitingReset => Some(("💤 waiting", Style::default().fg(theme.text_dim))),
        AgentState::Idle | AgentState::Exited { .. } => None,
    }
}

/// The `owner/repo` PR-number color reused for roster numbers, so a
/// roster `#123` matches the sidebar / header treatment.
fn state_chip(state: TaskState, theme: &crate::theme::Theme) -> Option<(&'static str, Style)> {
    let bold = Modifier::BOLD;
    match state {
        TaskState::Merged => Some((
            "merged",
            Style::default().fg(theme.hover).add_modifier(bold),
        )),
        TaskState::Closed => Some(("closed", Style::default().fg(theme.error))),
        TaskState::Draft => Some(("draft", Style::default().fg(theme.text_dim))),
        // Open / InProgress / InReview read as "live"; no chip keeps the
        // row quiet — the number + agent badge carry the signal.
        TaskState::Open | TaskState::InProgress | TaskState::InReview => None,
    }
}

/// A `k · label` count segment, dropped entirely when `k == 0` (except
/// the always-present workspace count) so the summary line stays terse.
fn count_span(k: usize, label: &str, style: Style) -> Option<Vec<Span<'static>>> {
    if k == 0 {
        return None;
    }
    Some(vec![
        Span::styled(format!("{k} "), style),
        Span::styled(label.to_string(), Style::default()),
    ])
}

impl RepoOverview {
    /// Build the render lines plus a roster hit-map: `(line_index,
    /// SessionKey)` for every roster row, so the pane can translate a
    /// click row into a workspace selection.
    pub fn lines(&self, width: u16) -> (Vec<Line<'static>>, Vec<(usize, SessionKey)>) {
        let theme = crate::theme::current();
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut hits: Vec<(usize, SessionKey)> = Vec::new();

        // ── Title row: KIND chip + name ────────────────────────────
        let (chip, chip_label) = match self.kind {
            OverviewKind::Repo => (" REPO ", &self.title),
            OverviewKind::Space => (" SPACE ", &self.title),
        };
        lines.push(Line::from(vec![
            Span::styled(
                chip,
                Style::default()
                    .bg(theme.chrome)
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                chip_label.clone(),
                Style::default()
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));

        // ── Counts summary ─────────────────────────────────────────
        let c = &self.counts;
        let mut segs: Vec<Vec<Span<'static>>> = Vec::new();
        let repos = self.rollup.len();
        if self.kind == OverviewKind::Space && repos > 0 {
            segs.push(vec![
                Span::styled(
                    format!("{repos} "),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(if repos == 1 { "repo" } else { "repos" }),
            ]);
        }
        segs.push(vec![
            Span::styled(
                format!("{} ", c.workspaces),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(if c.workspaces == 1 {
                "workspace"
            } else {
                "workspaces"
            }),
        ]);
        for seg in [
            count_span(c.open_prs, "PRs", Style::default().fg(theme.accent)),
            count_span(c.open_issues, "issues", Style::default().fg(theme.warn)),
            count_span(
                c.with_agent,
                "with agent",
                Style::default().fg(theme.success),
            ),
            count_span(
                c.failing_ci,
                "CI failing",
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ),
            count_span(c.unread, "unread", Style::default().fg(theme.accent)),
        ]
        .into_iter()
        .flatten()
        {
            segs.push(seg);
        }
        let mut summary: Vec<Span<'static>> = Vec::new();
        for (i, seg) in segs.into_iter().enumerate() {
            if i > 0 {
                summary.push(Span::styled("  ·  ", Style::default().fg(theme.chrome)));
            }
            summary.extend(seg);
        }
        lines.push(Line::from(summary));
        lines.push(Line::from(""));

        // ── Per-repo rollup (Space only) ───────────────────────────
        if self.kind == OverviewKind::Space && !self.rollup.is_empty() {
            lines.push(section_header("Repos"));
            for r in &self.rollup {
                let mut spans = vec![
                    Span::raw("  "),
                    Span::styled(r.repo.clone(), Style::default().fg(theme.text_strong)),
                ];
                let mut mini: Vec<String> = vec![format!("{} ws", r.counts.workspaces)];
                if r.counts.open_prs > 0 {
                    mini.push(format!("{} PR", r.counts.open_prs));
                }
                if r.counts.failing_ci > 0 {
                    mini.push(format!("{}✗", r.counts.failing_ci));
                }
                if r.counts.unread > 0 {
                    mini.push(format!("{}•", r.counts.unread));
                }
                spans.push(Span::styled(
                    format!("   {}", mini.join(" · ")),
                    Style::default().fg(theme.text_dim),
                ));
                lines.push(Line::from(spans));
            }
            lines.push(Line::from(""));
        }

        // ── Roster ─────────────────────────────────────────────────
        lines.push(section_header("Workspaces"));
        if self.roster.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (none tracked yet)",
                theme_hint(theme),
            )));
        }
        for row in &self.roster {
            let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
            if let Some(n) = row.number {
                spans.push(Span::styled(
                    format!("#{n}"),
                    Style::default()
                        .fg(crate::components::task_label::pr_number_color(n))
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                truncate(&row.title, title_budget(width)),
                Style::default().fg(theme.text_strong),
            ));
            if let Some((label, style)) = row.state.and_then(|s| state_chip(s, theme)) {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(label, style));
            }
            if let Some((label, style)) = row.agent.as_ref().and_then(|a| agent_chip(a, theme)) {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(label, style));
            }
            if row.unread > 0 {
                spans.push(Span::styled(
                    format!("  ● {}", row.unread),
                    Style::default().fg(theme.accent),
                ));
            }
            hits.push((lines.len(), row.key.clone()));
            lines.push(Line::from(spans));
        }
        if self.roster_total > self.roster.len() {
            let more = self.roster_total - self.roster.len();
            lines.push(Line::from(Span::styled(
                format!("  … +{more} more"),
                theme_hint(theme),
            )));
        }
        lines.push(Line::from(""));

        // ── Actions hint ───────────────────────────────────────────
        lines.push(section_header("Actions"));
        let actions = match self.kind {
            OverviewKind::Repo => "  b c agent on main · x n new workspace · g o open in browser",
            OverviewKind::Space => "  x n new workspace · Space collapse",
        };
        lines.push(Line::from(Span::styled(actions, theme_hint(theme))));

        (lines, hits)
    }
}

fn section_header(label: &str) -> Line<'static> {
    let theme = crate::theme::current();
    Line::from(Span::styled(
        label.to_string(),
        Style::default()
            .fg(theme.text_dim)
            .add_modifier(Modifier::BOLD),
    ))
}

fn theme_hint(theme: &crate::theme::Theme) -> Style {
    Style::default()
        .fg(theme.text_dim)
        .add_modifier(Modifier::ITALIC)
}

/// Column budget for a roster title before the trailing chips — leaves
/// room for `#NNNN`, a state chip, and an agent badge.
fn title_budget(width: u16) -> usize {
    (width as usize).saturating_sub(28).max(8)
}

/// Truncate to a display-**width** budget (not a char count), so a
/// title of wide (CJK / emoji) glyphs can't overflow `max` columns and
/// silently push the trailing chips off the pane.
fn truncate(s: &str, max: usize) -> String {
    if crate::util::visual_width(s) <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = crate::util::char_visual_width(ch);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use lazybox_core::{CiStatus, Task, TaskId, TaskKind, TaskRole};

    fn now() -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000, 0).single().unwrap()
    }

    fn task(repo: &str, n: u64, kind: TaskKind, state: TaskState, ci: CiStatus) -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: format!("{repo}#{n}"),
            },
            title: format!("Task {n}"),
            body: None,
            state,
            role: TaskRole::Author,
            ci,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: match kind {
                TaskKind::Pr => format!("https://github.com/{repo}/pull/{n}"),
                TaskKind::Issue => format!("https://github.com/{repo}/issues/{n}"),
            },
            repo: Some(repo.to_string()),
            branch: Some("feature".into()),
            base_branch: None,
            updated_at: now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            approval_policy: Default::default(),
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
            kind: Some(kind),
            priority: None,
            state_label: None,
        }
    }

    fn ws(repo: &str, n: u64, kind: TaskKind, state: TaskState, ci: CiStatus) -> Workspace {
        Workspace::from_task(task(repo, n, kind, state, ci), now())
    }

    fn map(items: Vec<Workspace>) -> HashMap<SessionKey, Workspace> {
        items
            .into_iter()
            .map(|w| (SessionKey::from(&w.key), w))
            .collect()
    }

    #[test]
    fn repo_overview_counts_prs_issues_and_ci() {
        let workspaces = map(vec![
            ws("o/r", 1, TaskKind::Pr, TaskState::Open, CiStatus::Success),
            ws("o/r", 2, TaskKind::Issue, TaskState::Open, CiStatus::None),
            ws("o/r", 3, TaskKind::Pr, TaskState::Merged, CiStatus::None),
            ws("o/r", 4, TaskKind::Pr, TaskState::Open, CiStatus::Failure),
        ]);
        let ov = build_repo_overview("o/r", &workspaces, &BTreeMap::new(), &HashMap::new());
        assert_eq!(ov.counts.workspaces, 4);
        assert_eq!(ov.counts.open_prs, 2, "merged PR is not counted open");
        assert_eq!(ov.counts.open_issues, 1);
        assert_eq!(ov.counts.failing_ci, 1);
        assert_eq!(ov.roster_total, 4);
        assert_eq!(ov.kind, OverviewKind::Repo);
    }

    #[test]
    fn repo_overview_excludes_other_repos() {
        let workspaces = map(vec![
            ws("o/r", 1, TaskKind::Pr, TaskState::Open, CiStatus::None),
            ws("o/other", 2, TaskKind::Pr, TaskState::Open, CiStatus::None),
        ]);
        let ov = build_repo_overview("o/r", &workspaces, &BTreeMap::new(), &HashMap::new());
        assert_eq!(ov.counts.workspaces, 1);
    }

    #[test]
    fn with_agent_counts_every_state_but_exited() {
        let workspaces = map(vec![
            ws("o/r", 1, TaskKind::Pr, TaskState::Open, CiStatus::None),
            ws("o/r", 2, TaskKind::Pr, TaskState::Open, CiStatus::None),
            ws("o/r", 3, TaskKind::Pr, TaskState::Open, CiStatus::None),
        ]);
        let keys: Vec<SessionKey> = workspaces.keys().cloned().collect();
        let mut agents = HashMap::new();
        agents.insert(keys[0].clone(), AgentState::Working);
        // An Idle agent is still a live process — it must count.
        agents.insert(keys[1].clone(), AgentState::Idle);
        // A dead process must not.
        agents.insert(keys[2].clone(), AgentState::Exited { code: Some(0) });
        let ov = build_repo_overview("o/r", &workspaces, &BTreeMap::new(), &agents);
        assert_eq!(
            ov.counts.with_agent, 2,
            "Working + Idle count; Exited does not",
        );
    }

    #[test]
    fn lines_render_title_and_counts() {
        let workspaces = map(vec![ws(
            "o/r",
            1,
            TaskKind::Pr,
            TaskState::Open,
            CiStatus::None,
        )]);
        let ov = build_repo_overview("o/r", &workspaces, &BTreeMap::new(), &HashMap::new());
        let (lines, hits) = ov.lines(80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("REPO"));
        assert!(text.contains("o/r"));
        assert!(text.contains("workspace"));
        assert!(text.contains("#1"));
        assert_eq!(hits.len(), 1, "one roster row → one hit");
    }

    #[test]
    fn roster_cap_truncates_and_reports_total() {
        let mut items = Vec::new();
        for n in 0..(ROSTER_CAP as u64 + 5) {
            items.push(ws("o/r", n, TaskKind::Pr, TaskState::Open, CiStatus::None));
        }
        let ov = build_repo_overview("o/r", &map(items), &BTreeMap::new(), &HashMap::new());
        assert_eq!(ov.roster.len(), ROSTER_CAP);
        assert_eq!(ov.roster_total, ROSTER_CAP + 5);
        let (lines, _) = ov.lines(80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("+5 more"));
    }

    #[test]
    fn space_overview_rolls_up_repos() {
        let workspaces = map(vec![
            ws("o/r1", 1, TaskKind::Pr, TaskState::Open, CiStatus::None),
            ws("o/r2", 2, TaskKind::Issue, TaskState::Open, CiStatus::None),
            ws("o/r1", 3, TaskKind::Pr, TaskState::Open, CiStatus::None),
        ]);
        // No explicit spaces config → owner-seeded Space "o".
        let ov = build_space_overview("o", &workspaces, &BTreeMap::new(), &[], &HashMap::new());
        assert_eq!(ov.kind, OverviewKind::Space);
        assert_eq!(ov.counts.workspaces, 3);
        assert_eq!(ov.rollup.len(), 2, "two repos under the space");
        // Busiest repo (r1, 2 ws) leads the rollup.
        assert_eq!(ov.rollup[0].repo, "o/r1");
        assert_eq!(ov.rollup[0].counts.workspaces, 2);
    }

    #[test]
    fn truncate_respects_display_width_not_char_count() {
        // Ten fullwidth CJK glyphs = 20 display columns. A char-count
        // truncation would keep all ten (10 chars ≤ budget) and overflow;
        // the width-aware one must clip to fit the budget + ellipsis.
        let wide = "上上上上上上上上上上";
        let out = truncate(wide, 8);
        assert!(
            crate::util::visual_width(&out) <= 8,
            "truncated width {} exceeds budget",
            crate::util::visual_width(&out)
        );
        assert!(out.ends_with('…'));
        // A short ASCII title is returned untouched.
        assert_eq!(truncate("hi", 8), "hi");
    }
}
