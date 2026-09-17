//! Client-free inbox view logic: grouping, sort, filter, and search
//! over `lazybox-core`/`lazybox-ipc` domain types. No render context —
//! both the ratatui TUI and the desktop client build their sidebar
//! from [`compute_visible`], so the two clients cannot drift.
//!
//! What "visible" means: the workspaces in the focused mailbox,
//! grouped by their parent Project, with a `(no repo)` bucket for
//! task-less / project-less workspaces. Each group emits a
//! `RepoHeader`; if not collapsed, the workspace rows follow (and
//! their session sub-rows when a workspace has 2+ sessions).
//!
//! Extracted from `Sidebar::recompute_visible_inner` so the
//! classification matrix — which project a workspace lands under,
//! whether an empty subscribed repo emits a header — is testable
//! as a free function with no `Sidebar` instance. Cursor
//! preservation stays on `Sidebar` (it reads/writes `self.cursor`);
//! this function is purely the rebuild half.

mod attention;
mod filter;
mod model;

pub use attention::{
    AttentionSignal, INACTIVE_GRACE, attention_gate, mailbox_membership, punches_through,
    workspace_attention_signals, workspace_needs_attention,
};
pub use filter::{
    Filter, FilterAxis, FilterCtx, FilterEntry, FilterMenuItem, FilterSet, task_involves,
};
pub use model::{
    Mailbox, RepoSummary, SearchState, SortMode, TicketTreeMeta, VisibleRow, WorkspaceKind,
    role_rank,
};

use lazybox_core::{Project, ProjectKey, SessionKey, Task, TaskId, Workspace};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Output of `compute_visible`. Held together because the
/// summaries are derived during the same pass that builds the
/// row list — re-deriving them would duplicate the grouping.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct ComputeOutcome {
    pub visible: Vec<VisibleRow>,
    pub summaries: BTreeMap<String, RepoSummary>,
    /// Per-workspace ticket hierarchy for rows in `visible`.
    pub ticket_tree: HashMap<SessionKey, TicketTreeMeta>,
    /// For rows an `agent:` term matched, a short excerpt of the agent
    /// text around the hit (#1774). The row's title holds nothing the
    /// underline could mark — the match isn't in it — so the excerpt is
    /// what tells the user *why* the row is in the result set. Produced
    /// by the same [`search_evaluate`] pass that filtered the row, so a
    /// shown excerpt always corresponds to a real hit.
    pub agent_excerpts: HashMap<SessionKey, String>,
}

/// Inputs to the visible-rows pass. Borrowed so the function
/// doesn't take ownership; lifetime threads from the caller.
///
/// This is the client-free entry point's single argument bundle: a
/// non-TUI caller (the desktop's `src-tauri`) builds the same
/// workspace map + agent-state map + [`FilterSet`] + [`SortMode`] +
/// optional [`SearchState`] its gateway connection exposes and gets
/// back a [`ComputeOutcome`].
pub struct ComputeInputs<'a> {
    pub workspaces: &'a HashMap<SessionKey, Workspace>,
    pub mailbox: Mailbox,
    /// Composable predicate filter layered on top of the mailbox. An
    /// empty set is the no-op identity. See [`FilterSet::accepts`].
    pub filters: &'a FilterSet,
    /// How to order workspaces within each repo group. `Default`
    /// (recency) is the legacy behavior; `ByRole` / `ByRoleSplit`
    /// promote Author rows then Reviewer etc. See [`SortMode`].
    pub sort_mode: SortMode,
    pub show_inactive_in_inbox: bool,
    /// Projects mirrored from the daemon's project table — plus
    /// synthetic entries for the user's subscribed scopes (the model
    /// merges them in via `refresh_subscribed_projects`). Each entry
    /// emits a sidebar header so a freshly-subscribed repo with no
    /// workspaces still shows up.
    pub projects: &'a BTreeMap<ProjectKey, Project>,
    pub collapsed_repos: &'a BTreeSet<String>,
    /// Repo group names the user pinned to the top, in pin order.
    /// Pinned groups render first (in this order); every other group
    /// keeps the algorithmic (alphabetical) order. A name here that
    /// has no matching group this pass is simply ignored.
    pub pinned_repos: &'a [String],
    /// Workspace keys the user has starred ("focused"), in focus order.
    /// A visible starred workspace is lifted out of its repo group into
    /// the synthetic `★ Focused` section rendered first, in this order.
    /// A key here with no matching visible workspace this pass is simply
    /// ignored. The manual, per-workspace counterpart to [`Self::pinned_repos`].
    pub focused_workspaces: &'a [SessionKey],
    /// User-defined Spaces — the higher-level grouping tier rendered
    /// above the repo headers (#860). Empty when the user has defined
    /// none, in which case sources auto-seed into owner-named Spaces
    /// via [`space_of`]. The Space tier only renders when it yields ≥2
    /// distinct Spaces (a lone Space is pure chrome).
    pub spaces: &'a [lazybox_config::SpaceConfig],
    /// Space names whose repo groups the user collapsed. Mirrors
    /// `collapsed_repos` one tier up.
    pub collapsed_spaces: &'a BTreeSet<String>,
    /// Provider task ids whose visible ticket descendants are folded.
    pub collapsed_tickets: &'a HashSet<TaskId>,
    pub attention: &'a lazybox_config::AttentionConfig,
    /// Per-source attention ladder (`ui.source_attention`, #scale):
    /// group labels / `space:<name>` keys → level + optional snooze.
    /// Demoted (Muted / source-snoozed) groups sink to the bottom of
    /// their tier and their rows hide behind the collapsed residue
    /// header — except rows that [`punches_through`]. An empty map is
    /// the no-op identity (everything Live).
    pub source_attention: &'a BTreeMap<String, lazybox_config::SourceAttention>,
    pub agents: &'a HashMap<SessionKey, lazybox_ipc::AgentState>,
    /// Live epic snapshots the daemon derived (#1517 §4d), keyed by epic
    /// key. Each epic with a visible member emits an `EpicHeader` tier above
    /// the Space / repo headers, its members lifted out of their repo groups
    /// and ordered by the wave the resolver assigned. Empty is the no-op
    /// identity — the tree keeps its pre-epic shape exactly.
    pub epics: &'a BTreeMap<String, lazybox_ipc::EpicSnapshot>,
    /// Epic keys whose member rows the user folded away. Mirrors
    /// `collapsed_spaces` one tier up.
    pub collapsed_epics: &'a BTreeSet<String>,
    pub now: chrono::DateTime<chrono::Utc>,
    /// Free-text search, or `None`. When `Some` with a non-empty
    /// query, a scoped search (`scope: Some`) keeps only that
    /// project's fuzzy-matching rows while other projects pass through
    /// untouched; a global search (`scope: None`) keeps only
    /// fuzzy-matching rows across every project. See [`search_matches`].
    pub search: Option<&'a SearchState>,
    /// Per-workspace agent text the `agent:` / `said:` qualifiers search
    /// (#1774), keyed by the same `SessionKey` as `workspaces`. A key
    /// that's absent simply never matches an `agent:` term — a workspace
    /// that has never run an agent has nothing to say. Only read when
    /// the query carries one of those qualifiers, so the cost of holding
    /// it is paid by the feature that asks for it. An empty map is the
    /// no-op identity.
    pub agent_text: &'a HashMap<SessionKey, String>,
}

const NO_REPO: &str = "(no repo)";

/// Default Space for a source that carries no owner boundary and isn't
/// explicitly assigned — Linear project labels, the `(no repo)` bucket.
pub const UNGROUPED_SPACE: &str = lazybox_config::UNGROUPED_SPACE;

/// Which Space a source group label (`group_label`'s output) belongs
/// to. An explicit `ui.spaces` assignment wins; otherwise the owner
/// segment of an `owner/repo` label auto-seeds an owner-named Space;
/// everything else falls into [`UNGROUPED_SPACE`]. Pure and total so
/// both clients derive the same tier (#860). Canonical in
/// `lazybox_config` so the daemon resolves Space-level attention with
/// the identical rule (#scale).
pub fn space_of(label: &str, spaces: &[lazybox_config::SpaceConfig]) -> String {
    lazybox_config::space_of(label, spaces)
}

/// Assign `source` to the Space named `space`, mutating the persisted
/// `ui.spaces` list in place (#860). The source is first removed from
/// every Space, then appended to the target — so re-assigning within
/// the same Space moves it to the end (the within-Space reorder
/// handle), and a blank `space` leaves it unassigned (owner auto-seed).
/// A `space` name not yet present is created at the end, establishing
/// its display order. Pure over the config list so the mutation is
/// testable without touching disk; the caller persists the result.
pub fn assign_source(spaces: &mut Vec<lazybox_config::SpaceConfig>, source: &str, space: &str) {
    let space = space.trim();
    for s in spaces.iter_mut() {
        s.sources.retain(|src| src != source);
    }
    if !space.is_empty() {
        match spaces.iter_mut().find(|s| s.name == space) {
            Some(existing) => existing.sources.push(source.to_string()),
            None => spaces.push(lazybox_config::SpaceConfig {
                name: space.to_string(),
                sources: vec![source.to_string()],
            }),
        }
    }
}

/// Direction for a Space / repo reorder (#1211).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDir {
    Up,
    Down,
    Top,
    Bottom,
}

impl MoveDir {
    /// Human label for footer notices ("moved obin-ai up").
    pub fn label(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Top => "to the top",
            Self::Bottom => "to the bottom",
        }
    }
}

/// Where `idx` lands in a list of `len` after moving `dir`, clamped at
/// the ends so an over-move is a no-op rather than an error.
fn moved_index(len: usize, idx: usize, dir: MoveDir) -> usize {
    match dir {
        MoveDir::Up => idx.saturating_sub(1),
        MoveDir::Down => (idx + 1).min(len.saturating_sub(1)),
        MoveDir::Top => 0,
        MoveDir::Bottom => len.saturating_sub(1),
    }
}

/// Reorder the Space tier (#1211): rewrite `spaces` so its entry order
/// encodes `rendered` (the full on-screen Space order) with `name`
/// moved per `dir`. Spaces without a config entry (owner auto-seeds /
/// `Ungrouped`) are materialized with an empty source list — explicit
/// order without changing what resolves into them. Config entries not
/// rendered this pass keep their sources and follow at the end in
/// their original relative order. Returns `false` when `name` isn't
/// rendered (nothing to move).
pub fn move_space(
    spaces: &mut Vec<lazybox_config::SpaceConfig>,
    rendered: &[String],
    name: &str,
    dir: MoveDir,
) -> bool {
    let Some(idx) = rendered.iter().position(|s| s == name) else {
        return false;
    };
    let mut order: Vec<String> = rendered.to_vec();
    let item = order.remove(idx);
    order.insert(moved_index(rendered.len(), idx, dir), item);

    let mut out: Vec<lazybox_config::SpaceConfig> = order
        .iter()
        .map(|n| {
            spaces
                .iter()
                .find(|s| &s.name == n)
                .cloned()
                .unwrap_or_else(|| lazybox_config::SpaceConfig {
                    name: n.clone(),
                    sources: Vec::new(),
                })
        })
        .collect();
    for cfg in spaces.iter() {
        if !order.iter().any(|n| n == &cfg.name) {
            out.push(cfg.clone());
        }
    }
    *spaces = out;
    true
}

/// Reorder a repo within its Space (#1211): materialize `rendered`
/// (the Space's on-screen repo order) into the Space's `sources` with
/// `repo` moved per `dir`. Sources the Space already claims but that
/// aren't rendered this pass (filtered out) keep their claim, appended
/// in their original order. The Space entry is created when the tier
/// was implicit (owner auto-seed) — reordering inside it is the moment
/// it becomes hand-managed. Returns `false` when `repo` isn't
/// rendered.
pub fn move_source_in_space(
    spaces: &mut Vec<lazybox_config::SpaceConfig>,
    space: &str,
    rendered: &[String],
    repo: &str,
    dir: MoveDir,
) -> bool {
    let Some(idx) = rendered.iter().position(|s| s == repo) else {
        return false;
    };
    let mut order: Vec<String> = rendered.to_vec();
    let item = order.remove(idx);
    order.insert(moved_index(rendered.len(), idx, dir), item);

    if !spaces.iter().any(|s| s.name == space) {
        spaces.push(lazybox_config::SpaceConfig {
            name: space.to_string(),
            sources: Vec::new(),
        });
    }
    let Some(cfg) = spaces.iter_mut().find(|s| s.name == space) else {
        return false;
    };
    for src in cfg.sources.clone() {
        if !order.contains(&src) {
            order.push(src);
        }
    }
    cfg.sources = order;
    true
}

/// Rename a Space (#1211): move every source it claims — plus
/// `rendered_sources`, the repos currently resolving into it on screen
/// (covers auto-seeded Spaces with no config entry) — into the entry
/// named `new`, creating it in the old entry's position (or at the end
/// for an auto-seed). Renaming onto an existing Space merges into it.
/// Returns `false` for a blank/unchanged name (advise-level no-op).
pub fn rename_space(
    spaces: &mut Vec<lazybox_config::SpaceConfig>,
    old: &str,
    new: &str,
    rendered_sources: &[String],
) -> bool {
    let new = new.trim();
    if new.is_empty() || new == old {
        return false;
    }
    let old_idx = spaces.iter().position(|s| s.name == old);
    let mut moved: Vec<String> = match old_idx {
        Some(idx) => spaces.remove(idx).sources,
        None => Vec::new(),
    };
    for src in rendered_sources {
        if !moved.contains(src) {
            moved.push(src.clone());
        }
    }
    match spaces.iter_mut().find(|s| s.name == new) {
        Some(target) => {
            for src in moved {
                if !target.sources.contains(&src) {
                    target.sources.push(src);
                }
            }
        }
        None => {
            let entry = lazybox_config::SpaceConfig {
                name: new.to_string(),
                sources: moved,
            };
            match old_idx {
                // Keep the renamed Space's display position.
                Some(idx) => spaces.insert(idx.min(spaces.len()), entry),
                None => spaces.push(entry),
            }
        }
    }
    true
}

