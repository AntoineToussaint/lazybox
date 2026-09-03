//! Render tests for the Sidebar and related widgets. Layout-critical
//! behavior is pinned with direct assertions; the remaining golden
//! `insta` snapshots (e.g. the which-key popup) lock panels whose value
//! is purely visual. The sidebar header embeds the build version, so its
//! layout is asserted by substring rather than a golden snapshot — that
//! keeps a routine version bump from churning fixtures (task #76).
//!
//! When a golden snapshot intentionally changes:
//!
//!   cargo install cargo-insta
//!   cargo insta review
//!
//! Accept with `a`, reject with `r`. Rejected changes fail CI —
//! that's the point.

use chrono::{Duration, TimeZone, Utc};
use lazybox_core::{
    CiStatus, ReviewStatus, SessionKey, SessionKind, Task, TaskId, TaskRole, TaskState, Workspace,
    WorkspaceSession,
};
use lazybox_ipc::{Event, TerminalId, TerminalKind};
use lazybox_tui::PaneId;
use lazybox_tui::components::Sidebar;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::prelude::Rect;
use std::path::PathBuf;

fn fixed_time() -> chrono::DateTime<Utc> {
    // A stable "now" so snapshots don't drift with wall-clock time.
    Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap()
}

fn make_task(key: &str, minutes_old: i64) -> Task {
    Task {
        author: String::new(),
        id: TaskId {
            source: "github".into(),
            key: key.into(),
        },
        title: format!("task: {key}"),
        body: None,
        state: TaskState::Open,
        role: TaskRole::Author,
        ci: CiStatus::None,
        review: ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: format!("https://github.com/{key}"),
        repo: Some("owner/repo".into()),
        branch: Some("main".into()),
        base_branch: None,
        updated_at: fixed_time() - Duration::minutes(minutes_old),
        created_at: None,
        closed_at: None,
        labels: vec![],
        reviewers: vec![],
        reviews: vec![],
        assignees: vec![],
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

/// A sidebar whose clock is pinned to [`fixed_time`] from the start, so
/// every time-dependent path — the visible-set classification driven by
/// `on_event` *and* the relative timestamps produced by render — sees
/// the same fixed instant. Pinning here (before any event) rather than
/// just before render keeps the snapshots stable no matter when the test
/// runs; otherwise the ages drift with wall-clock time and the golden
/// files rot (e.g. `1mo` silently becomes `2mo` a month later).
fn sidebar() -> Sidebar {
    let mut s = Sidebar::new(PaneId::new(1));
    s.set_now_override(fixed_time());
    s
}

/// Star `key` (add it to the focused set) purely in-memory — jump
/// numbers ride only focused workspaces now. Goes through `apply_config`
/// rather than `toggle_focus_at_cursor` so the test never writes the
/// real `ui.focused_workspaces` to disk.
fn star_workspace(s: &mut Sidebar, key: &SessionKey) {
    s.apply_config(
        lazybox_config::AttentionConfig::default(),
        std::collections::BTreeSet::new(),
        Vec::new(),
        vec![key.clone()],
        Vec::new(),
        std::collections::BTreeSet::new(),
        std::collections::BTreeSet::new(),
        None,
        &lazybox_config::DisplayConfig::default(),
    );
}

fn render_to_string(component: &mut Sidebar, w: u16, h: u16, focused: bool) -> String {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|frame| {
        component.render(Rect::new(0, 0, w, h), frame, focused);
    })
    .unwrap();
    let buf = term.backend().buffer();
    (0..buf.area.height)
        .map(|y| {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            // Trim trailing whitespace — it's noise in the snapshot.
            row.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Display columns of every line on which `marker` appears, top to
/// bottom. Char-based, not byte-based: `render_to_string` emits one
/// entry per terminal cell, and the cursor prefix (`▶` vs a space) is a
/// single cell either way, so the returned column is stable no matter
/// which row the cursor rests on — a byte offset would shift by the
/// multi-byte `▶` and make the assertion depend on cursor placement.
fn title_columns(rendered: &str, marker: &str) -> Vec<usize> {
    rendered
        .lines()
        .filter_map(|line| line.find(marker).map(|b| line[..b].chars().count()))
        .collect()
}

/// The display column at which `marker` starts on the first line that
/// contains it. See [`title_columns`] for why this is cursor-invariant.
fn title_column(rendered: &str, marker: &str) -> usize {
    title_columns(rendered, marker)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no line contains {marker:?}\n{rendered}"))
}

#[test]
fn sidebar_multiple_agent_badges_shows_counts() {
    let mut s = sidebar();
    let mut task = make_task("o/r#621", 10);
    task.title = "Multi-agent workspace".into();
    let mut workspace = Workspace::from_task(task, fixed_time());
    let key = SessionKey::from(&workspace.key);
    s.on_event(&Event::Snapshot {
        workspaces: vec![workspace.clone()],
        terminals: vec![],
        projects: vec![],
        recent_snippets: Vec::new(),
        dismissed_updates: Vec::new(),
    });
    let session = WorkspaceSession::new(
        workspace.key.clone(),
        SessionKind::Agent {
            agent_id: "claude".into(),
        },
        PathBuf::from("/tmp/multi-agent"),
        fixed_time(),
    );
    workspace.add_session(session.clone());
    s.on_event(&Event::WorkspaceUpserted(std::sync::Arc::new(workspace)));
    s.on_event(&Event::SessionCreated(Box::new(session)));
    for (terminal_id, agent) in [(1, "claude"), (2, "claude"), (3, "codex")] {
        s.on_event(&Event::TerminalSpawned {
            terminal_id: TerminalId(terminal_id),
            session_key: key.clone(),
            kind: TerminalKind::Agent(agent.into()),
            no_permission: false,
            on_main: false,
            model_label: None,
        });
    }

    // Jump numbers now ride only focused (starred) workspaces, so star
    // this row in-memory (via config apply, no disk write) to make its
    // `]]1` badge render alongside the agent badges.
    star_workspace(&mut s, &key);

    let rendered = render_to_string(&mut s, 40, 8, true);
    assert!(
        rendered.contains(" 1C×2X"),
        "default-width sidebar row must show its jump number, both agents, and the Claude count:\n{rendered}",
    );
}

/// Issue #813 golden: the density pass. Two single-agent rows — one
/// running a verbose `gpt-5.6-sol · xhigh`, one a bare `Opus`, the second
/// also merge-on-green armed. The verbose model is compacted so it can't
/// anchor the agent column table-wide, the passive `ARM` badge packs into
/// the right-side cluster, and both titles read in full with the trailer
/// hugging the right edge instead of a starved title + big interior gap.
#[test]
fn sidebar_dense_agent_rows_compacts_model_and_keeps_titles() {
    let mut s = sidebar();

    let mut verbose_task = make_task("o/r#812", 5);
    verbose_task.title = "feat: queue + retry the sync worker".into();
    let mut verbose_ws = Workspace::from_task(verbose_task, fixed_time());
    let verbose_key = SessionKey::from(&verbose_ws.key);

    let mut opus_task = make_task("o/r#813", 20);
    opus_task.title = "fix: sidebar columns waste half the row".into();
    let mut opus_ws = Workspace::from_task(opus_task, fixed_time());
    opus_ws.auto_merge_on_green = true; // → ` ARM ` in the packed cluster
    let opus_key = SessionKey::from(&opus_ws.key);

    s.on_event(&Event::Snapshot {
        workspaces: vec![verbose_ws.clone(), opus_ws.clone()],
        terminals: vec![],
        projects: vec![],
        recent_snippets: Vec::new(),
        dismissed_updates: Vec::new(),
    });

    // The verbose gpt model runs under Codex (badge `X`), the bare `Opus`
    // under Claude (badge `C`) — each model paired with an agent that could
    // actually report it.
    for (ws, key, terminal_id, agent, model) in [
        (
            &mut verbose_ws,
            &verbose_key,
            1u64,
            "codex",
            "gpt-5.6-sol · xhigh",
        ),
        (&mut opus_ws, &opus_key, 2u64, "claude", "Opus"),
    ] {
        let session = WorkspaceSession::new(
            ws.key.clone(),
            SessionKind::Agent {
                agent_id: agent.into(),
            },
            PathBuf::from("/tmp/dense-agent"),
            fixed_time(),
        );
        ws.add_session(session.clone());
        s.on_event(&Event::WorkspaceUpserted(std::sync::Arc::new(ws.clone())));
        s.on_event(&Event::SessionCreated(Box::new(session)));
        s.on_event(&Event::TerminalSpawned {
            terminal_id: TerminalId(terminal_id),
            session_key: key.clone(),
            kind: TerminalKind::Agent(agent.into()),
            no_permission: false,
            on_main: false,
            model_label: Some(model.into()),
        });
    }

    let rendered = render_to_string(&mut s, 88, 10, true);
    assert!(
        !rendered.contains("xhigh"),
        "the verbose effort must be abbreviated, not rendered raw:\n{rendered}",
    );
    assert!(
        rendered.contains("feat: queue + retry the sync worker"),
        "the title must read in full, not starve to a fragment:\n{rendered}",
    );
    assert!(
        rendered.contains("fix: sidebar columns waste half the row"),
        "the Opus row's title must not be widened away by the other row's model:\n{rendered}",
    );
}

/// A Linear issue, in its own team group. Distinct source + `repo`
/// from [`make_task`] so it forms a separate provider group in the
/// sidebar; its key is the bare `TEAM-NNN` identifier (no `#`, so it
/// carries no GitHub-style reference number) and it has no CI /
/// review.
fn make_linear_task(identifier: &str, team: &str, minutes_old: i64) -> Task {
    let mut t = make_task(&format!("{team}-placeholder"), minutes_old);
    t.id.source = "linear".into();
    t.id.key = identifier.into();
    t.url = format!("https://linear.app/{team}/issue/{identifier}");
    t.repo = Some(format!("linear/{team}"));
    t.title = format!("linear {identifier}");
    t
}

/// Per-group column independence (#961): the Linear group renders
/// identically whether or not a GitHub group with a wide reference
/// number and status columns sits alongside it. This is the concrete
/// statement of "each group aligns independently" (#961) — before the
/// fix, the global pre-pass padded every Linear row's reference column
/// to the GitHub group's 5-digit width and reserved its status columns.
#[test]
fn mixed_github_linear_groups_are_column_independent() {
    use lazybox_tui::components::sidebar::SortMode;

    let linear_a = make_linear_task("OBI-2011", "OBI", 30);
    let linear_b = make_linear_task("OBI-9", "OBI", 90);

    // Linear group alongside a wide-number GitHub group with status.
    let mut mixed = sidebar();
    while mixed.sort_mode() != SortMode::Recent {
        mixed.cycle_sort_mode();
    }
    let mut pr = make_task("owner/repo#31000", 5);
    pr.url = "https://github.com/owner/repo/pull/31000".into();
    pr.ci = CiStatus::Failure;
    pr.review = ReviewStatus::Pending;
    mixed.on_event(&Event::Snapshot {
        workspaces: vec![
            Workspace::from_task(pr, fixed_time()),
            Workspace::from_task(linear_a.clone(), fixed_time()),
            Workspace::from_task(linear_b.clone(), fixed_time()),
        ],
        terminals: vec![],
        projects: vec![],
        recent_snippets: Vec::new(),
        dismissed_updates: Vec::new(),
    });

    // The same Linear group, on its own.
    let mut linear_only = sidebar();
    while linear_only.sort_mode() != SortMode::Recent {
        linear_only.cycle_sort_mode();
    }
    linear_only.on_event(&Event::Snapshot {
        workspaces: vec![
            Workspace::from_task(linear_a, fixed_time()),
            Workspace::from_task(linear_b, fixed_time()),
        ],
        terminals: vec![],
        projects: vec![],
        recent_snippets: Vec::new(),
        dismissed_updates: Vec::new(),
    });

    let mixed_render = render_to_string(&mut mixed, 40, 12, true);
    let linear_render = render_to_string(&mut linear_only, 40, 12, true);

    // The Linear title starts at the same column in both — the GitHub
    // group's 5-digit reference did not push it right.
    assert_eq!(
        title_column(&mixed_render, "linear OBI-2011"),
        title_column(&linear_render, "linear OBI-2011"),
        "Linear row column position must not depend on a neighbouring GitHub group\nmixed:\n{mixed_render}\n\nlinear only:\n{linear_render}",
    );
}

/// Regression for issue #961 in the one place the sidebar deliberately
/// mixes providers: the `★ Focused` pin lifts starred rows across repos
/// (and providers) into a single group. Column sizing must still be
/// per-provider *within* that group — a starred GitHub PR's wide `#NNN`
/// must not inflate a starred Linear row's reference column.
#[test]
fn focused_group_sizes_columns_per_provider() {
    use lazybox_core::SessionKey;
    use std::collections::BTreeSet;

    fn star(s: &mut Sidebar, keys: Vec<SessionKey>) {
        // `apply_config` sets the focus set without persisting to disk.
        s.apply_config(
            lazybox_config::AttentionConfig::default(),
            BTreeSet::new(),
            Vec::new(),
            keys,
            Vec::new(),
            BTreeSet::new(),
            BTreeSet::new(),
            None,
            &lazybox_config::DisplayConfig::default(),
        );
    }

    let mut pr = make_task("owner/repo#31000", 5);
    pr.url = "https://github.com/owner/repo/pull/31000".into();
    pr.ci = CiStatus::Failure;
    pr.review = ReviewStatus::Pending;
    let gh_ws = Workspace::from_task(pr, fixed_time());
    let lin_ws = Workspace::from_task(make_linear_task("OBI-9", "OBI", 90), fixed_time());
    let gh_key = SessionKey::from(gh_ws.key.as_str());
    let lin_key = SessionKey::from(lin_ws.key.as_str());

    // Both starred → they share the cross-provider ★ Focused group.
    let mut focused = sidebar();
    star(&mut focused, vec![gh_key, lin_key.clone()]);
    focused.on_event(&Event::Snapshot {
        workspaces: vec![gh_ws, lin_ws.clone()],
        terminals: vec![],
        projects: vec![],
        recent_snippets: Vec::new(),
        dismissed_updates: Vec::new(),
    });

    // Baseline: the same Linear row in its own single-provider repo group,
    // with no GitHub reference anywhere to inflate it.
    let mut baseline = sidebar();
    star(&mut baseline, vec![lin_key]);
    baseline.on_event(&Event::Snapshot {
        workspaces: vec![lin_ws],
        terminals: vec![],
        projects: vec![],
        recent_snippets: Vec::new(),
        dismissed_updates: Vec::new(),
    });

    let focused_render = render_to_string(&mut focused, 40, 14, true);
    let baseline_render = render_to_string(&mut baseline, 40, 14, true);

    // The starred Linear row sits at the same column whether or not a
    // starred GitHub PR shares its Focused group — per-provider sizing.
    assert_eq!(
        title_column(&focused_render, "linear OBI-9"),
        title_column(&baseline_render, "linear OBI-9"),
        "the ★ Focused group must size columns per provider — the GitHub PR must not inflate the Linear row\nfocused:\n{focused_render}\n\nbaseline:\n{baseline_render}"
    );
}

/// Regression for issue #231: at a small terminal size the row's
/// horizontal budget goes to content, not to empty gutters. The
/// selection marker is a single shared column (lpad + `▶`), so the
/// type glyph starts at column 2 — not pushed in by a 2-col marker —
/// and a long title fills the row right up to the lone scrollbar
/// gutter instead of leaving dead margin on either side.
#[test]
fn sidebar_tight_gutters_leave_room_for_content_at_small_width() {
    let mut s = sidebar();
    let mut t = make_task("o/r#1", 10);
    t.title = "A very long pull request title that keeps going".into();
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(t, fixed_time())],
        terminals: vec![],
        projects: vec![],
        recent_snippets: Vec::new(),
        dismissed_updates: Vec::new(),
    });
    let w: u16 = 30;
    let rendered = render_to_string(&mut s, w, 10, true);
    let row = rendered
        .lines()
        .find(|l| l.trim_start().starts_with('▶'))
        .expect("a cursor workspace row");
    let chars: Vec<char> = row.chars().collect();
    // Left gutter: lpad(1) + 1-col marker, then the type glyph — no
    // 2-col-per-depth marker padding ahead of it.
    assert_eq!(chars[0], ' ', "row: {row:?}");
    assert_eq!(chars[1], '▶', "row: {row:?}");
    assert_ne!(
        chars[2], ' ',
        "type glyph should sit right after the single marker: {row:?}"
    );
    // Right side: only the scrollbar column is reserved, so a long
    // title fills the row to within a column of the edge rather than
    // truncating early and leaving a dead right gutter.
    let used = lazybox_tui::util::visual_width(row);
    assert!(
        used >= (w - 2) as usize,
        "long title left a dead right gutter (used {used} of {w}): {row:?}",
    );
}

/// The build version sits next to the brand name so a running instance
/// is identifiable. Asserted against the live `CARGO_PKG_VERSION` so a
/// release bump can't silently drop the tag — checked by substring, not
/// a golden snapshot, so a version bump doesn't churn any fixture.
#[test]
fn sidebar_header_shows_build_version() {
    let mut s = sidebar();
    let rendered = render_to_string(&mut s, 40, 5, true);
    let expected = concat!("v", env!("CARGO_PKG_VERSION"));
    assert!(
        rendered.contains(expected),
        "sidebar header {rendered:?} should contain {expected:?}"
    );
}

#[test]
fn sidebar_header_fits_narrow_width() {
    let mut s = sidebar();
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(make_task("o/r#1", 10), fixed_time())],
        terminals: vec![],
        projects: vec![],
        recent_snippets: Vec::new(),
        dismissed_updates: Vec::new(),
    });

    let rendered = render_to_string(&mut s, 24, 5, true);
    let first_line = rendered.lines().next().unwrap_or_default();
    assert!(
        lazybox_tui::util::visual_width(first_line) <= 24,
        "header overflowed narrow sidebar: {first_line:?}"
    );
    assert!(
        first_line.contains("LAZYBOX"),
        "narrow header lost the brand: {first_line:?}"
    );
}