/// Pure function: build the sidebar's visible-row list + per-repo
/// summaries from the workspace map, mailbox filter, and
/// repo-subscription config. No `Sidebar` borrow.
pub fn compute_visible(input: ComputeInputs<'_>) -> ComputeOutcome {
    // Step 1: filter by mailbox membership. Uses the cell-tested
    // `mailbox_membership` predicate so snooze/merged/empty cases
    // can't drift from their unit tests. The snoozed lens (#scale)
    // widens Inbox membership: with `Filter::Snoozed` active, snoozed
    // rows are admitted here so the State axis can then select them —
    // showing every snoozed workspace in place (with its wake time)
    // instead of two mailbox cycles away. Filters otherwise only
    // narrow, so this is the one membership decision a filter makes.
    let snoozed_lens =
        input.filters.has(filter::Filter::Snoozed) && input.mailbox == Mailbox::Inbox;
    let mailbox_rows: Vec<(&SessionKey, &Workspace)> = input
        .workspaces
        .iter()
        .filter(|(_, w)| {
            mailbox_membership(w, input.mailbox, input.now, input.show_inactive_in_inbox)
                || (snoozed_lens && w.is_snoozed(input.now))
        })
        .collect();
    let mut agent_excerpts: HashMap<SessionKey, String> = HashMap::new();
    let mut filtered: Vec<(&SessionKey, &Workspace)> = mailbox_rows
        .iter()
        .copied()
        .filter(|(_, w)| {
            input.filters.accepts(&FilterCtx {
                w,
                agents: input.agents,
                now: input.now,
            })
        })
        // Free-text search. A scoped search (`scope: Some`) filters
        // only the matching project's rows and leaves every other
        // project fully visible; a global search (`scope: None`)
        // filters every repo group at once. A row is kept unless the
        // search's scope *covers* it and it doesn't match —
        // `search_scope_covers` is the single definition of "in scope",
        // shared with the sidebar's match-highlight decision so the two
        // can't drift (#1099).
        .filter(|(key, w)| {
            // Out-of-scope rows pass through untouched; an in-scope row is
            // kept only if it matches. `search_scope_covers` is `false`
            // for `None` / blank queries, so the `else` is the no-search
            // and out-of-scope case alike.
            let Some(s) = input
                .search
                .filter(|s| search_scope_covers(Some(s), w, input.projects, input.workspaces))
            else {
                return true;
            };
            let hit = search_evaluate(&s.query, w, input.agent_text.get(*key).map(String::as_str));
            if let Some(excerpt) = hit.agent_excerpt {
                agent_excerpts.insert((*key).clone(), excerpt);
            }
            hit.matched
        })
        .collect();

    // Keep in-mailbox ancestors of matching tickets as dimmed context.
    // This prevents a child from jumping to root whenever a search or filter
    // excludes its parent. Parent lookup is source-agnostic and restricted to
    // the same rendered project, so malformed cross-project links cannot
    // pull unrelated work into a group.
    let mut included: HashSet<SessionKey> =
        filtered.iter().map(|(key, _)| (*key).clone()).collect();
    let mut context_only: HashSet<SessionKey> = HashSet::new();
    // Index every in-mailbox workspace by *all* its provider ids, so a parent
    // that has since acquired a PR (headline task = the PR) is still reachable
    // by the ticket id its children reference. Mirrors `emit_workspace_forest`.
    let by_task: HashMap<TaskId, (&SessionKey, &Workspace)> = mailbox_rows
        .iter()
        .flat_map(|(key, workspace)| {
            workspace
                .hierarchy_task_ids()
                .map(move |id| (id.clone(), (*key, *workspace)))
        })
        .collect();
    // One O(W) label-map build for every group_label below (U6): the
    // per-call scan made this pass — one label per row AND per
    // ancestor step — O(W²) string-allocating work per recompute.
    let labels = ProjectRepoLabels::build(input.workspaces);
    let mut seen = HashSet::new();
    for (_, matched) in filtered.clone() {
        let group = labels.group_label(matched, input.projects);
        let mut cursor = matched;
        seen.clear();
        while let Some(parent_id) = cursor.hierarchy_parent() {
            if !seen.insert(parent_id.clone()) {
                break;
            }
            let Some((parent_key, parent)) = by_task.get(parent_id).copied() else {
                break;
            };
            if labels.group_label(parent, input.projects) != group {
                break;
            }
            if included.insert(parent_key.clone()) {
                filtered.push((parent_key, parent));
                context_only.insert(parent_key.clone());
            }
            cursor = parent;
        }
    }

    // Step 1b: lift the starred ("focused") workspaces out of the
    // filtered set. A visible starred workspace is gathered into the
    // synthetic `★ Focused` section — rendered first, in focus order —
    // regardless of which repo it belongs to, and is NOT re-listed
    // under its repo group. A star naming a workspace not visible in
    // the current mailbox/filter this pass is simply skipped, exactly
    // as a pin naming an absent repo is (Step 4b). Focus is the manual,
    // per-workspace counterpart to the repo pin.
    let focused_set: BTreeSet<&str> = input
        .focused_workspaces
        .iter()
        .map(|k| k.as_str())
        .collect();
    let filtered_by_key: BTreeMap<&str, (&SessionKey, &Workspace)> = filtered
        .iter()
        .map(|(k, w)| (k.as_str(), (*k, *w)))
        .collect();
    let focused_rows: Vec<(&SessionKey, &Workspace)> = input
        .focused_workspaces
        .iter()
        .filter_map(|key| filtered_by_key.get(key.as_str()).copied())
        .collect();

    let mut hopper_rows: Vec<(&SessionKey, &Workspace)> = filtered
        .iter()
        .copied()
        .filter(|(key, workspace)| {
            workspace.hopper.is_some() && !focused_set.contains(key.as_str())
        })
        .collect();
    hopper_rows.sort_by_key(|(key, workspace)| {
        (
            workspace
                .hopper
                .map(|meta| meta.position)
                .unwrap_or(u32::MAX),
            key.as_str(),
        )
    });

    // Epic sections (#1517 §4d): a live epic's members are lifted out of
    // their repo groups into a cross-repo tier, in the wave order the
    // resolver assigned. A workspace already lifted into Focused or Hopper
    // stays there — those are explicit personal placements, and the epic
    // header's counts come from the snapshot rather than from the rows on
    // screen, so the status line reads the same either way. A workspace two
    // epics both claim lands under the first, keeping the tree a tree.
    let mut epic_sections: Vec<(&str, Vec<(&SessionKey, &Workspace)>)> = Vec::new();
    let mut epic_member_set: HashSet<&str> = HashSet::new();
    for (key, snapshot) in input.epics {
        let mut rows: Vec<(&SessionKey, &Workspace)> = Vec::new();
        for member in &snapshot.members {
            let id = member.key.as_str();
            if focused_set.contains(id) || epic_member_set.contains(id) {
                continue;
            }
            let Some(row) = filtered_by_key.get(id).copied() else {
                continue;
            };
            if row.1.hopper.is_some() {
                continue;
            }
            epic_member_set.insert(id);
            rows.push(row);
        }
        if !rows.is_empty() {
            epic_sections.push((key.as_str(), rows));
        }
    }

    // Step 2: bucket the non-focused workspaces by project. A
    // workspace's parent project is looked up via
    // `lazybox_core::workspace_project_key` → resolved through the
    // daemon's project table to get the display name. Workspaces with
    // no project_key (back-compat reads of pre-Stage-1 records OR
    // orphans whose task.repo failed to derive) land under `(no repo)`.
    let mut by_repo: BTreeMap<String, Vec<(&SessionKey, &Workspace)>> = BTreeMap::new();
    for (k, w) in &filtered {
        if focused_set.contains(k.as_str())
            || w.hopper.is_some()
            || epic_member_set.contains(k.as_str())
        {
            continue;
        }
        by_repo
            .entry(labels.group_label(w, input.projects))
            .or_default()
            .push((k, w));
    }

    // Step 3: sort each bucket. `Recent` keeps the legacy recency
    // order. `ByRole` promotes Author rows, then Reviewer, then
    // Assignee, then Mentioned. `ByRoleSplit` additionally pushes
    // PR workspaces above issue workspaces so a single linear pass
    // in step 5 can drop a `KindHeader` between the two sections.
    // Within each role/kind band, recency breaks ties — so the
    // most recently updated PR-you-authored is still on top.
    for rows in by_repo.values_mut() {
        rows.sort_by(|(ka, a), (kb, b)| {
            let a_ts = a
                .primary_task()
                .map(|t| t.updated_at)
                .unwrap_or(a.created_at);
            let b_ts = b
                .primary_task()
                .map(|t| t.updated_at)
                .unwrap_or(b.created_at);
            // Announced re-entry (#scale, B4): a row whose event-
            // conditional snooze just fired floats to the top of its
            // group for WOKE_WINDOW, in EVERY sort mode — a snooze
            // ending must never be a silent reappearance mid-list.
            let woke = b
                .is_recently_woken(input.now)
                .cmp(&a.is_recently_woken(input.now));
            let recency = b_ts.cmp(&a_ts);
            let tie = ka.as_str().cmp(kb.as_str());
            let role_cmp = || {
                role_rank(a.primary_task().map(|t| t.role))
                    .cmp(&role_rank(b.primary_task().map(|t| t.role)))
            };
            // `WorkspaceKind` derives `Ord` with `Pr < Issue`, so
            // a plain `cmp` does the PR-first ordering.
            let kind_cmp = || WorkspaceKind::classify(a).cmp(&WorkspaceKind::classify(b));
            let base = match input.sort_mode {
                SortMode::Recent => recency.then(tie),
                SortMode::ByRole => role_cmp().then(recency).then(tie),
                SortMode::ByRoleSplit => kind_cmp().then_with(role_cmp).then(recency).then(tie),
            };
            woke.then(base)
        });
    }

    // Step 4: collect the header set. Every project from the
    // (daemon-mirrored + scope-synthesized) projects map gets a
    // header in the Inbox mailbox, so an empty project still shows
    // up. Omitted from Inactive / Snoozed (alternate views, not
    // subscriptions), and omitted while a filter is active — a
    // narrowed view listing every subscribed repo as an empty header
    // buries the few matches behind a wall of chrome, so under a
    // filter only repos with matching workspaces get a header. A
    // *global* search (`scope: None`, the desktop's global `/`) narrows
    // the same way, so it suppresses them too; the TUI's scoped search
    // leaves other projects untouched, so it keeps their headers.
    let global_search = input
        .search
        .is_some_and(|s| !s.query.is_empty() && s.scope.is_none());
    let mut all_repos: BTreeSet<String> = by_repo.keys().cloned().collect();
    if input.mailbox == Mailbox::Inbox && input.filters.is_empty() && !global_search {
        all_repos.extend(input.projects.values().map(|p| labels.project_label(p)));
    }

    // Step 4b: order the groups. Pinned groups lead, in the user's pin
    // order (skipping pins with no group this pass); the rest follow in
    // the algorithmic `BTreeSet` (alphabetical) order. Pinning overrides
    // the default order only for the pinned repos and leaves everything
    // else untouched — the "pinned > algorithmic" compose rule (#760).
    let mut ordered_repos: Vec<String> = Vec::with_capacity(all_repos.len());
    for pinned in input.pinned_repos {
        if all_repos.remove(pinned) {
            ordered_repos.push(pinned.clone());
        }
    }
    ordered_repos.extend(all_repos);

    // Step 4c: attention-demoted (Muted / source-snoozed) groups sink
    // to the bottom of the list, after every Live/Quiet/Digest group —
    // a stable partition, so pins-then-alphabetical order is preserved
    // within each half. Demotion beats pinning: muting a pinned repo
    // means the user changed their mind about its prominence (#scale).
    let attention_of = |label: &str| {
        lazybox_config::effective_source_attention(
            input.source_attention,
            label,
            Some(&space_of(label, input.spaces)),
        )
    };
    let is_demoted = |label: &str| {
        attention_of(label).effective_level(input.now)
            == lazybox_config::SourceAttentionLevel::Muted
    };
    if !input.source_attention.is_empty() {
        let (kept, demoted): (Vec<String>, Vec<String>) =
            ordered_repos.into_iter().partition(|r| !is_demoted(r));
        ordered_repos = kept;
        ordered_repos.extend(demoted);
    }

    // Step 5: emit the row tree. Two shapes, chosen by whether the
    // Space tier is active. A source's Space comes from `space_of`
    // (explicit `ui.spaces` assignment, else owner auto-seed). The
    // tier only renders when it yields ≥2 distinct Spaces this pass —
    // a lone Space wrapping every repo is pure chrome, so we keep the
    // legacy flat shape byte-for-byte in that case.
    let mut visible: Vec<VisibleRow> = Vec::with_capacity(filtered.len() + ordered_repos.len() + 4);
    let mut summaries: BTreeMap<String, RepoSummary> = BTreeMap::new();
    let mut ticket_tree: HashMap<SessionKey, TicketTreeMeta> = HashMap::new();

    // Step 5a: the `★ Focused` section, first and above every repo /
    // Space. Only emitted when at least one starred workspace is visible
    // this pass. Session sub-rows follow the same 2+-sessions rule as
    // repo rows; no KindHeader split — the section is a flat, cross-repo
    // shortlist (#846).
    if !focused_rows.is_empty() {
        visible.push(VisibleRow::FocusedHeader);
        emit_workspace_forest(
            &focused_rows,
            input.collapsed_tickets,
            &context_only,
            &mut visible,
            &mut ticket_tree,
        );
    }

    // Step 5b: the personal Hopper follows Focused and stays outside
    // repository grouping even after a repo is assigned. Repository is
    // execution context; Hopper remains the workspace's ownership.
    if !hopper_rows.is_empty() {
        visible.push(VisibleRow::HopperHeader);
        emit_workspace_forest(
            &hopper_rows,
            input.collapsed_tickets,
            &context_only,
            &mut visible,
            &mut ticket_tree,
        );
    }

    // Step 5b-bis: the epic tier, below the personal shortlists and above
    // every Space / repo header. A collapsed epic keeps its header — the
    // header line *is* the status — and folds its members away.
    for (key, rows) in &epic_sections {
        visible.push(VisibleRow::EpicHeader((*key).to_string()));
        if input.collapsed_epics.contains(*key) {
            continue;
        }
        emit_workspace_forest(
            rows,
            input.collapsed_tickets,
            &context_only,
            &mut visible,
            &mut ticket_tree,
        );
    }

    // Step 5c: the repo groups (#860). Two shapes, chosen by whether the
    // Space tier is active. A source's Space comes from `space_of`
    // (explicit `ui.spaces` assignment, else owner auto-seed). The tier
    // only renders when it yields ≥2 distinct Spaces this pass — a lone
    // Space wrapping every repo is pure chrome, so we keep the legacy
    // flat shape byte-for-byte in that case.
    let space_of_repo: Vec<String> = ordered_repos
        .iter()
        .map(|r| space_of(r, input.spaces))
        .collect();
    let distinct_spaces: BTreeSet<&str> = space_of_repo.iter().map(String::as_str).collect();

    // The tier renders when it yields ≥2 distinct Spaces, OR when the
    // user has ANY explicit `ui.spaces` entry (#scale, proposal F):
    // the old ≥2-only gate silently hid the whole feature from
    // single-org users — an explicitly-created Space must show up, or
    // `x m` appears to do nothing.
    if distinct_spaces.len() < 2 && input.spaces.is_empty() {
        // Even without the Space tier rendered, an explicit source
        // order on the lone Space's config still reorders the flat
        // repo list (#1211) — so `x u`-style moves keep working when
        // every repo shares one owner. Stable sort: unlisted repos
        // keep the pins-then-alphabetical order.
        let mut flat_repos: Vec<&String> = ordered_repos.iter().collect();
        if let Some(space) = distinct_spaces.iter().next()
            && let Some(cfg) = input.spaces.iter().find(|s| s.name == **space)
        {
            // Demotion (step 4c) survives the explicit source order:
            // a muted repo sinks below every live one even when the
            // Space config lists it first.
            flat_repos.sort_by_key(|r| {
                (
                    is_demoted(r),
                    cfg.sources
                        .iter()
                        .position(|src| src == *r)
                        .unwrap_or(usize::MAX),
                )
            });
        }
        for repo in flat_repos {
            emit_repo_group(
                repo,
                &input,
                &by_repo,
                &mut visible,
                &mut summaries,
                &mut ticket_tree,
                &context_only,
            );
        }
    } else {
        // Space order: explicitly-configured Spaces present this pass
        // lead in `ui.spaces` order; the rest (owner auto-seed +
        // Ungrouped) follow alphabetically (the `BTreeSet` iteration).
        let mut space_order: Vec<String> = Vec::new();
        let mut placed: BTreeSet<String> = BTreeSet::new();
        for s in input.spaces {
            if distinct_spaces.contains(s.name.as_str()) && placed.insert(s.name.clone()) {
                space_order.push(s.name.clone());
            }
        }
        for s in &distinct_spaces {
            if placed.insert((*s).to_string()) {
                space_order.push((*s).to_string());
            }
        }
        // A Space muted at its own tier (`space:<name>`) sinks below
        // every non-muted Space — same stable partition as step 4c.
        if !input.source_attention.is_empty() {
            let space_demoted = |name: &str| {
                input
                    .source_attention
                    .get(&format!("space:{name}"))
                    .is_some_and(|att| {
                        att.effective_level(input.now)
                            == lazybox_config::SourceAttentionLevel::Muted
                    })
            };
            let (kept, demoted): (Vec<String>, Vec<String>) =
                space_order.into_iter().partition(|s| !space_demoted(s));
            space_order = kept;
            space_order.extend(demoted);
        }

        for space in &space_order {
            visible.push(VisibleRow::SpaceHeader(space.clone()));
            if input.collapsed_spaces.contains(space) {
                continue;
            }
            // Repos in this Space, in `ordered_repos` order (which
            // already encodes pins > alphabetical); an explicit source
            // list in the matching Space config reorders within it.
            let cfg = input.spaces.iter().find(|s| &s.name == space);
            let mut repos: Vec<&String> = ordered_repos
                .iter()
                .zip(&space_of_repo)
                .filter(|(_, sp)| *sp == space)
                .map(|(r, _)| r)
                .collect();
            if let Some(cfg) = cfg {
                repos.sort_by_key(|r| {
                    (
                        is_demoted(r),
                        cfg.sources
                            .iter()
                            .position(|src| src == *r)
                            .unwrap_or(usize::MAX),
                    )
                });
            }
            for repo in repos {
                emit_repo_group(
                    repo,
                    &input,
                    &by_repo,
                    &mut visible,
                    &mut summaries,
                    &mut ticket_tree,
                    &context_only,
                );
            }
        }
    }

    ComputeOutcome {
        visible,
        summaries,
        ticket_tree,
        agent_excerpts,
    }
}

/// Emit one repo group — its `RepoHeader`, the per-repo summary, and
/// (unless collapsed) its workspace rows, `KindHeader`s, and session
/// sub-rows. Shared by the flat and Space-tiered layouts so the
/// within-group shape can't drift between them.
fn emit_repo_group<'a>(
    repo: &str,
    input: &ComputeInputs<'a>,
    by_repo: &BTreeMap<String, Vec<(&'a SessionKey, &'a Workspace)>>,
    visible: &mut Vec<VisibleRow>,
    summaries: &mut BTreeMap<String, RepoSummary>,
    ticket_tree: &mut HashMap<SessionKey, TicketTreeMeta>,
    context_only: &HashSet<SessionKey>,
) {
    visible.push(VisibleRow::RepoHeader(repo.to_string()));
    let mut summary = RepoSummary::default();
    // Source-attention ladder (#scale): resolve this group's effective
    // level once. Quiet/Digest/Muted suppress the ambient attention
    // count — only punch-through rows keep counting — and a collapsed
    // Muted group still emits its punch-through rows below the header
    // (direct address is never hidden).
    let source_att = lazybox_config::effective_source_attention(
        input.source_attention,
        repo,
        Some(&space_of(repo, input.spaces)),
    );
    let level = source_att.effective_level(input.now);
    if level != lazybox_config::SourceAttentionLevel::Live {
        summary.source_attention = Some(level.label().to_string());
    }
    summary.source_snooze_until_epoch_ms = source_att
        .active_snooze(input.now)
        .map(|until| until.timestamp_millis());
    if let Some(rows) = by_repo.get(repo) {
        summary.active = rows
            .iter()
            .filter(|(key, _)| !context_only.contains(*key))
            .count();
        for (key, w) in rows {
            if context_only.contains(*key) {
                continue;
            }
            if workspace_needs_attention(w, input.attention, input.agents)
                && (level == lazybox_config::SourceAttentionLevel::Live
                    || punches_through(w, input.agents))
            {
                summary.attention += 1;
            }
            // Kind / role breakdown for the header (#1744). The kind
            // follows the row's type glyph precedence (a folded issue+PR
            // row is a PR); the role is the primary task's, so it is the
            // same letter the row leads with.
            if w.pr.is_some() {
                summary.prs += 1;
            } else if !w.gh_issues.is_empty() {
                summary.issues += 1;
            } else if !w.linear_issues.is_empty() {
                summary.tickets += 1;
            }
            match w.primary_task().map(|t| t.role) {
                Some(lazybox_core::TaskRole::Author) => summary.authored += 1,
                Some(lazybox_core::TaskRole::Reviewer) => summary.reviewing += 1,
                Some(lazybox_core::TaskRole::Assignee) => summary.assigned += 1,
                Some(lazybox_core::TaskRole::Mentioned) | None => {}
            }
        }
        if input.collapsed_repos.contains(repo) {
            if level == lazybox_config::SourceAttentionLevel::Muted {
                let punch: Vec<_> = rows
                    .iter()
                    .copied()
                    .filter(|(key, w)| {
                        !context_only.contains(*key) && punches_through(w, input.agents)
                    })
                    .collect();
                if !punch.is_empty() {
                    emit_workspace_forest(
                        &punch,
                        input.collapsed_tickets,
                        context_only,
                        visible,
                        ticket_tree,
                    );
                }
            }
        } else {
            // ByRoleSplit drops a `KindHeader` between the PR
            // workspaces and the Issue workspaces of this repo.
            // Step 3 already sorted PRs ahead of issues, so a
            // single linear pass detects the boundary cleanly.
            // In other sort modes the kind header is suppressed.
            if input.sort_mode == SortMode::ByRoleSplit {
                for kind in [
                    WorkspaceKind::Pr,
                    WorkspaceKind::Issue,
                    WorkspaceKind::Other,
                ] {
                    let band: Vec<_> = rows
                        .iter()
                        .copied()
                        .filter(|(_, w)| WorkspaceKind::classify(w) == kind)
                        .collect();
                    if band.is_empty() {
                        continue;
                    }
                    visible.push(VisibleRow::KindHeader(kind));
                    emit_workspace_forest(
                        &band,
                        input.collapsed_tickets,
                        context_only,
                        visible,
                        ticket_tree,
                    );
                }
            } else {
                emit_workspace_forest(
                    rows,
                    input.collapsed_tickets,
                    context_only,
                    visible,
                    ticket_tree,
                );
            }
        }
    }
    summaries.insert(repo.to_string(), summary);
}

/// Emit a stable preorder forest over a pre-sorted workspace slice.
/// Parent links are honored only when both endpoints are visible in this
/// exact section. Missing/cross-project parents therefore degrade to roots.
/// Cycles are broken deterministically at the first sorted member so corrupt
/// provider data can never hide rows or recurse forever.
fn emit_workspace_forest(
    rows: &[(&SessionKey, &Workspace)],
    collapsed: &HashSet<TaskId>,
    context_only: &HashSet<SessionKey>,
    visible: &mut Vec<VisibleRow>,
    ticket_tree: &mut HashMap<SessionKey, TicketTreeMeta>,
) {
    if rows.is_empty() {
        return;
    }

    // Address each workspace by *every* provider id it holds, not just the
    // headline task. A ticket that has acquired a PR reports the PR as its
    // `primary_task`, yet its children still reference the ticket id — so
    // indexing on the headline alone would orphan the whole subtree.
    let mut task_index: HashMap<TaskId, usize> = HashMap::new();
    for (index, (_, workspace)) in rows.iter().enumerate() {
        for id in workspace.hierarchy_task_ids() {
            task_index.insert(id.clone(), index);
        }
    }

    let mut parents: Vec<Option<usize>> = rows
        .iter()
        .enumerate()
        .map(|(index, (_, workspace))| {
            workspace
                .hierarchy_parent()
                .and_then(|parent| task_index.get(parent).copied())
                .filter(|parent| *parent != index)
        })
        .collect();

    // Break every cycle without recursion. Walking up from each start until we
    // reach a real root, a node already proven to reach one, or a repeat (the
    // cycle) — cutting only the entry node so the rest of the chain keeps its
    // structure. `resolves` memoizes proven-safe nodes so shared ancestry is
    // never re-walked, keeping this linear on deep chains instead of O(n²).
    // Iterating in sorted order makes the chosen root stable across renders.
    let mut resolves = vec![false; parents.len()];
    let mut path: Vec<usize> = Vec::new();
    // Clear-and-reuse like `path` — a fresh HashSet per start node was
    // N container allocations per group per rebuild (U6).
    let mut seen: HashSet<usize> = HashSet::new();
    for start in 0..parents.len() {
        path.clear();
        seen.clear();
        let mut cursor = start;
        let reaches_root = loop {
            if resolves[cursor] {
                break true;
            }
            if !seen.insert(cursor) {
                break false;
            }
            path.push(cursor);
            match parents[cursor] {
                None => break true,
                Some(parent) => cursor = parent,
            }
        };
        if reaches_root {
            for node in &path {
                resolves[*node] = true;
            }
        } else {
            parents[start] = None;
            resolves[start] = true;
        }
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    for (child, parent) in parents.iter().enumerate() {
        if let Some(parent) = parent {
            children[*parent].push(child);
        }
    }

    let roots: Vec<usize> = parents
        .iter()
        .enumerate()
        .filter_map(|(index, parent)| parent.is_none().then_some(index))
        .collect();

    let mut emitted = HashSet::new();
    for root in roots {
        if emitted.contains(&root) {
            continue;
        }
        let mut stack = vec![(root, 0usize)];
        while let Some((index, depth)) = stack.pop() {
            if !emitted.insert(index) {
                continue;
            }
            let (key, workspace) = rows[index];
            let has_children = !children[index].is_empty();
            let task_id = workspace.primary_task().map(|task| &task.id);
            let is_context = context_only.contains(key);
            // A context-only row is present solely because a descendant matched
            // the active search/filter. Honoring its stored collapse here would
            // fold that match back out of view — the search would silently drop
            // the very row it found. Never collapse an ancestor kept for
            // context; user-driven collapse still applies to rows that matched.
            let is_collapsed =
                has_children && !is_context && task_id.is_some_and(|id| collapsed.contains(id));
            visible.push(VisibleRow::Workspace(key.clone()));
            ticket_tree.insert(
                key.clone(),
                TicketTreeMeta {
                    depth,
                    has_children,
                    collapsed: is_collapsed,
                    context_only: is_context,
                },
            );
            emit_session_rows(key, workspace, visible);
            if !is_collapsed {
                for child in children[index].iter().rev() {
                    stack.push((*child, depth.saturating_add(1)));
                }
            }
        }
    }
}

fn emit_session_rows(key: &SessionKey, workspace: &Workspace, visible: &mut Vec<VisibleRow>) {
    if workspace.session_count() < 2 {
        return;
    }
    let mut sessions: Vec<&lazybox_core::WorkspaceSession> = workspace.sessions.iter().collect();
    sessions.sort_by_key(|session| session.created_at);
    for session in sessions {
        visible.push(VisibleRow::Session {
            workspace: key.clone(),
            session_id: session.id,
        });
    }
}

fn github_task_repo(w: &Workspace, project_key: &ProjectKey) -> Option<String> {
    if project_key.source_prefix() != "github" {
        return None;
    }
    let task = w.primary_task()?;
    let repo = task.repo.as_deref()?.trim();
    (!repo.is_empty() && lazybox_core::project_key_for_task(task).as_ref() == Some(project_key))
        .then(|| repo.to_string())
}

pub fn project_label(project: &Project, workspaces: &HashMap<SessionKey, Workspace>) -> String {
    workspaces
        .values()
        .filter(|w| lazybox_core::workspace_project_key(w).as_ref() == Some(&project.key))
        .find_map(|w| github_task_repo(w, &project.key))
        .unwrap_or_else(|| project.display_name())
}

/// One-pass memo behind [`group_label`] / [`project_label`] for the
/// hot compute path (2026-08-19 audit, U6). `project_label` scans
/// EVERY workspace per call, and the collapsible-hierarchy pass
/// (812edba0) calls `group_label` once per row *and once per ancestor
/// step* — O(W²) string-allocating work per recompute. Building this
/// map is one O(W) pass; each lookup is O(1) with identical labels.
pub struct ProjectRepoLabels {
    by_project: HashMap<ProjectKey, String>,
}

impl ProjectRepoLabels {
    pub fn build(workspaces: &HashMap<SessionKey, Workspace>) -> Self {
        let mut by_project = HashMap::new();
        for w in workspaces.values() {
            if let Some(pk) = lazybox_core::workspace_project_key(w)
                && !by_project.contains_key(&pk)
                && let Some(repo) = github_task_repo(w, &pk)
            {
                by_project.insert(pk, repo);
            }
        }
        Self { by_project }
    }

    /// Memoized [`project_label`].
    pub fn project_label(&self, project: &Project) -> String {
        self.by_project
            .get(&project.key)
            .cloned()
            .unwrap_or_else(|| project.display_name())
    }

    /// Memoized [`group_label`] — same resolution order, same labels.
    pub fn group_label(&self, w: &Workspace, projects: &BTreeMap<ProjectKey, Project>) -> String {
        if let Some(pk) = lazybox_core::workspace_project_key(w) {
            if let Some(repo) = github_task_repo(w, &pk) {
                return repo;
            }
            if let Some(p) = projects.get(&pk) {
                return self.project_label(p);
            }
        }
        if let Some(repo) = w.primary_task().and_then(|t| t.repo.clone())
            && !repo.is_empty()
        {
            return repo;
        }
        NO_REPO.to_string()
    }
}

pub fn group_label(
    w: &Workspace,
    projects: &BTreeMap<ProjectKey, Project>,
    workspaces: &HashMap<SessionKey, Workspace>,
) -> String {
    // A matching GitHub task carries the owner/repo boundary that the
    // flat project key loses. Otherwise use the project record label so
    // moved/local workspaces still group under their chosen project.
    if let Some(pk) = lazybox_core::workspace_project_key(w) {
        if let Some(repo) = github_task_repo(w, &pk) {
            return repo;
        }
        if let Some(p) = projects.get(&pk) {
            return project_label(p, workspaces);
        }
    }
    // Workspace knows its project but we haven't seen the record
    // yet (startup race, or polling hasn't completed). Fall back
    // to the workspace's primary task's repo — for github
    // workspaces this is the same `"owner/repo"` string the
    // project record carries, so once polling registers the
    // project the bucket label is identical and no row jumps.
    if let Some(repo) = w.primary_task().and_then(|t| t.repo.clone())
        && !repo.is_empty()
    {
        return repo;
    }
    // Orphan: no project_key AND no upstream task. The old
    // "(sandbox)" bucket retired in Stage 4 — those workspaces
    // land here too. A future migration can lift them into a
    // local Project by name.
    NO_REPO.to_string()
}

/// Convert `Task.id.key` (e.g. `"owner/repo#1234"`) to the trailing
/// integer. Returns `None` when there's no `#`-suffix or the suffix
/// isn't a number — those cases shouldn't happen for GitHub-derived
/// tasks today, but we don't want to panic if Linear or a custom
/// provider lands here. A pure dependency of both [`search_matches`]
/// and the sidebar row renderer (`task_label::pr_number_color` colors
/// the number the TUI side keeps).
pub fn pr_number(task: &Task) -> Option<u64> {
    let key = task.id.key.as_str();
    let (_, num) = key.rsplit_once('#')?;
    num.parse().ok()
}

/// What the sidebar's identifier column shows for `task`: the bare PR /
/// issue number for GitHub-style `owner/repo#N` keys, otherwise the
/// tracker's own identifier (`ENG-123`, `PROJ-42`). `None` only for an
/// empty key. Width via [`identifier_width`] so the column can be sized
/// without allocating on the render hot path.
pub fn task_identifier(task: &Task) -> Option<String> {
    if let Some(n) = pr_number(task) {
        return Some(n.to_string());
    }
    let key = task.id.key.as_str();
    (!key.is_empty()).then(|| key.to_string())
}

/// Display width of [`task_identifier`] without building the string.
pub fn identifier_width(task: &Task) -> Option<usize> {
    if let Some(n) = pr_number(task) {
        return Some(1 + n.checked_ilog10().unwrap_or(0) as usize);
    }
    let key = task.id.key.as_str();
    (!key.is_empty()).then(|| key.chars().count())
}

/// Does `query` match `w`? Matches when the query is a case-insensitive
/// fuzzy (subsequence) match on the workspace's displayed title, OR a
/// substring match on any of its searchable metadata: PR/issue number,
/// repo, labels, or requested reviewers / assignees. A leading `#` on
/// the query is ignored so both `100` and `#100` find issue #100. An
/// empty query (after trimming) matches everything — callers guard
/// against that, but it keeps the function total.
pub fn search_matches(query: &str, w: &Workspace) -> bool {
    search_evaluate(query, w, None).matched
}

/// What a search evaluation found. [`search_matches`] is the boolean
/// projection of this; the extra field carries the agent-text excerpt so
/// the filter and the row's "why did this match" cue come from ONE pass
/// over the query — the same no-drift discipline `search_scope_covers`
/// enforces for the title highlight (#1099).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchHit {
    pub matched: bool,
    /// Text around the first `agent:` hit, whitespace-collapsed and
    /// ellipsized to [`AGENT_EXCERPT_CHARS`]. `None` whenever no
    /// `agent:` term contributed — including for a row that matched on
    /// metadata alone, which has nothing extra to explain.
    pub agent_excerpt: Option<String>,
}

impl SearchHit {
    const MISS: Self = Self {
        matched: false,
        agent_excerpt: None,
    };
}

/// Length budget, in `char`s, for the excerpt an `agent:` hit renders
/// (#1774). Wide enough to carry the match plus enough surrounding words
/// to recognize the moment, short enough to ride in the title cell's
/// sheddable tail rather than fight the title for space. Counted in
/// chars, not display columns, so a full-width (CJK) excerpt draws wider
/// than this — harmless, because the renderer sheds the whole cue on a
/// narrow row rather than letting it overflow.
pub const AGENT_EXCERPT_CHARS: usize = 48;

/// The qualifiers that search agent text. `said:` reads better for a
/// phrase the agent produced, `agent:` for a topic — same corpus.
const AGENT_QUALIFIERS: [&str; 2] = ["agent", "said"];

/// [`search_matches`], plus the agent-text corpus for this workspace and
/// the excerpt of what matched in it. `agent_text` is `None` for a
/// workspace that has never run an agent — an `agent:` term then simply
/// doesn't match it, which is why the search filter can pass `None`
/// freely rather than guarding first.
pub fn search_evaluate(query: &str, w: &Workspace, agent_text: Option<&str>) -> SearchHit {
    let raw = normalized_query(query).to_lowercase();
    if raw.is_empty() {
        return SearchHit {
            matched: true,
            agent_excerpt: None,
        };
    }
    // Qualifier grammar (#scale, C2): whitespace-split terms AND
    // together. A term is `-`-negatable and either field-qualified —
    // `author:x` `reviewer:x` `assignee:x` `repo:x` `label:x`
    // `is:pr|issue|draft` `@login` (the involves role-union) — or bare
    // text. Bare terms re-join into one blob matched exactly like the
    // pre-grammar behavior (number / repo / labels / people substring,
    // fuzzy-subsequence title), so a query with no qualifiers is
    // byte-for-byte the legacy search.
    let mut bare: Vec<&str> = Vec::new();
    let mut agent_excerpt: Option<String> = None;
    for term in search_terms(&raw) {
        let (negated, term) = match term.strip_prefix('-') {
            Some(rest) if !rest.is_empty() => (true, rest),
            _ => (false, term),
        };
        // `agent:` / `said:` reach outside the workspace record, so they
        // resolve here rather than in `qualified_term_matches` (which is
        // a pure function of the task). The first hit's excerpt is the
        // one the row shows.
        if let Some(value) = agent_qualifier_value(term) {
            let hit = agent_text.and_then(|text| agent_excerpt_around(text, value));
            if hit.is_some() == negated {
                return SearchHit::MISS;
            }
            if agent_excerpt.is_none() {
                agent_excerpt = hit;
            }
            continue;
        }
        match qualified_term_matches(term, w) {
            // A qualified term must match (or, negated, must NOT).
            Some(matched) => {
                if matched == negated {
                    return SearchHit::MISS;
                }
            }
            // Not a recognized qualifier: a negated bare term excludes
            // rows its blob-match would hit; positive ones join the
            // combined blob below.
            None => {
                if negated {
                    if bare_blob_matches(term, w) {
                        return SearchHit::MISS;
                    }
                } else {
                    bare.push(term);
                }
            }
        }
    }
    let matched = bare.is_empty() || bare_blob_matches(&bare.join(" "), w);
    SearchHit {
        matched,
        agent_excerpt: matched.then_some(agent_excerpt).flatten(),
    }
}

/// Split a query into terms. Whitespace-separated, except that a `"`
/// opening immediately after a qualifier's `:` runs to the next `"` — so
/// `said:"cannot borrow"` is one term and multi-word phrases are
/// expressible. The quotes stay inside the returned slice (it borrows
/// from `raw`); the value parser strips them.
///
/// A query with no `field:"` sequence tokenizes exactly as
/// `split_whitespace` did, which is what keeps a bare query byte-for-byte
/// the legacy search.
fn search_terms(raw: &str) -> Vec<&str> {
    let mut terms = Vec::new();
    let mut start: Option<usize> = None;
    let mut quoted = false;
    let mut prev = '\0';
    for (i, ch) in raw.char_indices() {
        match start {
            // Splitting on `char::is_whitespace`, not the ASCII subset:
            // `split_whitespace` uses the Unicode property, so an NBSP or
            // an ideographic space pasted into the query has to keep
            // separating terms exactly as it did before.
            None => {
                if !ch.is_whitespace() {
                    start = Some(i);
                }
            }
            Some(s) => {
                if quoted {
                    quoted = ch != '"';
                } else if ch == '"' && prev == ':' && raw[i + 1..].contains('"') {
                    // Only open a quoted run that actually CLOSES. Without
                    // the lookahead a missing closing quote swallowed the
                    // rest of the query into one needle — `said:"a b is:pr`
                    // became a single term and the `is:pr` filter vanished
                    // silently, which is easy to hit mid-typing.
                    quoted = true;
                } else if ch.is_whitespace() {
                    terms.push(&raw[s..i]);
                    start = None;
                }
            }
        }
        prev = ch;
    }
    if let Some(s) = start {
        terms.push(&raw[s..]);
    }
    terms
}