/// Which-key popup for the `g` leader (#126, #102). Locks the panel
/// layout — title row plus one `key  label` row per continuation,
/// anchored bottom-left above the footer. The rows are derived from
/// the catalog (every entry whose chord is a `g …` sequence), the same
/// way the model builds them at render time.
#[test]
fn which_key_github_group_golden_render() {
    use lazybox_tui::realm::components::which_key;
    use lazybox_tui_core::action::{ActionDef, Chord, ChordCode, KeyStroke};
    let g = KeyStroke::new(false, false, false, ChordCode::Char('g'));
    let rows: Vec<(String, String)> = ActionDef::all()
        .flat_map(|def| {
            def.default_chords()
                .into_iter()
                .filter_map(move |c| match c {
                    Chord::Seq(s) if s.len() == 2 && s[0] == g => {
                        Some((s[1].display(), def.label.to_string()))
                    }
                    _ => None,
                })
        })
        .collect();
    let backend = TestBackend::new(40, 13);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|frame| {
        which_key::render(
            frame,
            Rect::new(0, 0, 40, 13),
            g,
            Some("github"),
            &rows,
            None,
        );
    })
    .unwrap();
    let buf = term.backend().buffer();
    let rendered = (0..buf.area.height)
        .map(|y| {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            row.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("which_key_github_group", rendered);
}