/// The needle of an `agent:` / `said:` term, or `None` when `term` is
/// anything else. An empty value (`agent:`) is not a qualifier — it
/// falls through to bare text rather than matching every row.
fn agent_qualifier_value(term: &str) -> Option<&str> {
    let (field, value) = term.split_once(':')?;
    if !AGENT_QUALIFIERS.contains(&field) {
        return None;
    }
    let value = unquote(value);
    (!value.is_empty()).then_some(value)
}

/// Drop one pair of surrounding double quotes, if present.
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .map_or(value, |v| v.strip_suffix('"').unwrap_or(v))
}

/// One field-qualified search term. `None` = not a qualifier (bare
/// text). Values match case-insensitively by substring; `@login`
/// matches the [`task_involves`] role-union.
fn qualified_term_matches(term: &str, w: &Workspace) -> Option<bool> {
    let task = w.primary_task();
    if let Some(login) = term.strip_prefix('@').filter(|l| !l.is_empty()) {
        return Some(task.is_some_and(|t| task_involves(t, login)));
    }
    let (field, value) = term.split_once(':')?;
    let value = unquote(value);
    if value.is_empty() {
        return None;
    }
    let contains = |hay: &str| hay.to_lowercase().contains(value);
    Some(match field {
        "author" => task.is_some_and(|t| contains(&t.author)),
        "reviewer" => task.is_some_and(|t| {
            t.reviewers.iter().any(|r| contains(r)) || t.reviews.iter().any(|r| contains(&r.login))
        }),
        "assignee" => task.is_some_and(|t| t.assignees.iter().any(|a| contains(a))),
        "repo" => task.is_some_and(|t| t.repo.as_deref().is_some_and(contains)),
        "label" => task.is_some_and(|t| t.labels.iter().any(|l| contains(&l.name))),
        "is" => match value {
            "pr" => WorkspaceKind::classify(w) == WorkspaceKind::Pr,
            "issue" => WorkspaceKind::classify(w) == WorkspaceKind::Issue,
            "draft" => task.is_some_and(|t| t.state == lazybox_core::TaskState::Draft),
            // Unknown `is:` value — treat as bare text, not a
            // silently-false filter that empties the sidebar.
            _ => return None,
        },
        _ => return None,
    })
}

/// The pre-grammar single-blob match: PR number, repo, labels,
/// requested people by substring; title (else workspace name) by
/// fuzzy subsequence.
fn bare_blob_matches(q: &str, w: &Workspace) -> bool {
    let task = w.primary_task();
    if let Some(n) = task.and_then(pr_number)
        && n.to_string().contains(q)
    {
        return true;
    }
    if let Some(t) = task {
        // Substring matches on metadata: repo, labels, and the people
        // requested on the task (reviewers / assignees).
        if t.repo
            .as_deref()
            .is_some_and(|r| r.to_lowercase().contains(q))
            || t.labels.iter().any(|l| l.name.to_lowercase().contains(q))
            || t.reviewers.iter().any(|r| r.to_lowercase().contains(q))
            || t.assignees.iter().any(|a| a.to_lowercase().contains(q))
        {
            return true;
        }
    }
    // Same title the workspace row renders: task title, else the
    // workspace's own name.
    let title = task
        .map(|t| t.title.as_str())
        .unwrap_or_else(|| w.name.as_str());
    is_subsequence(&title.to_lowercase(), q)
}

/// Whether the active search's scope *covers* workspace `w` — the set of
/// rows the search actually filters. A global search (`scope: None`)
/// covers every row; a scoped search (`scope: Some`) covers only its own
/// repo group; no search, or an empty query, covers nothing.
///
/// This is the single source of truth shared by [`compute_visible`]'s
/// search filter and the sidebar's match-highlight decision, so the two
/// can't drift (#1099): a covered row that survives the filter is exactly
/// a row the query matched — hence the one whose title gets highlighted —
/// while an uncovered (out-of-scope) row is shown untouched and never
/// highlighted even if its title happens to contain the query text.
pub fn search_scope_covers(
    search: Option<&SearchState>,
    w: &Workspace,
    projects: &BTreeMap<ProjectKey, Project>,
    workspaces: &HashMap<SessionKey, Workspace>,
) -> bool {
    match search {
        // Emptiness is judged on the *normalized* query — the same reduction
        // `search_matches` and the highlight use — so "covers", "matches",
        // and "highlights" all agree on when a query is blank (a whitespace-
        // or `#`-only query covers nothing, exactly as it highlights nothing).
        Some(s) if !normalized_query(&s.query).is_empty() => match &s.scope {
            None => true,
            Some(scope) => group_label(w, projects, workspaces) == *scope,
        },
        _ => false,
    }
}

/// The query reduced to its meaningful core: surrounding whitespace
/// trimmed and a leading `#` dropped (so `100` and `#100` are the same
/// search, and a blank or `#`-only query is empty). The single
/// normalization shared by the search filter ([`search_matches`]), the
/// scope test ([`search_scope_covers`]), and the sidebar's match
/// highlight, so all three agree on when a query is "empty" (#1099).
pub fn normalized_query(query: &str) -> &str {
    query.trim().trim_start_matches('#')
}

/// Byte offset into `hay` of the first case-insensitive occurrence of
/// `needle` (already `to_lowercase`d by the caller).
///
/// Folds the haystack a char at a time through `char::to_lowercase`
/// rather than lowercasing it into a new `String`: the corpus is orders
/// of magnitude larger than any metadata field and the search re-runs on
/// every keystroke, so an allocation per row per keystroke is what this
/// avoids. It matters that the fold is the SAME one `str::to_lowercase`
/// applied to the needle — folding the haystack ASCII-only would make
/// this the one qualifier in the grammar that is case-sensitive for
/// non-ASCII, and asymmetrically so (`agent:CAFÉ` would find `café`
/// while `agent:café` missed `CAFÉ`). `to_lowercase` yields an iterator,
/// so the multi-char expansions (`İ` → `i̇`) line up on both sides
/// without either allocating.
///
/// The offset indexes the ORIGINAL `hay`, so the excerpt reads back in
/// the case the user wrote — which a lowercased copy could not give,
/// since lowercasing is not length-preserving and its offsets would not
/// map back.
fn ci_find(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    hay.char_indices().map(|(i, _)| i).find(|&i| {
        let mut folded = hay[i..].chars().flat_map(char::to_lowercase);
        needle.chars().all(|n| folded.next() == Some(n))
    })
}

/// A one-line window of `text` around its first case-insensitive
/// occurrence of `needle`, for the row's "why did this match" cue.
/// Whitespace (newlines included — the corpus is multi-line) collapses
/// to single spaces, and the window is ellipsized on whichever side it
/// was cut. `None` when `needle` isn't there.
fn agent_excerpt_around(text: &str, needle: &str) -> Option<String> {
    let at = ci_find(text, needle)?;
    // A little lead-in so the match reads in context rather than opening
    // the excerpt, but short enough that the match itself always fits.
    let lead = AGENT_EXCERPT_CHARS / 4;
    let start = text[..at]
        .char_indices()
        .nth_back(lead.saturating_sub(1))
        .map_or(0, |(i, _)| i);
    let mut out = String::new();
    let mut last_was_space = true;
    let mut taken = 0usize;
    let mut end = start;
    for (offset, ch) in text[start..].char_indices() {
        if taken >= AGENT_EXCERPT_CHARS {
            break;
        }
        end = start + offset + ch.len_utf8();
        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                taken += 1;
                last_was_space = true;
            }
            continue;
        }
        out.push(ch);
        taken += 1;
        last_was_space = false;
    }
    let out = out.trim_end();
    let mut excerpt = String::with_capacity(out.len() + 6);
    if start > 0 {
        excerpt.push('…');
    }
    excerpt.push_str(out);
    if end < text.len() {
        excerpt.push('…');
    }
    Some(excerpt)
}

/// True when every char of `needle` appears in `haystack` in order
/// (not necessarily contiguous) — the fzf-style loose match.
fn is_subsequence(haystack: &str, needle: &str) -> bool {
    let mut hay = haystack.chars();
    needle.chars().all(|nc| hay.any(|hc| hc == nc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use lazybox_core::{
        CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace, WorkspaceKey,
    };

    fn fixed_time() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap()
    }

    fn workspace_with_task(key_str: &str, repo: Option<&str>, minutes_old: i64) -> Workspace {
        let task = Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: format!("owner/{}#1", repo.unwrap_or("repo")),
            },
            title: "x".into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "x".into(),
            repo: repo.map(String::from),
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
            blocked_by: vec![],
            merge_after: vec![],
            contracts: vec![],
            blocked_on: None,
        };
        let mut ws = Workspace::from_task(task, fixed_time());
        ws.key = WorkspaceKey(key_str.into());
        ws
    }

    fn linear_ticket(
        workspace_key: &str,
        identifier: &str,
        parent: Option<&str>,
        minutes_old: i64,
    ) -> Workspace {
        let mut workspace = workspace_with_task(workspace_key, Some("linear/ENG"), minutes_old);
        let mut task = workspace.gh_issues.remove(0);
        task.id = TaskId {
            source: "linear".into(),
            key: identifier.into(),
        };
        task.title = identifier.into();
        task.kind = Some(lazybox_core::TaskKind::Issue);
        task.parent = parent.map(|key| TaskId {
            source: "linear".into(),
            key: key.into(),
        });
        workspace.linear_issues.push(task);
        workspace
    }

    /// A Linear ticket that the poller has merged with its linked PR: the PR
    /// is the headline task (`primary_task`) and carries no parent, while the
    /// ticket keeps its hierarchy on `linear_issues`. Mirrors the merge in
    /// `server::polling` (issue tasks absorbed onto the PR workspace).
    fn linear_ticket_with_pr(
        workspace_key: &str,
        identifier: &str,
        parent: Option<&str>,
        minutes_old: i64,
    ) -> Workspace {
        let mut workspace = linear_ticket(workspace_key, identifier, parent, minutes_old);
        let mut pr = workspace.linear_issues[0].clone();
        pr.id = TaskId {
            source: "github".into(),
            key: format!("owner/eng#{identifier}"),
        };
        pr.kind = Some(lazybox_core::TaskKind::Pr);
        pr.parent = None;
        workspace.pr = Some(pr);
        workspace
    }

    fn visible_workspace_keys(outcome: &ComputeOutcome) -> Vec<&str> {
        outcome
            .visible
            .iter()
            .filter_map(|row| match row {
                VisibleRow::Workspace(key) => Some(key.as_str()),
                _ => None,
            })
            .collect()
    }

    fn inputs<'a>(
        workspaces: &'a HashMap<SessionKey, Workspace>,
        _subscribed: &'a BTreeSet<String>,
        collapsed: &'a BTreeSet<String>,
        attention: &'a lazybox_config::AttentionConfig,
        asking: &'a HashMap<SessionKey, lazybox_ipc::AgentState>,
        projects: &'a BTreeMap<ProjectKey, Project>,
    ) -> ComputeInputs<'a> {
        static NO_FILTERS: FilterSet = FilterSet::new();
        static NO_SPACES: Vec<lazybox_config::SpaceConfig> = Vec::new();
        static NO_COLLAPSED_SPACES: BTreeSet<String> = BTreeSet::new();
        static NO_SOURCE_ATTENTION: BTreeMap<String, lazybox_config::SourceAttention> =
            BTreeMap::new();
        static NO_COLLAPSED_TICKETS: std::sync::LazyLock<HashSet<TaskId>> =
            std::sync::LazyLock::new(HashSet::new);
        static NO_EPICS: BTreeMap<String, lazybox_ipc::EpicSnapshot> = BTreeMap::new();
        static NO_COLLAPSED_EPICS: BTreeSet<String> = BTreeSet::new();
        static NO_AGENT_TEXT: std::sync::LazyLock<HashMap<SessionKey, String>> =
            std::sync::LazyLock::new(HashMap::new);
        ComputeInputs {
            workspaces,
            mailbox: Mailbox::Inbox,
            filters: &NO_FILTERS,
            sort_mode: SortMode::Recent,
            show_inactive_in_inbox: false,
            projects,
            collapsed_repos: collapsed,
            pinned_repos: &[],
            focused_workspaces: &[],
            spaces: &NO_SPACES,
            collapsed_spaces: &NO_COLLAPSED_SPACES,
            source_attention: &NO_SOURCE_ATTENTION,
            collapsed_tickets: &NO_COLLAPSED_TICKETS,
            attention,
            agents: asking,
            epics: &NO_EPICS,
            collapsed_epics: &NO_COLLAPSED_EPICS,
            now: fixed_time(),
            search: None,
            agent_text: &NO_AGENT_TEXT,
        }
    }

    /// Empty workspace map + no subscribed repos = no rows.
    #[test]
    fn empty_inputs_produce_empty_visible() {
        let ws = HashMap::new();
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert!(out.visible.is_empty());
        assert!(out.summaries.is_empty());
    }

    /// One workspace under one repo: header + workspace row.
    #[test]
    fn single_workspace_emits_header_then_row() {
        let mut ws = HashMap::new();
        let w = workspace_with_task("k1", Some("owner/r"), 10);
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert_eq!(out.visible.len(), 2);
        assert!(matches!(out.visible[0], VisibleRow::RepoHeader(_)));
        assert!(matches!(out.visible[1], VisibleRow::Workspace(_)));
    }

    #[test]
    fn linear_tickets_emit_parent_before_nested_descendants() {
        let mut workspaces = HashMap::new();
        for workspace in [
            linear_ticket("child", "ENG-2", Some("ENG-1"), 1),
            linear_ticket("grandchild", "ENG-3", Some("ENG-2"), 2),
            linear_ticket("parent", "ENG-1", None, 10),
        ] {
            workspaces.insert(SessionKey::from(&workspace.key), workspace);
        }
        let collapsed_repos = BTreeSet::new();
        let attention = lazybox_config::AttentionConfig::default();
        let agents = HashMap::new();
        let projects = BTreeMap::new();
        let outcome = compute_visible(inputs(
            &workspaces,
            &BTreeSet::new(),
            &collapsed_repos,
            &attention,
            &agents,
            &projects,
        ));

        assert_eq!(
            visible_workspace_keys(&outcome),
            vec!["parent", "child", "grandchild"]
        );
        assert_eq!(outcome.ticket_tree[&SessionKey::new("parent")].depth, 0);
        assert!(outcome.ticket_tree[&SessionKey::new("parent")].has_children);
        assert_eq!(outcome.ticket_tree[&SessionKey::new("child")].depth, 1);
        assert_eq!(outcome.ticket_tree[&SessionKey::new("grandchild")].depth, 2);
    }

    #[test]
    fn collapsed_parent_hides_all_visible_descendants() {
        let mut workspaces = HashMap::new();
        for workspace in [
            linear_ticket("parent", "ENG-1", None, 10),
            linear_ticket("child", "ENG-2", Some("ENG-1"), 1),
            linear_ticket("grandchild", "ENG-3", Some("ENG-2"), 2),
        ] {
            workspaces.insert(SessionKey::from(&workspace.key), workspace);
        }
        let collapsed_repos = BTreeSet::new();
        let attention = lazybox_config::AttentionConfig::default();
        let agents = HashMap::new();
        let projects = BTreeMap::new();
        let mut collapsed_tickets = HashSet::new();
        collapsed_tickets.insert(TaskId {
            source: "linear".into(),
            key: "ENG-1".into(),
        });
        let subscribed = BTreeSet::new();
        let mut input = inputs(
            &workspaces,
            &subscribed,
            &collapsed_repos,
            &attention,
            &agents,
            &projects,
        );
        input.collapsed_tickets = &collapsed_tickets;
        let outcome = compute_visible(input);

        assert_eq!(visible_workspace_keys(&outcome), vec!["parent"]);
        assert!(outcome.ticket_tree[&SessionKey::new("parent")].collapsed);
    }

    #[test]
    fn search_keeps_nonmatching_parent_as_context() {
        let mut workspaces = HashMap::new();
        for workspace in [
            linear_ticket("parent", "ENG-1", None, 10),
            linear_ticket("child", "ENG-2", Some("ENG-1"), 1),
        ] {
            workspaces.insert(SessionKey::from(&workspace.key), workspace);
        }
        let collapsed_repos = BTreeSet::new();
        let attention = lazybox_config::AttentionConfig::default();
        let agents = HashMap::new();
        let projects = BTreeMap::new();
        let search = global_search("ENG-2");
        let subscribed = BTreeSet::new();
        let mut input = inputs(
            &workspaces,
            &subscribed,
            &collapsed_repos,
            &attention,
            &agents,
            &projects,
        );
        input.search = Some(&search);
        let outcome = compute_visible(input);

        assert_eq!(visible_workspace_keys(&outcome), vec!["parent", "child"]);
        assert!(outcome.ticket_tree[&SessionKey::new("parent")].context_only);
        assert!(!outcome.ticket_tree[&SessionKey::new("child")].context_only);
        assert_eq!(
            outcome.summaries["linear/ENG"].active, 1,
            "ancestor context is not counted as a search match"
        );
    }

    #[test]
    fn missing_and_cyclic_parents_never_hide_tickets() {
        let mut workspaces = HashMap::new();
        for workspace in [
            linear_ticket("missing", "ENG-1", Some("ENG-404"), 1),
            linear_ticket("cycle-a", "ENG-2", Some("ENG-3"), 2),
            linear_ticket("cycle-b", "ENG-3", Some("ENG-2"), 3),
        ] {
            workspaces.insert(SessionKey::from(&workspace.key), workspace);
        }
        let collapsed_repos = BTreeSet::new();
        let attention = lazybox_config::AttentionConfig::default();
        let agents = HashMap::new();
        let projects = BTreeMap::new();
        let outcome = compute_visible(inputs(
            &workspaces,
            &BTreeSet::new(),
            &collapsed_repos,
            &attention,
            &agents,
            &projects,
        ));
        let keys = visible_workspace_keys(&outcome);

        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&"missing"));
        assert!(keys.contains(&"cycle-a"));
        assert!(keys.contains(&"cycle-b"));
    }

    /// A ticket that has acquired a linked PR: the PR becomes the headline
    /// task (`primary_task`), but the ticket's `parent`/id still drive the
    /// hierarchy. Both parent and child carry a PR here, so this only passes
    /// when the forest addresses workspaces by every provider id they hold.
    #[test]
    fn tickets_with_linked_prs_keep_their_hierarchy() {
        let mut workspaces = HashMap::new();
        for workspace in [
            linear_ticket_with_pr("parent", "ENG-1", None, 10),
            linear_ticket_with_pr("child", "ENG-2", Some("ENG-1"), 1),
        ] {
            workspaces.insert(SessionKey::from(&workspace.key), workspace);
        }
        let collapsed_repos = BTreeSet::new();
        let attention = lazybox_config::AttentionConfig::default();
        let agents = HashMap::new();
        let projects = BTreeMap::new();
        let outcome = compute_visible(inputs(
            &workspaces,
            &BTreeSet::new(),
            &collapsed_repos,
            &attention,
            &agents,
            &projects,
        ));

        assert_eq!(visible_workspace_keys(&outcome), vec!["parent", "child"]);
        assert!(outcome.ticket_tree[&SessionKey::new("parent")].has_children);
        assert_eq!(outcome.ticket_tree[&SessionKey::new("parent")].depth, 0);
        assert_eq!(outcome.ticket_tree[&SessionKey::new("child")].depth, 1);
    }

    /// A collapsed ancestor must not swallow a descendant that the active
    /// search pulled in: the match would vanish with no indication. The
    /// ancestor is kept as context and force-expanded regardless of its
    /// stored collapse.
    #[test]
    fn collapsed_ancestor_never_hides_a_search_match() {
        let mut workspaces = HashMap::new();
        for workspace in [
            linear_ticket("parent", "ENG-1", None, 10),
            linear_ticket("child", "ENG-2", Some("ENG-1"), 1),
            linear_ticket("grandchild", "ENG-3", Some("ENG-2"), 2),
        ] {
            workspaces.insert(SessionKey::from(&workspace.key), workspace);
        }
        let collapsed_repos = BTreeSet::new();
        let attention = lazybox_config::AttentionConfig::default();
        let agents = HashMap::new();
        let projects = BTreeMap::new();
        let mut collapsed_tickets = HashSet::new();
        collapsed_tickets.insert(TaskId {
            source: "linear".into(),
            key: "ENG-1".into(),
        });
        let search = global_search("ENG-3");
        let subscribed = BTreeSet::new();
        let mut input = inputs(
            &workspaces,
            &subscribed,
            &collapsed_repos,
            &attention,
            &agents,
            &projects,
        );
        input.collapsed_tickets = &collapsed_tickets;
        input.search = Some(&search);
        let outcome = compute_visible(input);

        let keys = visible_workspace_keys(&outcome);
        assert!(
            keys.contains(&"grandchild"),
            "the search match must stay visible under a collapsed ancestor: {keys:?}"
        );
        assert!(
            !outcome.ticket_tree[&SessionKey::new("parent")].collapsed,
            "a context-only ancestor must not fold away the match it contextualizes"
        );
    }

    /// Workspaces in different repos: one header each, alphabetical
    /// repo order (BTreeMap), workspaces under each.
    #[test]
    fn multiple_repos_grouped_and_alphabetized() {
        let mut ws = HashMap::new();
        let a = workspace_with_task("ka", Some("owner/a"), 10);
        let b = workspace_with_task("kb", Some("owner/b"), 10);
        ws.insert(SessionKey::from(&a.key), a);
        ws.insert(SessionKey::from(&b.key), b);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        // header(a) + ws + header(b) + ws.
        assert_eq!(out.visible.len(), 4);
        if let VisibleRow::RepoHeader(name) = &out.visible[0] {
            assert_eq!(name, "owner/a");
        } else {
            panic!("expected RepoHeader, got {:?}", out.visible[0]);
        }
        if let VisibleRow::RepoHeader(name) = &out.visible[2] {
            assert_eq!(name, "owner/b");
        } else {
            panic!("expected RepoHeader, got {:?}", out.visible[2]);
        }
    }

    /// A pinned repo floats to the top ahead of the alphabetical
    /// order; unpinned repos keep their relative (alphabetical) order
    /// below it (#760).
    #[test]
    fn pinned_repo_floats_above_alphabetical_order() {
        let mut ws = HashMap::new();
        for (k, repo) in [("ka", "owner/a"), ("kb", "owner/b"), ("kc", "owner/c")] {
            let w = workspace_with_task(k, Some(repo), 10);
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let pins = vec!["owner/c".to_string()];
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.pinned_repos = &pins;
        let out = compute_visible(inp);
        let headers: Vec<&str> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::RepoHeader(name) => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, ["owner/c", "owner/a", "owner/b"]);
    }

    /// Multiple pins render first in pin order (not alphabetical), and
    /// a pin naming a repo with no rows this pass is skipped without
    /// disturbing the rest.
    #[test]
    fn multiple_pins_keep_pin_order_and_skip_absent() {
        let mut ws = HashMap::new();
        for (k, repo) in [("ka", "owner/a"), ("kb", "owner/b"), ("kc", "owner/c")] {
            let w = workspace_with_task(k, Some(repo), 10);
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        // Pin c then a; "owner/z" has no group and must be ignored.
        let pins = vec![
            "owner/c".to_string(),
            "owner/z".to_string(),
            "owner/a".to_string(),
        ];
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.pinned_repos = &pins;
        let out = compute_visible(inp);
        let headers: Vec<&str> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::RepoHeader(name) => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, ["owner/c", "owner/a", "owner/b"]);
    }

    /// Build a one-member-per-key snapshot in the given wave order.
    fn epic_snapshot(key: &str, members: &[&str]) -> lazybox_ipc::EpicSnapshot {
        lazybox_ipc::EpicSnapshot {
            key: key.to_string(),
            name: key.to_string(),
            members: members
                .iter()
                .enumerate()
                .map(|(i, m)| lazybox_ipc::EpicMember {
                    key: lazybox_core::WorkspaceKey::new(*m),
                    wave: i as u16,
                    status: lazybox_ipc::EpicMemberStatus::Ready,
                    blocked_by: Vec::new(),
                    external_blockers: Vec::new(),
                    blockers: Vec::new(),
                    blocked_reason: None,
                })
                .collect(),
            done: 0,
            total: members.len() as u32,
            ready: members.len() as u32,
            blocked: 0,
            asking: 0,
            failing: 0,
            blockers_needing_operator: 0,
            cycle: false,
            critical_path: Vec::new(),
            edges: Vec::new(),
            merge_order: Vec::new(),
            policies: lazybox_core::EpicPolicies::default(),
            computed_at: 0,
        }
    }

    /// An epic's members are lifted out of their repo groups into a
    /// cross-repo `EpicHeader` section, in the snapshot's wave order, and
    /// are not re-listed under their repos.
    #[test]
    fn epic_members_lift_into_a_cross_repo_section() {
        let mut ws = HashMap::new();
        for (k, repo) in [("ka", "owner/a"), ("kb", "owner/b"), ("kc", "owner/c")] {
            let w = workspace_with_task(k, Some(repo), 10);
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        // Wave order deliberately reverses the alphabetical repo order.
        let mut epics = BTreeMap::new();
        epics.insert("auth".to_string(), epic_snapshot("auth", &["kc", "ka"]));
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.epics = &epics;
        let out = compute_visible(inp);

        assert!(matches!(&out.visible[0], VisibleRow::EpicHeader(k) if k == "auth"));
        assert!(matches!(&out.visible[1], VisibleRow::Workspace(k) if k.as_str() == "kc"));
        assert!(matches!(&out.visible[2], VisibleRow::Workspace(k) if k.as_str() == "ka"));

        // Each member appears exactly once — lifted, not duplicated.
        for member in ["ka", "kc"] {
            let count = out
                .visible
                .iter()
                .filter(|r| matches!(r, VisibleRow::Workspace(k) if k.as_str() == member))
                .count();
            assert_eq!(count, 1, "{member} duplicated");
        }
        // The non-member still renders under its own repo header.
        assert!(
            out.visible
                .iter()
                .any(|r| matches!(r, VisibleRow::Workspace(k) if k.as_str() == "kb"))
        );
    }

    /// A collapsed epic keeps its header — the header line *is* the status
    /// — and folds its member rows away.
    #[test]
    fn collapsed_epic_keeps_its_header_and_hides_members() {
        let mut ws = HashMap::new();
        let w = workspace_with_task("ka", Some("owner/a"), 10);
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let mut epics = BTreeMap::new();
        epics.insert("auth".to_string(), epic_snapshot("auth", &["ka"]));
        let collapsed_epics: BTreeSet<String> = ["auth".to_string()].into_iter().collect();
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.epics = &epics;
        inp.collapsed_epics = &collapsed_epics;
        let out = compute_visible(inp);

        assert!(matches!(&out.visible[0], VisibleRow::EpicHeader(k) if k == "auth"));
        assert!(
            !out.visible
                .iter()
                .any(|r| matches!(r, VisibleRow::Workspace(k) if k.as_str() == "ka")),
            "collapsed epic still emitted its member"
        );
    }

    /// A starred member stays in `★ Focused` rather than moving into the
    /// epic section, and is never listed twice.
    #[test]
    fn starred_epic_member_stays_in_focused() {
        let mut ws = HashMap::new();
        for (k, repo) in [("ka", "owner/a"), ("kb", "owner/b")] {
            let w = workspace_with_task(k, Some(repo), 10);
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let mut epics = BTreeMap::new();
        epics.insert("auth".to_string(), epic_snapshot("auth", &["ka", "kb"]));
        let focus = vec![SessionKey::from("ka")];
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.epics = &epics;
        inp.focused_workspaces = &focus;
        let out = compute_visible(inp);

        assert!(matches!(out.visible[0], VisibleRow::FocusedHeader));
        assert!(matches!(&out.visible[1], VisibleRow::Workspace(k) if k.as_str() == "ka"));
        let ka_rows = out
            .visible
            .iter()
            .filter(|r| matches!(r, VisibleRow::Workspace(k) if k.as_str() == "ka"))
            .count();
        assert_eq!(ka_rows, 1);
        // The epic section still renders for its remaining member.
        let epic_at = out
            .visible
            .iter()
            .position(|r| matches!(r, VisibleRow::EpicHeader(k) if k == "auth"))
            .expect("epic header");
        assert!(
            matches!(&out.visible[epic_at + 1], VisibleRow::Workspace(k) if k.as_str() == "kb")
        );
    }

    /// A member two epics both claim lands under the first only, so the
    /// tree stays a tree.
    #[test]
    fn a_member_two_epics_claim_renders_once() {
        let mut ws = HashMap::new();
        let w = workspace_with_task("ka", Some("owner/a"), 10);
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let mut epics = BTreeMap::new();
        epics.insert("aaa".to_string(), epic_snapshot("aaa", &["ka"]));
        epics.insert("zzz".to_string(), epic_snapshot("zzz", &["ka"]));
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.epics = &epics;
        let out = compute_visible(inp);

        let ka_rows = out
            .visible
            .iter()
            .filter(|r| matches!(r, VisibleRow::Workspace(k) if k.as_str() == "ka"))
            .count();
        assert_eq!(ka_rows, 1);
        // The second epic has no rows left, so it emits no header at all.
        assert!(
            !out.visible
                .iter()
                .any(|r| matches!(r, VisibleRow::EpicHeader(k) if k == "zzz"))
        );
    }

    /// An epic whose members are all filtered out of this mailbox emits no
    /// header — an empty section is pure chrome.
    #[test]
    fn epic_with_no_visible_member_emits_no_header() {
        let mut ws = HashMap::new();
        let w = workspace_with_task("ka", Some("owner/a"), 10);
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let mut epics = BTreeMap::new();
        epics.insert("auth".to_string(), epic_snapshot("auth", &["gone"]));
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.epics = &epics;
        let out = compute_visible(inp);

        assert!(
            !out.visible
                .iter()
                .any(|r| matches!(r, VisibleRow::EpicHeader(_)))
        );
    }

    /// A starred workspace is lifted into the `★ Focused` section at the
    /// very top — a `FocusedHeader` first, the starred row under it — and
    /// is NOT re-listed under its repo group.
    #[test]
    fn focused_workspace_lifts_into_top_section() {
        let mut ws = HashMap::new();
        for (k, repo) in [("ka", "owner/a"), ("kb", "owner/b")] {
            let w = workspace_with_task(k, Some(repo), 10);
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let focus = vec![SessionKey::from("kb")];
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.focused_workspaces = &focus;
        let out = compute_visible(inp);

        // First two rows: the synthetic header then the starred row.
        assert!(matches!(out.visible[0], VisibleRow::FocusedHeader));
        assert!(matches!(&out.visible[1], VisibleRow::Workspace(k) if k.as_str() == "kb"));

        // `kb` appears exactly once (lifted, not duplicated); its repo
        // header still renders but with no workspace under it.
        let kb_rows = out
            .visible
            .iter()
            .filter(|r| matches!(r, VisibleRow::Workspace(k) if k.as_str() == "kb"))
            .count();
        assert_eq!(kb_rows, 1);
        // The non-focused `ka` stays in its repo group.
        let ka_after_a_header = out.visible.windows(2).any(|w| {
            matches!(&w[0], VisibleRow::RepoHeader(n) if n == "owner/a")
                && matches!(&w[1], VisibleRow::Workspace(k) if k.as_str() == "ka")
        });
        assert!(ka_after_a_header, "ka stays under owner/a");
    }

    #[test]
    fn hopper_follows_focused_and_stays_out_of_repo_groups() {
        let mut ws = HashMap::new();
        let focused = workspace_with_task("focused", Some("owner/a"), 10);
        ws.insert(SessionKey::from(&focused.key), focused);
        for (key, position) in [("later", 1), ("first", 0)] {
            let mut hopper = Workspace::empty(WorkspaceKey::new(key), "main", fixed_time());
            hopper.name = key.into();
            hopper.project_key = Some(ProjectKey::github("owner", "a"));
            hopper.hopper = Some(lazybox_core::HopperMeta {
                position,
                completed_at: None,
                canceled_at: None,
            });
            ws.insert(SessionKey::from(&hopper.key), hopper);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let focus = vec![SessionKey::from("focused")];
        let mut input = inputs(&ws, &sub, &col, &att, &asking, &projects);
        input.focused_workspaces = &focus;

        let out = compute_visible(input);
        assert!(matches!(out.visible[0], VisibleRow::FocusedHeader));
        assert!(matches!(out.visible[2], VisibleRow::HopperHeader));
        assert!(matches!(&out.visible[3], VisibleRow::Workspace(key) if key.as_str() == "first"));
        assert!(matches!(&out.visible[4], VisibleRow::Workspace(key) if key.as_str() == "later"));
        for hopper_key in ["first", "later"] {
            assert_eq!(
                out.visible
                    .iter()
                    .filter(|row| matches!(row, VisibleRow::Workspace(key) if key.as_str() == hopper_key))
                    .count(),
                1,
                "hopper rows are never duplicated under their assigned repo"
            );
        }
    }

    /// The Focused section renders its rows in focus (Vec) order, and a
    /// star naming a workspace not visible this pass is skipped — the
    /// per-workspace parallel of the pin's skip-absent behavior.
    #[test]
    fn focused_section_keeps_focus_order_and_skips_absent() {
        let mut ws = HashMap::new();
        for (k, repo) in [("ka", "owner/a"), ("kb", "owner/b"), ("kc", "owner/c")] {
            let w = workspace_with_task(k, Some(repo), 10);
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        // Focus kc then ka; "kz" has no workspace and must be skipped.
        let focus = vec![
            SessionKey::from("kc"),
            SessionKey::from("kz"),
            SessionKey::from("ka"),
        ];
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.focused_workspaces = &focus;
        let out = compute_visible(inp);

        // Rows under the FocusedHeader, up to the first RepoHeader.
        let focused: Vec<&str> = out
            .visible
            .iter()
            .skip_while(|r| !matches!(r, VisibleRow::FocusedHeader))
            .skip(1)
            .take_while(|r| matches!(r, VisibleRow::Workspace(_)))
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(focused, ["kc", "ka"]);
    }

    /// A star naming a workspace filtered out by the current mailbox is
    /// not surfaced — the Focused section respects the same filtered set
    /// the repo groups do (parity with the pin, which only reorders what
    /// the view already shows). With no visible starred rows, no
    /// `FocusedHeader` is emitted.
    #[test]
    fn focused_section_respects_mailbox_filter() {
        let mut ws = HashMap::new();
        let w = workspace_with_task("ka", Some("owner/a"), 10);
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let focus = vec![SessionKey::from("ka")];
        // Snoozed mailbox: the (non-snoozed) workspace isn't a member, so
        // even though it's starred it doesn't appear anywhere.
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.mailbox = Mailbox::Snoozed;
        inp.focused_workspaces = &focus;
        let out = compute_visible(inp);
        assert!(
            !out.visible
                .iter()
                .any(|r| matches!(r, VisibleRow::FocusedHeader)),
            "no focused header when no starred row is visible"
        );
    }

    /// `space_of`: explicit assignment wins, else the owner segment
    /// auto-seeds a Space, else the `Ungrouped` default.
    #[test]
    fn space_of_prefers_config_then_owner_then_ungrouped() {
        let spaces = vec![lazybox_config::SpaceConfig {
            name: "Obin".into(),
            sources: vec!["me/side-project".into()],
        }];
        // Explicit assignment overrides the owner auto-seed.
        assert_eq!(space_of("me/side-project", &spaces), "Obin");
        // Owner auto-seed for an unassigned GitHub label.
        assert_eq!(space_of("obin-ai/platform", &spaces), "obin-ai");
        // No owner boundary (Linear label / no-repo) → default Space.
        assert_eq!(space_of("Obin Eng", &spaces), UNGROUPED_SPACE);
        assert_eq!(space_of(NO_REPO, &spaces), UNGROUPED_SPACE);
    }

    /// `assign_source` moves a source between Spaces, creates a new
    /// Space at the end, reorders within a Space on re-assign, and
    /// unassigns on a blank target — the persisted-mutation half of the
    /// move/reorder acceptance criterion.
    #[test]
    fn assign_source_moves_creates_reorders_and_unassigns() {
        let mut spaces: Vec<lazybox_config::SpaceConfig> = Vec::new();

        // Create "Obin" with two sources.
        assign_source(&mut spaces, "obin-ai/platform", "Obin");
        assign_source(&mut spaces, "obin-ai/studio", "Obin");
        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].name, "Obin");
        assert_eq!(spaces[0].sources, ["obin-ai/platform", "obin-ai/studio"]);

        // A second Space is appended (its display order).
        assign_source(&mut spaces, "me/dotfiles", "Personal");
        assert_eq!(
            spaces.iter().map(|s| &s.name).collect::<Vec<_>>(),
            ["Obin", "Personal"]
        );

        // Re-assigning within Obin moves the source to the end.
        assign_source(&mut spaces, "obin-ai/platform", "Obin");
        assert_eq!(spaces[0].sources, ["obin-ai/studio", "obin-ai/platform"]);

        // Moving across Spaces removes it from the old one first.
        assign_source(&mut spaces, "obin-ai/studio", "Personal");
        assert_eq!(spaces[0].sources, ["obin-ai/platform"]);
        assert_eq!(spaces[1].sources, ["me/dotfiles", "obin-ai/studio"]);

        // A blank target unassigns (back to owner auto-seed).
        assign_source(&mut spaces, "obin-ai/platform", "");
        assert!(spaces[0].sources.is_empty());
        assert_eq!(space_of("obin-ai/platform", &spaces), "obin-ai");
    }

    /// #1211: moving a Space materializes the full rendered order into
    /// config (auto-seeds get empty-source entries, existing entries
    /// keep their sources), unrendered entries survive at the tail,
    /// and end-of-list moves clamp instead of erroring.
    /// #1211: rename keeps display position, merges into an existing
    /// target, materializes an auto-seed's rendered sources, and
    /// refuses blank/unchanged names.
    #[test]
    fn rename_space_moves_claims_and_rendered_sources() {
        let mut spaces = vec![
            lazybox_config::SpaceConfig {
                name: "Work".into(),
                sources: vec!["o/r".into()],
            },
            lazybox_config::SpaceConfig {
                name: "Later".into(),
                sources: vec!["x/y".into()],
            },
        ];

        // Plain rename: claims move, position kept.
        assert!(rename_space(&mut spaces, "Work", "Deep Work", &[]));
        assert_eq!(spaces[0].name, "Deep Work");
        assert_eq!(spaces[0].sources, ["o/r"]);

        // Rename onto an existing Space merges (no dupes).
        assert!(rename_space(
            &mut spaces,
            "Deep Work",
            "Later",
            &["o/r".into()]
        ));
        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].name, "Later");
        assert_eq!(spaces[0].sources, ["x/y", "o/r"]);

        // Auto-seed (no entry): rendered sources materialize the claim.
        assert!(rename_space(
            &mut spaces,
            "obin-ai",
            "Platform",
            &["obin-ai/a".into(), "obin-ai/b".into()],
        ));
        let platform = spaces
            .iter()
            .find(|s| s.name == "Platform")
            .expect("created");
        assert_eq!(platform.sources, ["obin-ai/a", "obin-ai/b"]);

        // Blank / unchanged: refuse, mutate nothing.
        let before = spaces.clone();
        assert!(!rename_space(&mut spaces, "Later", "  ", &[]));
        assert!(!rename_space(&mut spaces, "Later", "Later", &[]));
        assert_eq!(spaces, before);
    }

    #[test]
    fn move_space_materializes_rendered_order_and_clamps() {
        let mut spaces = vec![
            lazybox_config::SpaceConfig {
                name: "Work".into(),
                sources: vec!["o/r".into()],
            },
            lazybox_config::SpaceConfig {
                name: "Hidden".into(),
                sources: vec!["h/h".into()],
            },
        ];
        // On screen: Work, then two auto-seeds; "Hidden" is filtered out.
        let rendered: Vec<String> = vec!["Work".into(), "acme".into(), "obin-ai".into()];

        assert!(move_space(&mut spaces, &rendered, "obin-ai", MoveDir::Up));
        assert_eq!(
            spaces.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["Work", "obin-ai", "acme", "Hidden"],
        );
        assert_eq!(spaces[0].sources, ["o/r"], "existing sources survive");
        assert!(spaces[1].sources.is_empty(), "auto-seed materialized empty");
        assert_eq!(spaces[3].sources, ["h/h"], "unrendered entry keeps claim");

        // Clamped: already at the top of the rendered order.
        assert!(move_space(&mut spaces, &rendered, "Work", MoveDir::Up));
        assert_eq!(spaces[0].name, "Work");

        assert!(move_space(&mut spaces, &rendered, "Work", MoveDir::Bottom));
        assert_eq!(
            spaces.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["acme", "obin-ai", "Work", "Hidden"],
        );

        assert!(
            !move_space(&mut spaces, &rendered, "Nope", MoveDir::Up),
            "unrendered name refuses instead of corrupting order",
        );
    }

    /// #1211: moving a repo inside a Space materializes the rendered
    /// repo order into `sources` (creating the entry for an implicit
    /// auto-seed Space), keeps unrendered claims, and resolution is
    /// unchanged — every repo still lands in the same Space.
    #[test]
    fn move_source_in_space_reorders_and_keeps_claims() {
        let mut spaces: Vec<lazybox_config::SpaceConfig> = Vec::new();
        let rendered: Vec<String> =
            vec!["obin-ai/a".into(), "obin-ai/b".into(), "obin-ai/c".into()];

        // Implicit owner Space: first move materializes it.
        assert!(move_source_in_space(
            &mut spaces,
            "obin-ai",
            &rendered,
            "obin-ai/c",
            MoveDir::Top,
        ));
        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].name, "obin-ai");
        assert_eq!(spaces[0].sources, ["obin-ai/c", "obin-ai/a", "obin-ai/b"]);
        assert_eq!(
            space_of("obin-ai/c", &spaces),
            "obin-ai",
            "resolution unchanged"
        );

        // An unrendered claimed source survives a later move. The move
        // is computed over the *rendered* order passed in ([a, b, c]),
        // so `a` moving down lands as [b, a, c] + the unrendered `z`.
        spaces[0].sources.push("obin-ai/z".into());
        assert!(move_source_in_space(
            &mut spaces,
            "obin-ai",
            &rendered,
            "obin-ai/a",
            MoveDir::Down,
        ));
        assert_eq!(
            spaces[0].sources,
            ["obin-ai/b", "obin-ai/a", "obin-ai/c", "obin-ai/z"],
        );

        assert!(
            !move_source_in_space(&mut spaces, "obin-ai", &rendered, "x/x", MoveDir::Up),
            "unrendered repo refuses",
        );
    }

    /// A single owner yields a lone Space, so the tier stays suppressed
    /// and the flat repo shape is preserved byte-for-byte.
    #[test]
    fn single_space_suppresses_the_tier() {
        let mut ws = HashMap::new();
        for (k, repo) in [("ka", "owner/a"), ("kb", "owner/b")] {
            let w = workspace_with_task(k, Some(repo), 10);
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert!(
            !out.visible
                .iter()
                .any(|r| matches!(r, VisibleRow::SpaceHeader(_))),
            "one owner = one Space = no tier"
        );

        // …but an EXPLICIT `ui.spaces` entry un-suppresses it even at
        // one distinct Space (#scale, proposal F): a Space the user
        // created must be visible, or move-to-Space looks broken.
        let explicit = vec![lazybox_config::SpaceConfig {
            name: "mine".into(),
            sources: vec!["owner/a".into(), "owner/b".into()],
        }];
        let mut input = inputs(&ws, &sub, &col, &att, &asking, &projects);
        input.spaces = &explicit;
        let out = compute_visible(input);
        assert!(
            out.visible
                .iter()
                .any(|r| matches!(r, VisibleRow::SpaceHeader(name) if name == "mine")),
            "an explicit Space renders its header even when it is the only one"
        );
    }

    /// Two owners auto-seed two Spaces, so the tier turns on: a
    /// `SpaceHeader` leads each owner's repo group, Spaces alphabetical.
    #[test]
    fn two_owners_auto_seed_space_tier() {
        let mut ws = HashMap::new();
        for (k, repo) in [("ka", "zeta/a"), ("kb", "alpha/b")] {
            let w = workspace_with_task(k, Some(repo), 10);
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        let rows: Vec<String> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::SpaceHeader(n) => Some(format!("S:{n}")),
                VisibleRow::RepoHeader(n) => Some(format!("R:{n}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            rows,
            ["S:alpha", "R:alpha/b", "S:zeta", "R:zeta/a"],
            "auto-seeded Spaces alphabetical, each above its repo"
        );
    }

    /// A user-defined Space unifies repos across owners under one
    /// bucket, in the config source order, and leads config Spaces
    /// before auto-seeded ones.
    #[test]
    fn config_space_unifies_owners_in_source_order() {
        let mut ws = HashMap::new();
        for (k, repo) in [
            ("ka", "obin-ai/platform"),
            ("kb", "obin-ai/studio"),
            ("kc", "me/dotfiles"),
        ] {
            let w = workspace_with_task(k, Some(repo), 10);
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let spaces = vec![lazybox_config::SpaceConfig {
            name: "Obin".into(),
            // studio before platform — source order overrides alpha.
            sources: vec!["obin-ai/studio".into(), "obin-ai/platform".into()],
        }];
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.spaces = &spaces;
        let out = compute_visible(i);
        let rows: Vec<String> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::SpaceHeader(n) => Some(format!("S:{n}")),
                VisibleRow::RepoHeader(n) => Some(format!("R:{n}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            rows,
            [
                "S:Obin",
                "R:obin-ai/studio",
                "R:obin-ai/platform",
                "S:me",
                "R:me/dotfiles",
            ],
            "config Space leads (source order), then the auto-seeded owner Space"
        );
    }

    /// Collapsing a Space keeps its `SpaceHeader` but drops every repo
    /// header and row beneath it.
    #[test]
    fn collapsed_space_hides_its_repos() {
        let mut ws = HashMap::new();
        for (k, repo) in [("ka", "alpha/a"), ("kb", "zeta/z")] {
            let w = workspace_with_task(k, Some(repo), 10);
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let mut collapsed_spaces = BTreeSet::new();
        collapsed_spaces.insert("alpha".to_string());
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.collapsed_spaces = &collapsed_spaces;
        let out = compute_visible(i);
        let rows: Vec<String> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::SpaceHeader(n) => Some(format!("S:{n}")),
                VisibleRow::RepoHeader(n) => Some(format!("R:{n}")),
                VisibleRow::Workspace(k) => Some(format!("W:{}", k.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            rows,
            ["S:alpha", "S:zeta", "R:zeta/z", "W:kb"],
            "collapsed Space shows only its header"
        );
        // Summary for the collapsed Space's repo isn't computed (its
        // header never renders), but the visible Space's repo is.
        assert!(out.summaries.contains_key("zeta/z"));
    }

    /// Collapsed repo: header only, workspace rows under it are
    /// suppressed.
    #[test]
    fn collapsed_repo_emits_header_only() {
        let mut ws = HashMap::new();
        let w = workspace_with_task("k1", Some("owner/r"), 10);
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let mut col = BTreeSet::new();
        col.insert("owner/r".to_string());
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert_eq!(out.visible.len(), 1);
        assert!(matches!(out.visible[0], VisibleRow::RepoHeader(_)));
        // Summary still counts the active workspace.
        assert_eq!(out.summaries.get("owner/r").unwrap().active, 1);
    }

    /// Workspace with project_key set lands under its project's
    /// display-name header, NOT the legacy `task.repo`-derived one.
    /// Drives the post-Stage-4 grouping invariant.
    #[test]
    fn workspaces_group_under_project_display_name() {
        let mut ws = HashMap::new();
        let mut w = workspace_with_task("k1", Some("owner/r"), 10);
        let pk = ProjectKey::github("acme", "tool");
        w.project_key = Some(pk.clone());
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let mut projects = BTreeMap::new();
        projects.insert(
            pk.clone(),
            Project::new(pk, "acme/tool", chrono::Utc::now()),
        );
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert!(
            out.summaries.contains_key("acme/tool"),
            "header should be the project's display name"
        );
        assert!(!out.summaries.contains_key("owner/r"));
    }

    #[test]
    fn github_workspace_repairs_lossy_project_header_from_task_repo() {
        let mut w = workspace_with_task("k1", Some("codefly-dev/warden-platform"), 10);
        let pk = ProjectKey::github("codefly-dev", "warden-platform");
        w.project_key = Some(pk.clone());
        let mut ws = HashMap::new();
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let mut projects = BTreeMap::new();
        projects.insert(
            pk.clone(),
            Project::new(pk, "codefly/dev-warden-platform", chrono::Utc::now()),
        );

        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));

        assert_eq!(out.visible.len(), 2);
        assert!(matches!(
            &out.visible[0],
            VisibleRow::RepoHeader(name) if name == "codefly-dev/warden-platform"
        ));
        assert!(!out.summaries.contains_key("codefly/dev-warden-platform"));
    }

    /// Project with no workspace yields a header in Inbox
    /// (so the user can see "I'm subscribed but nothing's in flight").
    #[test]
    fn empty_project_emits_header_in_inbox() {
        let ws = HashMap::new();
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let mut projects = BTreeMap::new();
        let pk = ProjectKey::github("owner", "empty");
        projects.insert(
            pk.clone(),
            Project::new(pk, "owner/empty", chrono::Utc::now()),
        );
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert_eq!(out.visible.len(), 1);
        assert!(matches!(&out.visible[0], VisibleRow::RepoHeader(name) if name == "owner/empty"));
    }

    /// With a filter active, an empty subscribed project does NOT emit
    /// a header — a narrowed view shouldn't list every repo as an
    /// empty header burying the matches (issue #443 review).
    #[test]
    fn active_filter_suppresses_empty_project_headers() {
        let ws = HashMap::new();
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let mut projects = BTreeMap::new();
        let pk = ProjectKey::github("owner", "empty");
        projects.insert(
            pk.clone(),
            Project::new(pk, "owner/empty", chrono::Utc::now()),
        );
        let mut filters = FilterSet::default();
        filters.toggle(Filter::Author);
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.filters = &filters;
        let out = compute_visible(i);
        assert!(
            out.visible.is_empty(),
            "empty project header suppressed under an active filter"
        );
    }

    /// A project whose stored `name` is the raw key (the legacy
    /// self-add fallback) still renders as `owner/repo`, and its
    /// workspaces group under that same prettified header.
    #[test]
    fn raw_key_project_renders_as_owner_slash_repo() {
        let pk = ProjectKey::github("AntoineToussaint", "lazybox");
        let mut w = workspace_with_task("k1", None, 10);
        w.project_key = Some(pk.clone());
        let mut ws = HashMap::new();
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let mut projects = BTreeMap::new();
        // Stored with `name == key` — the bug this fix repairs.
        projects.insert(
            pk.clone(),
            Project::new(pk.clone(), pk.as_str(), chrono::Utc::now()),
        );
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert!(out.summaries.contains_key("AntoineToussaint/lazybox"));
        assert!(
            !out.summaries
                .contains_key("github-AntoineToussaint-lazybox")
        );
    }

    /// Same setup, but Inactive mailbox: the empty project header
    /// is NOT shown (alternate view, not a subscription).
    #[test]
    fn empty_project_skipped_in_inactive() {
        let ws = HashMap::new();
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let mut projects = BTreeMap::new();
        let pk = ProjectKey::github("owner", "empty");
        projects.insert(
            pk.clone(),
            Project::new(pk, "owner/empty", chrono::Utc::now()),
        );
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.mailbox = Mailbox::Inactive;
        let out = compute_visible(i);
        assert!(out.visible.is_empty());
    }

    /// Two workspaces in same repo: sorted by updated_at desc.
    #[test]
    fn same_repo_workspaces_sorted_by_updated_at_desc() {
        let mut ws = HashMap::new();
        // `older` was updated 60 min ago, `newer` was updated 10 min ago.
        let older = workspace_with_task("k_older", Some("owner/r"), 60);
        let newer = workspace_with_task("k_newer", Some("owner/r"), 10);
        ws.insert(SessionKey::from(&older.key), older);
        ws.insert(SessionKey::from(&newer.key), newer);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        // [header, newer, older].
        let keys: Vec<&str> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["k_newer", "k_older"]);
    }

    /// A workspace with no primary task (no .repo set) lands under
    /// `(no repo)`.
    #[test]
    fn workspace_with_no_repo_grouped_under_no_repo() {
        let mut ws = HashMap::new();
        let w = workspace_with_task("k1", None, 10);
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert!(out.summaries.contains_key("(no repo)"));
    }

    /// Active count in the summary matches the number of visible
    /// workspaces under the repo, regardless of collapse state.
    #[test]
    fn summary_active_counts_all_workspaces_even_when_collapsed() {
        let mut ws = HashMap::new();
        for i in 0..3 {
            let mut w = workspace_with_task(&format!("k{i}"), Some("owner/r"), 10 + i);
            w.key = WorkspaceKey(format!("k{i}"));
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let mut col = BTreeSet::new();
        col.insert("owner/r".to_string());
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert_eq!(out.summaries.get("owner/r").unwrap().active, 3);
    }

    /// Build a workspace in `repo` with the given issue/PR `num` and
    /// display title — enough to exercise search matching.
    fn titled(key: &str, repo: &str, num: u64, title: &str) -> Workspace {
        let mut w = workspace_with_task(key, Some(repo), 10);
        w.key = WorkspaceKey(key.into());
        // `workspace_with_task` seeds a GitHub issue (no PR slot), so
        // the primary task lives in `gh_issues`.
        if let Some(t) = w.gh_issues.get_mut(0) {
            t.id.key = format!("{repo}#{num}");
            t.title = title.into();
        }
        w.name = title.into();
        w
    }

    fn search(scope: &str, query: &str) -> SearchState {
        SearchState {
            scope: Some(scope.into()),
            query: query.into(),
            editing: true,
        }
    }

    fn global_search(query: &str) -> SearchState {
        SearchState {
            scope: None,
            query: query.into(),
            editing: true,
        }
    }

    /// Query keeps fuzzy title matches in the scoped project, hides
    /// the rest.
    #[test]
    fn search_filters_scoped_project_by_title() {
        let mut ws = HashMap::new();
        for w in [
            titled("k1", "owner/r", 1, "Add search bar"),
            titled("k2", "owner/r", 2, "Fix flaky test"),
        ] {
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let s = search("owner/r", "search");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&s);
        let out = compute_visible(i);
        let keys: Vec<&str> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["k1"]);
        assert_eq!(out.summaries.get("owner/r").unwrap().active, 1);
    }

    /// A bare number matches the issue/PR number (with or without `#`).
    #[test]
    fn search_matches_on_number() {
        let mut ws = HashMap::new();
        for w in [
            titled("k1", "owner/r", 100, "Alpha"),
            titled("k2", "owner/r", 7, "Beta"),
        ] {
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let s = search("owner/r", "#100");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&s);
        let out = compute_visible(i);
        let keys: Vec<&str> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["k1"]);
    }

    /// The search only filters its scoped project — workspaces in
    /// other projects stay fully visible.
    #[test]
    fn search_leaves_other_projects_untouched() {
        let mut ws = HashMap::new();
        for w in [
            titled("k1", "owner/a", 1, "Add search"),
            titled("k2", "owner/a", 2, "Unrelated"),
            titled("k3", "owner/b", 3, "Unrelated"),
        ] {
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let s = search("owner/a", "search");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&s);
        let out = compute_visible(i);
        // owner/a is filtered to k1; owner/b keeps its row regardless.
        assert_eq!(out.summaries.get("owner/a").unwrap().active, 1);
        assert_eq!(out.summaries.get("owner/b").unwrap().active, 1);
    }

    /// `search_scope_covers` is the single predicate the visible-row filter
    /// and the sidebar's match-highlight both read, so it must classify
    /// scope exactly the way the filter keeps rows: a scoped search covers
    /// only its own repo (even when an out-of-scope row's title contains
    /// the query text), a global search covers every repo, and no / empty
    /// search covers nothing (#1099).
    #[test]
    fn search_scope_covers_matches_the_filter_scope() {
        let in_scope = titled("k1", "owner/a", 1, "Add search bar");
        let out_of_scope = titled("k2", "owner/b", 2, "search everywhere");
        let projects = BTreeMap::new();
        let ws = HashMap::new();

        let scoped = search("owner/a", "search");
        assert!(search_scope_covers(
            Some(&scoped),
            &in_scope,
            &projects,
            &ws
        ));
        assert!(
            !search_scope_covers(Some(&scoped), &out_of_scope, &projects, &ws),
            "an out-of-scope row is not covered even though its title contains the term",
        );

        let global = global_search("search");
        assert!(search_scope_covers(
            Some(&global),
            &in_scope,
            &projects,
            &ws
        ));
        assert!(search_scope_covers(
            Some(&global),
            &out_of_scope,
            &projects,
            &ws
        ));

        assert!(!search_scope_covers(None, &in_scope, &projects, &ws));
        let empty = search("owner/a", "");
        assert!(!search_scope_covers(
            Some(&empty),
            &in_scope,
            &projects,
            &ws
        ));

        // Emptiness is judged on the normalized query, so a query that
        // reduces to nothing — whitespace, or a lone `#` — covers nothing,
        // matching what the highlight shows (which normalizes the same way).
        // This keeps "covered" and "highlighted" in lockstep (#1099).
        let whitespace = global_search("   ");
        assert!(
            !search_scope_covers(Some(&whitespace), &in_scope, &projects, &ws),
            "a whitespace-only query covers nothing",
        );
        let hash_only = global_search("#");
        assert!(
            !search_scope_covers(Some(&hash_only), &in_scope, &projects, &ws),
            "a `#`-only query covers nothing",
        );
    }

    /// A global search (`scope: None`) filters EVERY project at once —
    /// a matching row survives in each repo, non-matching rows drop.
    #[test]
    fn global_search_filters_all_projects() {
        let mut ws = HashMap::new();
        for w in [
            titled("k1", "owner/a", 1, "Add search"),
            titled("k2", "owner/a", 2, "Unrelated"),
            titled("k3", "owner/b", 3, "Search elsewhere"),
            titled("k4", "owner/b", 4, "Unrelated"),
        ] {
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let s = global_search("search");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&s);
        let out = compute_visible(i);
        let keys: Vec<&str> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["k1", "k3"]);
        assert_eq!(out.summaries.get("owner/a").unwrap().active, 1);
        assert_eq!(out.summaries.get("owner/b").unwrap().active, 1);
    }

    /// The killer case: a bare PR/issue number typed into the global
    /// box surfaces the match no matter which repo it lives in.
    #[test]
    fn global_search_jumps_to_number_across_repos() {
        let mut ws = HashMap::new();
        for w in [
            titled("k1", "owner/a", 100, "Alpha"),
            titled("k2", "owner/a", 7, "Beta"),
            titled("k3", "owner/b", 720, "Gamma"),
        ] {
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let s = global_search("#720");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&s);
        let out = compute_visible(i);
        let keys: Vec<&str> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["k3"]);
    }

    /// A global search suppresses empty subscribed-project headers (the
    /// desktop's global `/`), while a scoped search keeps them (the
    /// TUI's `/`).
    #[test]
    fn global_search_suppresses_empty_project_headers_scoped_keeps_them() {
        let ws = HashMap::new();
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let mut projects = BTreeMap::new();
        let pk = ProjectKey::github("owner", "empty");
        projects.insert(
            pk.clone(),
            Project::new(pk, "owner/empty", chrono::Utc::now()),
        );

        // Global (scope None): the empty repo header is gone.
        let global = global_search("anything");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&global);
        assert!(compute_visible(i).visible.is_empty());

        // Scoped (named repo): unrelated empty projects still get a header.
        let scoped = search("some/other", "anything");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&scoped);
        assert!(compute_visible(i).summaries.contains_key("owner/empty"));
    }

    /// An empty query is a no-op even when search state is present.
    #[test]
    fn empty_query_shows_full_tree() {
        let mut ws = HashMap::new();
        for w in [
            titled("k1", "owner/r", 1, "Alpha"),
            titled("k2", "owner/r", 2, "Beta"),
        ] {
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let s = search("owner/r", "");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&s);
        let out = compute_visible(i);
        assert_eq!(out.summaries.get("owner/r").unwrap().active, 2);
    }

    #[test]
    fn search_matches_is_case_insensitive_subsequence() {
        let w = titled("k1", "owner/r", 1, "Add Search Filter Bar");
        assert!(search_matches("sfb", &w)); // subsequence across words
        assert!(search_matches("FILTER", &w)); // case-insensitive
        assert!(!search_matches("zzz", &w));
    }

    #[test]
    fn search_matches_repo_labels_and_people() {
        let mut w = titled("k1", "octo/widget", 1, "Unrelated title");
        if let Some(t) = w.gh_issues.get_mut(0) {
            t.labels = vec![lazybox_core::Label {
                name: "bug".into(),
                color: String::new(),
            }];
            t.reviewers = vec!["alice".into()];
            t.assignees = vec!["bob".into()];
        }
        assert!(search_matches("widget", &w), "repo substring");
        assert!(search_matches("bug", &w), "label substring");
        assert!(search_matches("alice", &w), "reviewer substring");
        assert!(search_matches("bob", &w), "assignee substring");
        assert!(!search_matches("nobody", &w));
    }

    #[test]
    fn pr_number_extracts_trailing_int() {
        let w = titled("k1", "owner/r", 1234, "x");
        assert_eq!(pr_number(w.primary_task().unwrap()), Some(1234));
    }

    #[test]
    fn pr_number_returns_none_when_no_hash() {
        let mut w = workspace_with_task("k1", Some("owner/r"), 10);
        if let Some(t) = w.gh_issues.get_mut(0) {
            t.id.key = "plain-key".into();
        }
        assert_eq!(pr_number(w.primary_task().unwrap()), None);
    }
    /// The snoozed lens (#scale): `Filter::Snoozed` in the active set
    /// widens Inbox membership to admit currently-snoozed rows — and
    /// the State axis then selects exactly them. Without the filter,
    /// snooze still always wins (row hidden from Inbox). An EXPIRED
    /// snooze never matches the lens.
    #[test]
    fn snoozed_lens_shows_snoozed_rows_in_the_inbox() {
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();

        let mut snoozed = workspace_with_task("snoozed", Some("repo"), 5);
        snoozed.snoozed_until = Some(fixed_time() + Duration::hours(4));
        let awake = workspace_with_task("awake", Some("repo"), 3);
        let mut expired = workspace_with_task("expired", Some("repo"), 8);
        expired.snoozed_until = Some(fixed_time() - Duration::hours(1));
        let ws: HashMap<SessionKey, Workspace> = [
            (SessionKey::from("snoozed"), snoozed),
            (SessionKey::from("awake"), awake),
            (SessionKey::from("expired"), expired),
        ]
        .into();

        let keys = |out: &ComputeOutcome| -> Vec<String> {
            out.visible
                .iter()
                .filter_map(|r| match r {
                    VisibleRow::Workspace(k) => Some(k.to_string()),
                    _ => None,
                })
                .collect()
        };

        // Default Inbox: snooze always wins; expired snooze is awake.
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        let visible = keys(&out);
        assert!(!visible.contains(&"snoozed".to_string()));
        assert!(visible.contains(&"awake".to_string()));
        assert!(visible.contains(&"expired".to_string()));

        // Snoozed lens: exactly the currently-snoozed rows.
        let mut lens = filter::FilterSet::new();
        lens.toggle(filter::Filter::Snoozed);
        let mut input = inputs(&ws, &sub, &col, &att, &asking, &projects);
        input.filters = &lens;
        let out = compute_visible(input);
        let visible = keys(&out);
        assert!(visible.contains(&"snoozed".to_string()));
        assert!(
            !visible.contains(&"awake".to_string()),
            "the State axis narrows to snoozed rows"
        );
        assert!(
            !visible.contains(&"expired".to_string()),
            "an expired snooze must read as awake, not match the lens"
        );
    }

    /// #1744: the repo summary counts what the group is made of — kinds
    /// by the row's type-glyph precedence (a folded issue+PR row is a
    /// PR) and roles by the primary task's role letter — so the header
    /// can read `2⇄ 1○ 1◆ · 2A 1R 1@`. Mentioned rows and context-only
    /// rows count toward neither axis.
    #[test]
    fn repo_summary_breaks_down_kind_and_role() {
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();

        let mut pr_authored = workspace_with_task("pr-a", Some("repo"), 1);
        let issue = pr_authored.gh_issues.remove(0);
        let mut pr = issue.clone();
        pr.kind = Some(lazybox_core::TaskKind::Pr);
        pr.id.key = "owner/repo#100".into();
        pr_authored.pr = Some(pr);
        // The folded issue stays on the row: still one PR, not an issue.
        pr_authored.gh_issues.push(issue);

        let mut pr_reviewing = workspace_with_task("pr-r", Some("repo"), 2);
        let mut pr = pr_reviewing.gh_issues.remove(0);
        pr.kind = Some(lazybox_core::TaskKind::Pr);
        pr.role = TaskRole::Reviewer;
        pr_reviewing.pr = Some(pr);

        let mut issue_assigned = workspace_with_task("issue", Some("repo"), 3);
        issue_assigned.gh_issues[0].role = TaskRole::Assignee;

        let mut ticket = workspace_with_task("ticket", Some("repo"), 4);
        let mut linear = ticket.gh_issues.remove(0);
        linear.id.source = "linear".into();
        linear.role = TaskRole::Mentioned;
        ticket.linear_issues.push(linear);

        let ws: HashMap<SessionKey, Workspace> =
            [pr_authored, pr_reviewing, issue_assigned, ticket]
                .into_iter()
                .map(|w| (SessionKey::from(&w.key), w))
                .collect();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        let summary = &out.summaries["repo"];
        assert_eq!(summary.active, 4);
        assert_eq!((summary.prs, summary.issues, summary.tickets), (2, 1, 1));
        assert_eq!(
            (summary.authored, summary.reviewing, summary.assigned),
            (1, 1, 1),
            "a mentioned row counts toward no role"
        );
    }

    /// Source-attention ladder (#scale, proposal A): a Muted source
    /// sinks below live groups (overriding alphabetical order), its
    /// rows fold behind the collapsed residue header with only
    /// punch-through rows surfacing, its summary carries the level +
    /// punch-only attention count — and a time-boxed source snooze
    /// reads as muted with its wake instant exposed. Space-level
    /// entries are inherited by member sources.
    #[test]
    fn muted_source_sinks_folds_and_punches_through() {
        use lazybox_config::{SourceAttention, SourceAttentionLevel};
        let sub = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();

        // "aaa" sorts before "bbb" alphabetically — the muted sink must
        // override that. One plain row + one punch-through (review
        // requested of the viewer) row in the muted repo.
        let plain = workspace_with_task("plain", Some("aaa"), 5);
        let mut punch = workspace_with_task("punch", Some("aaa"), 3);
        {
            let t = punch.gh_issues.first_mut().expect("task");
            t.role = TaskRole::Reviewer;
            t.review = ReviewStatus::Pending;
        }
        let live = workspace_with_task("live", Some("bbb"), 1);
        let ws: HashMap<SessionKey, Workspace> = [
            (SessionKey::from("plain"), plain),
            (SessionKey::from("punch"), punch),
            (SessionKey::from("live"), live),
        ]
        .into();

        let mut map: BTreeMap<String, SourceAttention> = BTreeMap::new();
        map.insert(
            "aaa".into(),
            SourceAttention {
                level: SourceAttentionLevel::Muted,
                snoozed_until: None,
            },
        );
        // Muting auto-collapses (the sidebar writer does this); mirror it.
        let collapsed: BTreeSet<String> = ["aaa".to_string()].into();

        let mut input = inputs(&ws, &sub, &collapsed, &att, &asking, &projects);
        input.source_attention = &map;
        let out = compute_visible(input);

        let header_pos = |name: &str| {
            out.visible
                .iter()
                .position(|r| matches!(r, VisibleRow::RepoHeader(n) if n == name))
                .unwrap_or_else(|| panic!("{name} header missing"))
        };
        assert!(
            header_pos("bbb") < header_pos("aaa"),
            "the muted group sinks below the live one despite sorting first"
        );
        let row_visible = |key: &str| {
            out.visible
                .iter()
                .any(|r| matches!(r, VisibleRow::Workspace(k) if k.as_str() == key))
        };
        assert!(
            !row_visible("plain"),
            "a collapsed muted group folds its plain rows"
        );
        assert!(
            row_visible("punch"),
            "a review-requested-of-you row punches through the fold"
        );
        let summary = &out.summaries["aaa"];
        assert_eq!(summary.source_attention.as_deref(), Some("muted"));
        assert_eq!(summary.active, 2);
        assert_eq!(
            summary.attention, 1,
            "only the punch-through row counts toward the muted group's badge"
        );
        assert!(
            out.summaries["bbb"].source_attention.is_none(),
            "live sources carry no level chip"
        );

        // A time-boxed source snooze reads as muted and exposes its
        // wake instant; a Space-level entry covers member sources.
        let mut map2: BTreeMap<String, SourceAttention> = BTreeMap::new();
        map2.insert(
            "bbb".into(),
            SourceAttention {
                level: SourceAttentionLevel::Live,
                snoozed_until: Some(fixed_time() + Duration::hours(6)),
            },
        );
        map2.insert(
            "space:Ungrouped".into(),
            SourceAttention {
                level: SourceAttentionLevel::Quiet,
                snoozed_until: None,
            },
        );
        let empty_collapsed = BTreeSet::new();
        let mut input = inputs(&ws, &sub, &empty_collapsed, &att, &asking, &projects);
        input.source_attention = &map2;
        let out = compute_visible(input);
        let bbb = &out.summaries["bbb"];
        assert_eq!(bbb.source_attention.as_deref(), Some("muted"));
        assert_eq!(
            bbb.source_snooze_until_epoch_ms,
            Some((fixed_time() + Duration::hours(6)).timestamp_millis()),
        );
        assert_eq!(
            out.summaries["aaa"].source_attention.as_deref(),
            Some("quiet"),
            "a source without its own entry inherits its Space's level"
        );
        assert!(
            row_visible("plain")
                || out
                    .visible
                    .iter()
                    .any(|r| matches!(r, VisibleRow::Workspace(k) if k.as_str() == "plain"),),
            "quiet sources keep their rows visible"
        );
    }

    /// Search-qualifier grammar (#scale, C2): field-qualified terms,
    /// the `@login` involves-union, `is:` kinds, `-` negation, and —
    /// critically — a qualifier-free query behaving exactly like the
    /// legacy single-blob search.
    #[test]
    fn search_qualifiers_filter_and_negate() {
        let mut ws = workspace_with_task("q", Some("acme/api"), 5);
        {
            let t = ws.gh_issues.first_mut().expect("task");
            t.title = "Fix login flow".into();
            t.author = "Alice".into();
            t.reviewers = vec!["bob".into()];
            t.assignees = vec!["carol".into()];
            t.labels = vec![lazybox_core::Label::new("bug")];
        }

        // Legacy blob behavior is preserved verbatim.
        assert!(search_matches("login", &ws));
        assert!(search_matches("bug", &ws));
        assert!(!search_matches("nomatch", &ws));

        // Field qualifiers, case-insensitive.
        assert!(search_matches("author:alice", &ws));
        assert!(!search_matches("author:bob", &ws));
        assert!(search_matches("reviewer:bob", &ws));
        assert!(search_matches("assignee:carol", &ws));
        assert!(search_matches("repo:acme", &ws));
        assert!(search_matches("label:bug", &ws));
        assert!(search_matches("is:issue", &ws));
        assert!(!search_matches("is:pr", &ws));

        // `@login` is the involves role-union.
        assert!(search_matches("@alice", &ws));
        assert!(search_matches("@bob", &ws));
        assert!(search_matches("@carol", &ws));
        assert!(!search_matches("@mallory", &ws));

        // Terms AND together; negation excludes.
        assert!(search_matches("author:alice login", &ws));
        assert!(!search_matches("author:alice nomatch", &ws));
        assert!(!search_matches("-author:alice", &ws));
        assert!(search_matches("-author:mallory login", &ws));
        assert!(!search_matches("-login", &ws));

        // An unknown `is:` value degrades to bare text instead of
        // silently emptying the sidebar.
        assert!(!search_matches("is:banana", &ws));
    }

    /// `agent:` reaches the agent-text corpus and ONLY that corpus
    /// (#1774): a word present in the title but absent from what the
    /// agent was asked must not match, or the qualifier would be a
    /// no-op alias for bare search.
    #[test]
    fn agent_qualifier_searches_agent_text_not_the_title() {
        let mut ws = workspace_with_task("a", Some("acme/api"), 5);
        ws.gh_issues.first_mut().expect("task").title = "Fix login flow".into();
        let text = "rewrite the parser to handle nested groups";

        assert!(search_evaluate("agent:parser", &ws, Some(text)).matched);
        assert!(search_evaluate("said:parser", &ws, Some(text)).matched);
        // In the title, not in the agent text.
        assert!(!search_evaluate("agent:login", &ws, Some(text)).matched);
        // In the agent text, not in the title — the whole point.
        assert!(!search_matches("parser", &ws));

        // Qualifiers AND with the rest of the grammar, and negate.
        assert!(search_evaluate("agent:parser login", &ws, Some(text)).matched);
        assert!(!search_evaluate("agent:parser nomatch", &ws, Some(text)).matched);
        assert!(!search_evaluate("-agent:parser", &ws, Some(text)).matched);
        assert!(search_evaluate("-agent:zzz login", &ws, Some(text)).matched);

        // Case-insensitive, both directions.
        assert!(search_evaluate("agent:PARSER", &ws, Some("The Parser broke")).matched);
    }

    /// A workspace that has never run an agent has no corpus. An
    /// `agent:` term must simply not match it (#1774) — not match
    /// everything, and not blow up.
    #[test]
    fn agent_qualifier_never_matches_a_workspace_without_an_agent() {
        let mut ws = workspace_with_task("a", Some("acme/api"), 5);
        ws.gh_issues.first_mut().expect("task").title = "parser rewrite".into();

        assert!(!search_evaluate("agent:parser", &ws, None).matched);
        assert!(!search_evaluate("agent:parser", &ws, Some("")).matched);
        // Negated, an absent corpus is a non-hit, so the row survives.
        assert!(search_evaluate("-agent:parser parser", &ws, None).matched);
        // A bare query is unaffected by the missing corpus.
        assert!(search_evaluate("parser", &ws, None).matched);
    }

    /// The compatibility guarantee the grammar's doc comment makes: a
    /// query carrying no qualifier resolves identically whether or not
    /// a corpus is present, so adding agent text can't shift the legacy
    /// result set (#1774).
    #[test]
    fn bare_queries_are_unchanged_by_the_agent_corpus() {
        let mut ws = workspace_with_task("a", Some("acme/api"), 5);
        {
            let t = ws.gh_issues.first_mut().expect("task");
            t.title = "Fix login flow".into();
            t.author = "Alice".into();
            t.labels = vec![lazybox_core::Label::new("bug")];
        }
        let text = "nothing here matches the metadata terms zzz";
        for q in [
            "login",
            "bug",
            "acme",
            "sfb",
            "#1",
            "nomatch",
            "zzz",
            "author:alice",
            "-login",
            "is:issue",
            "@alice",
            "",
        ] {
            assert_eq!(
                search_evaluate(q, &ws, Some(text)).matched,
                search_matches(q, &ws),
                "query {q:?} must resolve identically with and without a corpus"
            );
        }
    }

    /// A quoted value keeps its spaces, so a phrase is one term rather
    /// than a word plus a stray bare term (#1774).
    #[test]
    fn quoted_qualifier_values_match_whole_phrases() {
        let ws = workspace_with_task("a", Some("acme/api"), 5);
        let text = "error: cannot borrow `self` as mutable";

        assert!(search_evaluate("said:\"cannot borrow\"", &ws, Some(text)).matched);
        assert!(!search_evaluate("said:\"cannot compile\"", &ws, Some(text)).matched);
        // Unquoted, the second word is a bare term matched against
        // metadata — which this workspace's title doesn't carry.
        assert!(!search_evaluate("said:cannot borrow", &ws, Some(text)).matched);

        // Only a quote opening right after `:` groups; a bare quote is
        // ordinary text, so legacy tokenization is untouched.
        assert_eq!(search_terms("a \"b c\" d"), vec!["a", "\"b", "c\"", "d"]);
        assert_eq!(search_terms("said:\"b c\" d"), vec!["said:\"b c\"", "d"]);
        assert_eq!(search_terms("  spaced   out "), vec!["spaced", "out"]);
    }

    /// An unclosed quote must not group (#1774). It used to swallow the
    /// rest of the query into one needle, so a qualifier typed after it
    /// was silently dropped from the filter — trivially reachable while
    /// typing a phrase left-to-right.
    #[test]
    fn an_unclosed_quote_does_not_swallow_the_rest_of_the_query() {
        assert_eq!(
            search_terms("said:\"a b is:pr"),
            vec!["said:\"a", "b", "is:pr"],
            "without a closing quote the run splits on whitespace as usual"
        );
        // The filter that was being dropped now survives.
        let mut ws = workspace_with_task("a", Some("acme/api"), 5);
        ws.gh_issues.first_mut().expect("task").title = "zzz".into();
        assert!(
            !search_evaluate("said:\"parser is:pr", &ws, Some("rewrite the parser")).matched,
            "`is:pr` still applies, and this workspace is an issue"
        );
        // A closed quote still groups.
        assert_eq!(
            search_terms("said:\"a b\" is:pr"),
            vec!["said:\"a b\"", "is:pr"]
        );
        // Every prefix of a phrase typed left-to-right tokenizes without
        // ever absorbing a later term.
        for n in 0.."said:\"cannot borrow\" is:pr".len() {
            let typed = &"said:\"cannot borrow\" is:pr"[..n];
            assert!(
                search_terms(typed).len() >= typed.split_whitespace().count().saturating_sub(1),
                "prefix {typed:?} collapsed its terms"
            );
        }
    }

    /// Every query WITHOUT a `field:"` sequence must tokenize exactly as
    /// `split_whitespace` did — the compatibility guarantee the grammar
    /// documents. Splitting on the ASCII subset instead of the Unicode
    /// `White_Space` property silently fuses terms around a pasted NBSP
    /// and empties the result set (#1774).
    #[test]
    fn unquoted_queries_tokenize_exactly_like_split_whitespace() {
        for q in [
            "login flow",
            "  spaced   out ",
            "login\u{a0}flow",    // NBSP, what a paste from HTML carries
            "login\u{3000}flow",  // ideographic space, from a CJK IME
            "login\u{b}flow",     // vertical tab: whitespace, but not ASCII-ws
            "\u{a0}\u{2007}lead", // leading Unicode whitespace
            "a \"b c\" d",        // a quote NOT opening a qualifier value
            "",
            "   ",
        ] {
            assert_eq!(
                search_terms(q),
                q.split_whitespace().collect::<Vec<_>>(),
                "query {q:?} must tokenize as the legacy search did"
            );
        }

        // And the end-to-end consequence: an NBSP-separated query still
        // finds the row its ASCII-spaced twin does.
        let mut ws = workspace_with_task("a", Some("acme/api"), 5);
        ws.gh_issues.first_mut().expect("task").title = "Fix login flow".into();
        assert!(search_matches("login flow", &ws));
        assert!(search_matches("login\u{a0}flow", &ws));
    }

    /// The excerpt is the row's "why did this match" cue, so it must
    /// carry the hit, collapse the corpus's newlines to one line, and
    /// stay within its width budget (#1774).
    #[test]
    fn agent_hit_yields_a_bounded_single_line_excerpt() {
        let ws = workspace_with_task("a", Some("acme/api"), 5);
        let text = "first line\n\n   second line mentions the parser here\nthird line";

        let hit = search_evaluate("agent:parser", &ws, Some(text));
        assert!(hit.matched);
        let excerpt = hit.agent_excerpt.expect("hit yields an excerpt");
        assert!(
            excerpt.to_lowercase().contains("parser"),
            "excerpt must carry the match: {excerpt:?}"
        );
        assert!(
            !excerpt.contains('\n'),
            "excerpt must be one line: {excerpt:?}"
        );
        assert!(
            !excerpt.contains("  "),
            "runs of whitespace collapse: {excerpt:?}"
        );
        assert!(
            excerpt.chars().count() <= AGENT_EXCERPT_CHARS + 2,
            "excerpt stays within budget (plus ellipses): {excerpt:?}"
        );

        // A long corpus is elided on both sides.
        let long = format!("{} needle {}", "pad ".repeat(40), "tail ".repeat(40));
        let excerpt = search_evaluate("agent:needle", &ws, Some(&long))
            .agent_excerpt
            .expect("excerpt");
        assert!(
            excerpt.starts_with('…') && excerpt.ends_with('…'),
            "{excerpt:?}"
        );

        // No excerpt without an `agent:` term, and none for a row the
        // rest of the query rejected.
        assert!(
            search_evaluate("login", &ws, Some(text))
                .agent_excerpt
                .is_none()
        );
        assert!(
            search_evaluate("agent:parser nomatch", &ws, Some(text))
                .agent_excerpt
                .is_none()
        );
    }

    /// A multi-byte corpus must not panic the excerpt's window walk or
    /// the case-folded scan (#1774).
    #[test]
    fn agent_search_handles_multibyte_text() {
        let ws = workspace_with_task("a", Some("acme/api"), 5);
        let text = "né… café ☕ the PARSER exploded 日本語テキスト";

        let hit = search_evaluate("agent:parser", &ws, Some(text));
        assert!(hit.matched);
        assert!(
            hit.agent_excerpt
                .expect("excerpt")
                .to_lowercase()
                .contains("parser")
        );
        assert!(search_evaluate("agent:café", &ws, Some(text)).matched);
        assert!(!search_evaluate("agent:zzz", &ws, Some(text)).matched);
    }

    /// `agent:` folds case the same way every other qualifier does, in
    /// both directions (#1774). Folding the corpus ASCII-only made this
    /// the one case-sensitive qualifier in the grammar — and
    /// asymmetrically, since the needle arrives `to_lowercase`d: an
    /// upper-case query found lower-case text but not the reverse, so a
    /// prompt mentioning `CAFÉ` was unreachable by `agent:café` while
    /// `label:café` matched a `CAFÉ` label.
    #[test]
    fn agent_qualifier_folds_case_like_every_other_qualifier() {
        let mut ws = workspace_with_task("a", Some("acme/api"), 5);
        {
            let t = ws.gh_issues.first_mut().expect("task");
            t.title = "zzz".into();
            t.labels = vec![lazybox_core::Label::new("CAFÉ")];
        }
        // The sibling qualifier is the oracle: whatever `label:` does
        // with this needle, `agent:` must do with the same text.
        assert!(search_matches("label:café", &ws), "oracle: label: folds É");
        for (corpus, query) in [
            ("Error: CAFÉ_CONFIG missing", "agent:café"),
            ("Error: café_config missing", "agent:CAFÉ"),
            ("ÉCOLE", "agent:école"),
            ("école", "agent:ÉCOLE"),
            ("STRAßE", "said:straße"),
            ("Grüße", "said:GRÜSSE"),
        ] {
            let hit = search_evaluate(query, &ws, Some(corpus)).matched;
            // `GRÜSSE`.to_lowercase() is `grüsse`, which is NOT a
            // substring of `grüße` — full case folding is a different
            // operation and neither this nor `label:` claims it. Assert
            // the two agree rather than asserting a hit.
            let oracle = corpus.to_lowercase().contains(
                normalized_query(query)
                    .to_lowercase()
                    .split_once(':')
                    .expect("qualifier")
                    .1,
            );
            assert_eq!(
                hit, oracle,
                "agent:/said: must fold {query:?} against {corpus:?} exactly as \
                 `to_lowercase().contains()` does"
            );
        }
    }

    /// Announced re-entry (#scale, B4): a row whose event-conditional
    /// snooze fired within WOKE_WINDOW floats to the top of its group
    /// in every sort mode; past the window it falls back into place.
    #[test]
    fn recently_woken_rows_float_to_the_top_of_their_group() {
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();

        // "fresh" is more recently updated, so recency alone puts it
        // first — the woke boost must override that for "woken".
        let fresh = workspace_with_task("fresh", Some("repo"), 1);
        let mut woken = workspace_with_task("woken", Some("repo"), 60);
        woken.woke_at = Some(fixed_time() - Duration::minutes(5));
        let ws: HashMap<SessionKey, Workspace> = [
            (SessionKey::from("fresh"), fresh),
            (SessionKey::from("woken"), woken),
        ]
        .into();

        let order = |out: &ComputeOutcome| -> Vec<String> {
            out.visible
                .iter()
                .filter_map(|r| match r {
                    VisibleRow::Workspace(k) => Some(k.to_string()),
                    _ => None,
                })
                .collect()
        };
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert_eq!(
            order(&out),
            vec!["woken".to_string(), "fresh".to_string()],
            "the woken row leads its group despite older activity"
        );

        // Past the window the boost expires (stale woke_at).
        let mut ws2 = ws.clone();
        ws2.get_mut(&SessionKey::from("woken")).unwrap().woke_at =
            Some(fixed_time() - lazybox_core::WOKE_WINDOW - Duration::minutes(1));
        let out = compute_visible(inputs(&ws2, &sub, &col, &att, &asking, &projects));
        assert_eq!(
            order(&out),
            vec!["fresh".to_string(), "woken".to_string()],
            "an expired announcement falls back to normal order"
        );
    }
}

/// The desktop client (#732) consumes these view-model types as
/// generated TypeScript. Pin that each one derives `ts_rs::TS` and
/// produces a non-empty declaration, so a serde/shape change that
/// breaks the contract fails here rather than in the desktop build.
#[cfg(all(test, feature = "desktop-contract"))]
mod contract_tests {
    use super::*;
    use ts_rs::{Config, TS};

    #[test]
    fn view_model_types_have_typescript_declarations() {
        let cfg = Config::default();
        assert!(ComputeOutcome::decl(&cfg).contains("ComputeOutcome"));
        assert!(VisibleRow::decl(&cfg).contains("VisibleRow"));
        assert!(TicketTreeMeta::decl(&cfg).contains("TicketTreeMeta"));
        assert!(WorkspaceKind::decl(&cfg).contains("WorkspaceKind"));
        assert!(SortMode::decl(&cfg).contains("SortMode"));
        assert!(Filter::decl(&cfg).contains("Filter"));
        assert!(FilterAxis::decl(&cfg).contains("FilterAxis"));
        assert!(FilterMenuItem::decl(&cfg).contains("FilterMenuItem"));
        assert!(RepoSummary::decl(&cfg).contains("RepoSummary"));
    }
}
