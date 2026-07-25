//! First-run / re-run setup as a **pure** state machine.
//!
//! The runner detects available tools, asks the user to enable
//! integrations (providers + agents), configure per-provider filters,
//! and pick scopes (orgs + repos) for providers that support them.
//!
//! ## Three layers
//!
//! This module is layer 1 — pure logic, no tuirealm and no tokio. A
//! transition takes the current state plus one input event and returns
//! a [`RunnerStep`]: a [`Screen`] *descriptor* (what to show) and an
//! optional [`Effect`] (IO to run). It never builds a widget or spawns
//! a task.
//!
//! - **Renderer** (`crate::realm::setup_screen::render`) turns a
//!   [`Screen`] into a concrete modal widget.
//! - **Executor** (`crate::realm::setup_screen::run_effect`) runs an
//!   [`Effect`] against the registered `ScopeSource`s and feeds the
//!   result back as a [`LoadResult`] via [`SetupRunner::step_loading_resolved`].
//!
//! Keeping IO and rendering at the edges means every transition —
//! including the load/refresh steps — is testable with a plain
//! `#[test]`: feed an input, assert on the returned `Screen`/`Effect`.

use crate::setup::{Category, SetupReport, ToolStatus};
use lazybox_core::{PersistedSetup, ProviderConfig, ProviderError, Scope};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Re-export for callers that want the canonical name.
pub type ProviderFilter = ProviderConfig;

// ── SetupOutcome ────────────────────────────────────────────────────────

/// The end product of a successful setup: which providers/agents are
/// enabled, per-provider filter (roles + item types), per-provider
/// scope selection. Persisted via `outcome_to_persisted` →
/// `KV_KEY_SETUP`.
#[derive(Debug, Clone)]
pub struct SetupOutcome {
    pub report: SetupReport,
    pub enabled_providers: BTreeSet<String>,
    pub enabled_agents: BTreeSet<String>,
    pub provider_filters: BTreeMap<String, ProviderConfig>,
    pub selected_scopes: BTreeMap<String, BTreeSet<String>>,
}

impl SetupOutcome {
    /// Default outcome: every detected tool enabled, default filter
    /// per provider, no scope narrowing yet.
    pub fn default_enabled(report: SetupReport) -> Self {
        let enabled_providers: BTreeSet<String> = report
            .tools
            .iter()
            .filter(|t| t.category == Category::Provider && t.state.is_found())
            .map(|t| t.id.to_string())
            .collect();
        let enabled_agents = report
            .tools
            .iter()
            .filter(|t| t.category == Category::Agent && t.state.is_found())
            .map(|t| t.id.to_string())
            .collect();
        let provider_filters = enabled_providers
            .iter()
            .map(|id| (id.clone(), ProviderFilter::default_for(id)))
            .collect();
        Self {
            report,
            enabled_providers,
            enabled_agents,
            provider_filters,
            selected_scopes: BTreeMap::new(),
        }
    }
}

// ── Persistence ─────────────────────────────────────────────────────────

pub fn outcome_to_persisted(o: &SetupOutcome) -> PersistedSetup {
    PersistedSetup {
        enabled_providers: o.enabled_providers.clone(),
        enabled_agents: o.enabled_agents.clone(),
        provider_filters: o.provider_filters.clone(),
        selected_scopes: o.selected_scopes.clone(),
    }
}

pub fn persisted_to_outcome(p: PersistedSetup, report: SetupReport) -> SetupOutcome {
    SetupOutcome {
        report,
        enabled_providers: p.enabled_providers,
        enabled_agents: p.enabled_agents,
        provider_filters: p.provider_filters,
        selected_scopes: p.selected_scopes,
    }
}

/// Path to the user's YAML config. The setup wizard, `,` Settings
/// palette, and any hand edits all converge here. Empty / missing
/// → no persisted setup, wizard runs on first launch. Profile-aware
/// via the shared `lazybox_core::paths` resolver.
pub fn config_yaml_path() -> std::path::PathBuf {
    lazybox_core::paths::config_yaml()
}

pub fn load_from_yaml(path: &std::path::Path) -> Option<PersistedSetup> {
    let raw = std::fs::read_to_string(path).ok()?;
    let cfg: lazybox_config::Config = match serde_yaml::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("config.yaml parse failed: {e}");
            return None;
        }
    };
    let s = cfg.setup;
    // The YAML schema keeps the same shape as PersistedSetup but
    // expressed as plain BTrees (no JSON parsing). Convert.
    if s.providers.is_empty() && s.agents.is_empty() && !s.wizard_completed {
        // Treat fully-empty as "no persisted setup" — first-run.
        // The `wizard_completed` marker (written on every wizard
        // Finish) overrides this: a user who completed the wizard
        // with everything un-ticked made a valid choice and must
        // not be dragged through first-run forever.
        return None;
    }
    let provider_filters = s
        .filters
        .into_iter()
        .map(|(k, v)| (k, ProviderConfig { enabled_keys: v }))
        .collect();
    let mut p = PersistedSetup {
        enabled_providers: s.providers,
        enabled_agents: s.agents,
        provider_filters,
        selected_scopes: s.scopes,
    };
    p.migrate_legacy_keys();
    Some(p)
}

/// Persist setup state by merging into `~/.lazybox/config.yaml`.
///
/// Reads the existing file (if any), updates only the `setup:`
/// section, writes back. Hand-edited sections (worktree mounts,
/// hooks, custom editors, etc.) are preserved.
///
/// When the existing file is present but does NOT parse (a hand-edit
/// typo), the malformed file is renamed to `config.yaml.bak-<ts>`
/// and a fresh config is written — never silently overwritten, so a
/// typo plus a wizard run can't wipe slack tokens / keymaps / hooks.
/// `Ok(Some(path))` reports that backup so the caller can surface it.
///
/// Errors (serialize / write / rename failures) bubble up so the UI
/// can tell the user their settings did NOT stick.
///
/// The store-backed wrapper (`save_persisted`) — which also clears the
/// legacy `KV_KEY_SETUP` blob — lives boot-side, since the UI library
/// carries no store handle (#548).

/// File-level core of the boot-side `save_persisted` wrapper,
/// path-injected for tests. Returns the backup path when a malformed
/// pre-existing config was moved aside.
pub fn save_persisted_yaml(
    p: &PersistedSetup,
    path: &std::path::Path,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    use anyhow::Context;

    let mut backed_up: Option<std::path::PathBuf> = None;
    let mut cfg: lazybox_config::Config = match std::fs::read_to_string(path) {
        Ok(raw) => match serde_yaml::from_str(&raw) {
            Ok(cfg) => cfg,
            Err(parse_err) => {
                // The file exists but is malformed. Move it aside
                // instead of clobbering it — the user's hand-edited
                // sections are still recoverable from the .bak.
                let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "config.yaml".to_string());
                let bak = path.with_file_name(format!("{name}.bak-{ts}"));
                std::fs::rename(path, &bak).with_context(|| {
                    format!(
                        "config.yaml is malformed ({parse_err}) and backing it up to {} failed — \
                         refusing to overwrite it",
                        bak.display(),
                    )
                })?;
                tracing::warn!(
                    "config.yaml was malformed ({parse_err}); moved to {} and writing fresh",
                    bak.display(),
                );
                backed_up = Some(bak);
                lazybox_config::Config::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => lazybox_config::Config::default(),
        Err(e) => {
            return Err(anyhow::Error::new(e).context("config.yaml read failed"));
        }
    };

    cfg.setup.providers = p.enabled_providers.clone();
    cfg.setup.agents = p.enabled_agents.clone();
    cfg.setup.filters = p
        .provider_filters
        .iter()
        .map(|(k, v)| (k.clone(), v.enabled_keys.clone()))
        .collect();
    cfg.setup.scopes = p.selected_scopes.clone();
    // Completing the wizard is what writes this file — record the
    // marker so an all-empty selection doesn't read as first-run on
    // the next launch.
    cfg.setup.wizard_completed = true;

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Use the config crate's write path so setup/settings saves keep
    // secret-bearing YAML owner-only, matching `Config::save()`.
    lazybox_config::Config::save_to(&cfg, path).context("config.yaml write failed")?;
    Ok(backed_up)
}

// ── Choices ─────────────────────────────────────────────────────────────

/// One detected tool — payload for both the provider-picker and
/// agent-picker. The `Category` is set at fixture time; the picker
/// filters by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolChoice {
    pub id: String,
    pub display_name: String,
    pub category: Category,
    /// Whether detection succeeded. Missing tools still appear in the
    /// picker (greyed out + un-tickable) so the user knows what the
    /// install hint is for.
    pub found: bool,
    /// Short hint shown after the name. For Found tools: the
    /// authenticated username / version. For Missing tools: the
    /// friendly install-hint label.
    pub detail: String,
}

impl ToolChoice {
    fn from_tool(t: &ToolStatus) -> Self {
        let (found, detail) = match &t.state {
            crate::setup::ToolState::Found { detail } => (true, detail.clone()),
            crate::setup::ToolState::Missing { kind, .. } => (false, kind.label().to_string()),
        };
        Self {
            id: t.id.to_string(),
            display_name: t.display_name.to_string(),
            category: t.category,
            found,
            detail,
        }
    }

    /// Display string for a picker row — `name · detail`. Used by the
    /// renderer (`crate::realm::setup_screen`).
    pub fn label(&self) -> String {
        if self.detail.is_empty() {
            self.display_name.clone()
        } else {
            format!("{}  ·  {}", self.display_name, self.detail)
        }
    }
}

/// One option in a per-provider filter modal — "include PRs", "include
/// reviewer role", etc. Maps to a `ProviderConfig` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterOption {
    pub key: String,
    pub label: String,
}

/// Human-readable provider name for titles + prompts.
pub fn provider_display(id: &str) -> String {
    match id {
        "github" => "GitHub".into(),
        "linear" => "Linear".into(),
        other => other.to_string(),
    }
}

/// Provider-specific filter options. GitHub uses a per-type schema
/// (`pr.*` / `issue.*`); Linear is flat.
fn filter_options(provider_id: &str) -> Vec<FilterOption> {
    match provider_id {
        "github" => vec![
            FilterOption {
                key: "pr.author".into(),
                label: "I authored".into(),
            },
            FilterOption {
                key: "pr.reviewer".into(),
                label: "Awaiting my review".into(),
            },
            FilterOption {
                key: "pr.assignee".into(),
                label: "Assigned to me".into(),
            },
            FilterOption {
                key: "pr.mentioned".into(),
                label: "Mentioned me".into(),
            },
            FilterOption {
                key: "issue.author".into(),
                label: "I authored".into(),
            },
            FilterOption {
                key: "issue.assignee".into(),
                label: "Assigned to me".into(),
            },
            FilterOption {
                key: "issue.mentioned".into(),
                label: "Mentioned me".into(),
            },
        ],
        "linear" => vec![
            FilterOption {
                key: "role.author".into(),
                label: "I created".into(),
            },
            FilterOption {
                key: "role.assignee".into(),
                label: "Assigned to me".into(),
            },
            FilterOption {
                key: "role.subscriber".into(),
                label: "Subscribed to".into(),
            },
            FilterOption {
                key: "role.mentioned".into(),
                label: "Mentioned me".into(),
            },
        ],
        _ => vec![],
    }
}

fn filter_keys_set(c: &ProviderConfig) -> BTreeSet<String> {
    c.enabled_keys.clone()
}

fn filter_from_keys(keys: &BTreeSet<String>) -> ProviderConfig {
    ProviderConfig {
        enabled_keys: keys.clone(),
    }
}

// ── Screens, effects, results (the runner's vocabulary) ─────────────────

/// A screen the runner wants shown — pure data, no widget. The
/// renderer (`crate::realm::setup_screen::render`) turns this into a
/// concrete modal. Each picker variant carries its items plus a
/// `selected` mask aligned to those items (pre-ticks), so the renderer
/// needs no knowledge of which scopes/providers the user had before.
pub enum Screen {
    /// Welcome card. Confirm advances to Providers.
    Splash,
    /// Provider multi-select (github / linear / …).
    Providers {
        items: Vec<ToolChoice>,
        selected: Vec<bool>,
    },
    /// Agent multi-select (claude / codex / …).
    Agents {
        items: Vec<ToolChoice>,
        selected: Vec<bool>,
    },
    /// Per-provider role / item-type filter.
    Filter {
        provider_id: String,
        options: Vec<FilterOption>,
        selected: Vec<bool>,
    },
    /// Spinner shown while the paired [`Effect`] runs.
    Loading { title: String, label: String },
    /// Org picker for a provider.
    ScopePick {
        provider_id: String,
        scopes: Vec<Scope>,
        selected: Vec<bool>,
    },
    /// Repo picker for one org under a provider.
    RepoPick {
        provider_id: String,
        parent_label: String,
        scopes: Vec<Scope>,
        selected: Vec<bool>,
    },
    /// Info / error modal — any key dismisses (resumes the next step).
    Info {
        title: String,
        kind: InfoKind,
        body: String,
    },
}

/// Severity for an [`Screen::Info`] modal. The renderer maps this to a
/// themed accent; classification (auth vs retryable vs permanent) is
/// pure logic and lives in the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoKind {
    Auth,
    Retryable,
    Permanent,
    Notice,
}

/// Background IO the runner wants run. Pure data — the executor
/// (`crate::realm::setup_screen::run_effect`) performs it and feeds the
/// outcome back through [`SetupRunner::step_loading_resolved`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Re-run tool detection (`setup::detect_all`).
    Detect,
    /// Enumerate a provider's top-level scopes (orgs).
    ListScopes { provider_id: String },
    /// Enumerate one org's children (repos).
    ListChildren {
        provider_id: String,
        parent_id: String,
    },
}

/// The typed outcome of an [`Effect`]. The Model downcasts the opaque
/// loading payload into this once at the boundary, so the runner stays
/// free of `Box<dyn Any>`.
pub enum LoadResult {
    /// `Effect::Detect` completed with a fresh report.
    Detected(SetupReport),
    /// `Effect::ListScopes` / `ListChildren` completed.
    Scopes(Result<Vec<Scope>, ProviderError>),
}

/// What the runner wants the orchestrator to do next.
pub enum RunnerStep {
    /// Show `screen`; if `effect` is `Some`, run it and feed the
    /// result back via [`SetupRunner::step_loading_resolved`]. `effect`
    /// is `Some` exactly when `screen` is [`Screen::Loading`].
    Show {
        screen: Screen,
        effect: Option<Effect>,
    },
    /// Setup completed; fire on_complete with this outcome.
    Finish(SetupOutcome),
    /// User cancelled. Drop the wizard, return to defaults.
    Cancel,
    /// No-op / stay on the current modal.
    Stay,
}

impl RunnerStep {
    /// Shorthand for a pure screen transition (no IO).
    fn show(screen: Screen) -> Self {
        RunnerStep::Show {
            screen,
            effect: None,
        }
    }

    /// Shorthand for a loading screen paired with the effect that
    /// resolves it.
    fn run(title: &str, label: String, effect: Effect) -> Self {
        RunnerStep::Show {
            screen: Screen::Loading {
                title: title.to_string(),
                label,
            },
            effect: Some(effect),
        }
    }
}

#[derive(Debug, Clone)]
enum ExpectingStep {
    /// Splash card. Already mounted at runner-construction time;
    /// SplashConfirmed advances to Providers.
    Splash,
    /// Provider multi-select.
    Providers,
    /// `Loading` running fresh `detect_all()` after user hit `r`.
    ProvidersRefresh,
    /// Agent multi-select.
    Agents,
    /// `Loading` re-detecting after `r` on Agents.
    AgentsRefresh,
    /// Filter step for `provider_id`.
    FilterFor(String),
    /// Scope-loading step for `provider_id`.
    ScopeLoadFor(String),
    /// Scope-pick step for `provider_id`.
    ScopePickFor(String),
    /// Repo-loading step for `(provider_id, parent_id, parent_label)`.
    RepoLoadFor(String, String, String),
    /// Repo-pick step for `(provider_id, parent_id)`.
    RepoPickFor(String, String),
    /// Generic info / error modal — dismiss returns to next pending
    /// step. Carries the step the runner was on when the error fired
    /// so dismissal can resume correctly.
    InfoFor(Box<ExpectingStep>),
}

/// Track items behind the active `Choice` modal — `Choice` emits
/// indices and we map them back here.
enum CurrentChoice {
    Providers(Vec<ToolChoice>),
    Agents(Vec<ToolChoice>),
    Filter(Vec<FilterOption>),
    ScopePick(Vec<Scope>),
    RepoPick(Vec<Scope>),
}

/// First-run / `--fresh` setup. Owns the state machine; the model
/// routes `Msg::SplashConfirmed` / `Msg::Choice*` / `Msg::LoadingResolved`
/// / `Msg::ModalDismissed` here when it's active.
pub struct SetupRunner {
    accumulator: SetupOutcome,
    /// Provider ids that have a registered `ScopeSource` — used to
    /// decide which providers get a scope-picking step. The runner
    /// only needs to *know* which providers are enumerable, not how to
    /// enumerate them; the actual `ScopeSource`s live in the executor.
    /// This is what keeps the runner free of `ScopeSource` / IO.
    scope_providers: BTreeSet<String>,
    pending_filters: VecDeque<String>,
    pending_scopes: VecDeque<String>,
    pending_repo_pickers: VecDeque<(String, String, String)>,
    expecting: ExpectingStep,
    current_choice: Option<CurrentChoice>,
    /// Entered via Settings → "Edit scopes" specifically to change
    /// repos. In this mode the scope-pick step walks the repo picker
    /// for EVERY kept org, not just newly-added ones — otherwise a
    /// returning user re-confirming their already-subscribed org skips
    /// straight to Finish and can never re-narrow its repos (the modal
    /// just vanishes). First-run / other flows leave this `false` so
    /// re-confirming an untouched org doesn't drag the user through
    /// its repo picker.
    edit_scopes: bool,
}

/// Entry point for the in-session "Settings" palette. Each variant
/// jumps directly to the relevant wizard step pre-seeded with the
/// current persisted setup, runs only that fragment of the flow,
/// and finishes — the orchestrator then writes the merged result
/// back to the store.
#[derive(Debug, Clone)]
pub enum PartialEntry {
    /// Re-show the providers picker. Filter + scope steps run only
    /// for newly-enabled providers; existing ones keep their config.
    EditProviders,
    /// Re-show the agents picker only.
    EditAgents,
    /// Edit role/type filters for a specific provider.
    EditFilter(String),
    /// Re-show the scope picker for a specific provider so the user
    /// can add (or remove) orgs / repos. Existing selections come
    /// pre-checked.
    EditScopes(String),
}

impl SetupRunner {
    /// Build a runner rooted at `report` (detection already done).
    /// `scope_providers` is the set of provider ids that have a
    /// registered `ScopeSource` (i.e. can enumerate orgs/repos).
    pub fn new(report: SetupReport, scope_providers: BTreeSet<String>) -> Self {
        Self {
            accumulator: SetupOutcome::default_enabled(report),
            scope_providers,
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::Splash,
            current_choice: None,
            edit_scopes: false,
        }
    }

    /// Build a runner pre-seeded with the user's existing persisted
    /// setup, jumping directly to a specific wizard step. Used by
    /// the in-session Settings palette ("Add a repo" / "Edit
    /// agents" / etc.) to avoid re-walking the full wizard. Returns
    /// the runner + the first `RunnerStep` to mount.
    pub fn at_partial(
        outcome: SetupOutcome,
        scope_providers: BTreeSet<String>,
        entry: PartialEntry,
    ) -> (Self, RunnerStep) {
        let mut runner = Self {
            accumulator: outcome,
            scope_providers,
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::Splash,
            current_choice: None,
            edit_scopes: matches!(entry, PartialEntry::EditScopes(_)),
        };
        let step = match entry {
            PartialEntry::EditProviders => {
                let screen = runner.screen_providers();
                runner.expecting = ExpectingStep::Providers;
                RunnerStep::show(screen)
            }
            PartialEntry::EditAgents => {
                let screen = runner.screen_agents();
                runner.expecting = ExpectingStep::Agents;
                RunnerStep::show(screen)
            }
            PartialEntry::EditFilter(provider_id) => {
                let screen = runner.screen_filter(&provider_id);
                runner.expecting = ExpectingStep::FilterFor(provider_id);
                RunnerStep::show(screen)
            }
            PartialEntry::EditScopes(provider_id) => {
                let step = runner.scope_load_step(provider_id.clone());
                runner.expecting = ExpectingStep::ScopeLoadFor(provider_id);
                step
            }
        };
        (runner, step)
    }

    // ── Entry points called by Model::update ──────────────────────────

    /// Splash already mounted at construction — `SplashConfirmed`
    /// advances to Providers.
    pub fn step_splash_confirmed(&mut self) -> RunnerStep {
        let screen = self.screen_providers();
        self.expecting = ExpectingStep::Providers;
        RunnerStep::show(screen)
    }

    /// User picked items in the active Choice.
    pub fn step_choice_picked(&mut self, picks: Vec<usize>) -> RunnerStep {
        let current = match self.current_choice.take() {
            Some(c) => c,
            None => return RunnerStep::Cancel,
        };
        match (self.expecting.clone(), current) {
            (ExpectingStep::Providers, CurrentChoice::Providers(items)) => {
                let chosen: Vec<ToolChoice> = picks
                    .into_iter()
                    .filter_map(|i| items.get(i).cloned())
                    .collect();
                self.accumulator.enabled_providers.clear();
                self.pending_filters.clear();
                self.pending_scopes.clear();
                for choice in chosen {
                    self.accumulator
                        .provider_filters
                        .entry(choice.id.clone())
                        .or_insert_with(|| ProviderFilter::default_for(&choice.id));
                    self.accumulator.enabled_providers.insert(choice.id.clone());
                    self.pending_filters.push_back(choice.id.clone());
                    self.pending_scopes.push_back(choice.id);
                }
                let screen = self.screen_agents();
                self.expecting = ExpectingStep::Agents;
                RunnerStep::show(screen)
            }
            (ExpectingStep::Agents, CurrentChoice::Agents(items)) => {
                let chosen: Vec<ToolChoice> = picks
                    .into_iter()
                    .filter_map(|i| items.get(i).cloned())
                    .collect();
                self.accumulator.enabled_agents.clear();
                for choice in chosen {
                    self.accumulator.enabled_agents.insert(choice.id);
                }
                self.next_filter_step()
            }
            (ExpectingStep::FilterFor(provider_id), CurrentChoice::Filter(items)) => {
                let picked: Vec<FilterOption> = picks
                    .into_iter()
                    .filter_map(|i| items.get(i).cloned())
                    .collect();
                let keys: BTreeSet<String> = picked.into_iter().map(|f| f.key).collect();
                self.accumulator
                    .provider_filters
                    .insert(provider_id, filter_from_keys(&keys));
                self.next_filter_step()
            }
            (ExpectingStep::ScopePickFor(provider_id), CurrentChoice::ScopePick(items)) => {
                let picked: Vec<Scope> = picks
                    .into_iter()
                    .filter_map(|i| items.get(i).cloned())
                    .collect();
                // The picker shows ALL orgs (with pre-ticks for the
                // user's existing subscriptions). The submission is
                // "this is now the complete set of orgs subscribed"
                // — but we must preserve any narrowed repo-level
                // entries under picked orgs, and drop everything
                // under un-picked orgs. The earlier bare
                // `insert(provider, picked_ids)` clobbered all
                // repo-level entries silently — users would add an
                // org and lose every prior narrowed subscription.
                let picked_orgs: BTreeSet<String> = picked.iter().map(|s| s.id.clone()).collect();
                let prefixes: Vec<String> = picked_orgs.iter().map(|o| format!("{o}/")).collect();
                // Snapshot which picked orgs were ALREADY subscribed
                // (org-level or via any narrowed repo) BEFORE this edit.
                // We only force the repo-narrowing step for orgs the
                // user is *adding* now — re-confirming or removing other
                // orgs must NOT drag the user through the repo picker of
                // an org they left untouched.
                let already_subscribed: BTreeSet<String> = {
                    let existing = self
                        .accumulator
                        .selected_scopes
                        .get(&provider_id)
                        .cloned()
                        .unwrap_or_default();
                    picked_orgs
                        .iter()
                        .filter(|org_id| {
                            let prefix = format!("{org_id}/");
                            existing.contains(*org_id)
                                || existing.iter().any(|id| id.starts_with(&prefix))
                        })
                        .cloned()
                        .collect()
                };
                let existing = self
                    .accumulator
                    .selected_scopes
                    .entry(provider_id.clone())
                    .or_default();
                // Keep entries belonging to picked orgs (the org
                // itself, or any repo under it). Drop everything
                // else — that's how un-ticking an org cleans up.
                existing.retain(|id| {
                    picked_orgs.contains(id) || prefixes.iter().any(|p| id.starts_with(p))
                });
                // For each newly-picked org with no narrowed repos
                // yet, add the org-level subscription so the repo
                // picker can later narrow it.
                for org_id in &picked_orgs {
                    let prefix = format!("{org_id}/");
                    let has_child = existing.iter().any(|id| id.starts_with(&prefix));
                    if !has_child {
                        existing.insert(org_id.clone());
                    }
                }
                // Queue the repo picker for NEWLY-added orgs so the user
                // can optionally narrow them. Orgs that were already
                // subscribed keep their settled (possibly narrowed)
                // scopes and are not re-walked — EXCEPT when the user
                // entered via Settings → "Edit scopes" specifically to
                // change repos. In that mode every kept org walks its
                // repo picker (pre-ticked with the current selection),
                // otherwise re-confirming an existing org would skip
                // straight to Finish and the modal would just vanish
                // with no way to re-narrow.
                for s in &picked {
                    if !self.edit_scopes && already_subscribed.contains(&s.id) {
                        continue;
                    }
                    self.pending_repo_pickers.push_back((
                        provider_id.clone(),
                        s.id.clone(),
                        s.label.clone(),
                    ));
                }
                self.next_scope_step()
            }
            (
                ExpectingStep::RepoPickFor(provider_id, parent_id),
                CurrentChoice::RepoPick(items),
            ) => {
                let picked: Vec<Scope> = picks
                    .into_iter()
                    .filter_map(|i| items.get(i).cloned())
                    .collect();
                let entry = self
                    .accumulator
                    .selected_scopes
                    .entry(provider_id)
                    .or_default();
                // Replace this org's subscription with exactly the
                // picked repos: drop the whole-org entry AND any
                // previously-narrowed repo under it, then re-add the
                // current picks. Without clearing the old repos,
                // un-ticking one would leave its stale entry behind.
                let prefix = format!("{parent_id}/");
                entry.retain(|id| *id != parent_id && !id.starts_with(&prefix));
                if picked.is_empty() {
                    // Un-ticking every repo is a real submission, not
                    // a no-op: fall back to the org-level subscription
                    // (the same entry the org picker would have left
                    // when no narrowing exists). Ignoring it silently
                    // kept the previous narrowing as-if-unchanged.
                    entry.insert(parent_id);
                } else {
                    for s in picked {
                        entry.insert(s.id);
                    }
                }
                self.next_repo_step()
            }
            _ => RunnerStep::Cancel,
        }
    }

    /// User hit `r` on the active Choice.
    pub fn step_choice_refresh(&mut self) -> RunnerStep {
        match self.expecting.clone() {
            ExpectingStep::Providers => {
                self.expecting = ExpectingStep::ProvidersRefresh;
                detect_step()
            }
            ExpectingStep::Agents => {
                self.expecting = ExpectingStep::AgentsRefresh;
                detect_step()
            }
            _ => RunnerStep::Stay,
        }
    }

    /// User hit Backspace on the active Choice.
    pub fn step_choice_back(&mut self) -> RunnerStep {
        match self.expecting.clone() {
            ExpectingStep::Splash => RunnerStep::Cancel,
            ExpectingStep::Providers => {
                self.current_choice = None;
                self.expecting = ExpectingStep::Splash;
                RunnerStep::show(Screen::Splash)
            }
            ExpectingStep::Agents => {
                let screen = self.screen_providers();
                self.expecting = ExpectingStep::Providers;
                RunnerStep::show(screen)
            }
            ExpectingStep::ProvidersRefresh => {
                let screen = self.screen_providers();
                self.expecting = ExpectingStep::Providers;
                RunnerStep::show(screen)
            }
            ExpectingStep::AgentsRefresh => {
                let screen = self.screen_agents();
                self.expecting = ExpectingStep::Agents;
                RunnerStep::show(screen)
            }
            // Anything inside the per-provider portion → back to Agents.
            // Step-by-step rewind through filter→scope→repo gets
            // confusing fast; "back to start of provider config" is
            // what users actually want.
            ExpectingStep::FilterFor(_)
            | ExpectingStep::ScopeLoadFor(_)
            | ExpectingStep::ScopePickFor(_)
            | ExpectingStep::RepoLoadFor(_, _, _)
            | ExpectingStep::RepoPickFor(_, _) => {
                self.rebuild_pending_queues();
                let screen = self.screen_agents();
                self.expecting = ExpectingStep::Agents;
                RunnerStep::show(screen)
            }
            ExpectingStep::InfoFor(_) => RunnerStep::Stay,
        }
    }

    /// A loading [`Effect`] resolved — `result` is the typed outcome the
    /// executor produced (the Model downcasts the opaque payload once,
    /// at the boundary, so this stays free of `Box<dyn Any>`).
    pub fn step_loading_resolved(&mut self, result: LoadResult) -> RunnerStep {
        match (self.expecting.clone(), result) {
            (ExpectingStep::ProvidersRefresh, LoadResult::Detected(report)) => {
                self.accumulator.report = report;
                let screen = self.screen_providers();
                self.expecting = ExpectingStep::Providers;
                RunnerStep::show(screen)
            }
            (ExpectingStep::AgentsRefresh, LoadResult::Detected(report)) => {
                self.accumulator.report = report;
                let screen = self.screen_agents();
                self.expecting = ExpectingStep::Agents;
                RunnerStep::show(screen)
            }
            (ExpectingStep::ScopeLoadFor(provider_id), LoadResult::Scopes(res)) => match res {
                Ok(scopes) if scopes.is_empty() => self.next_scope_step(),
                Ok(scopes) => {
                    let screen = self.screen_scope_pick(&provider_id, scopes);
                    self.expecting = ExpectingStep::ScopePickFor(provider_id);
                    RunnerStep::show(screen)
                }
                Err(err) => {
                    let screen = scope_error_screen(&provider_id, "orgs", &err);
                    self.expecting = ExpectingStep::InfoFor(Box::new(self.expecting.clone()));
                    RunnerStep::show(screen)
                }
            },
            (
                ExpectingStep::RepoLoadFor(provider_id, parent_id, parent_label),
                LoadResult::Scopes(res),
            ) => match res {
                Ok(scopes) if scopes.is_empty() => {
                    let screen = empty_repos_screen(&parent_label);
                    self.expecting = ExpectingStep::InfoFor(Box::new(self.expecting.clone()));
                    RunnerStep::show(screen)
                }
                Ok(scopes) => {
                    let screen = self.screen_repo_pick(&provider_id, &parent_label, scopes);
                    self.expecting = ExpectingStep::RepoPickFor(provider_id, parent_id);
                    RunnerStep::show(screen)
                }
                Err(err) => {
                    let screen = scope_error_screen(
                        &provider_id,
                        &format!("repos for {parent_label}"),
                        &err,
                    );
                    self.expecting = ExpectingStep::InfoFor(Box::new(self.expecting.clone()));
                    RunnerStep::show(screen)
                }
            },
            // Stale / mismatched result (e.g. user navigated away before
            // the effect resolved) — ignore.
            _ => RunnerStep::Stay,
        }
    }

    /// User dismissed the active modal (Esc / any key on info modal).
    /// Info modals advance to the next pending step; everything else
    /// cancels the wizard.
    pub fn step_dismissed(&mut self) -> RunnerStep {
        match self.expecting.clone() {
            ExpectingStep::InfoFor(prev) => match *prev {
                ExpectingStep::ScopeLoadFor(_) => {
                    self.expecting = ExpectingStep::ScopeLoadFor(String::new());
                    self.next_scope_step()
                }
                ExpectingStep::RepoLoadFor(_, _, _) => {
                    self.expecting =
                        ExpectingStep::RepoLoadFor(String::new(), String::new(), String::new());
                    self.next_repo_step()
                }
                _ => RunnerStep::Cancel,
            },
            _ => RunnerStep::Cancel,
        }
    }

    // ── Internal step builders ────────────────────────────────────────

    fn next_filter_step(&mut self) -> RunnerStep {
        if let Some(provider_id) = self.pending_filters.pop_front() {
            let screen = self.screen_filter(&provider_id);
            self.expecting = ExpectingStep::FilterFor(provider_id);
            return RunnerStep::show(screen);
        }
        self.next_scope_step()
    }

    fn next_scope_step(&mut self) -> RunnerStep {
        // Skip providers without a registered ScopeSource — we can't
        // enumerate their orgs, and "no narrowing" is the right
        // default for those.
        while let Some(provider_id) = self.pending_scopes.pop_front() {
            if !self.scope_providers.contains(&provider_id) {
                continue;
            }
            let step = self.scope_load_step(provider_id.clone());
            self.expecting = ExpectingStep::ScopeLoadFor(provider_id);
            return step;
        }
        self.next_repo_step()
    }

    fn next_repo_step(&mut self) -> RunnerStep {
        if let Some((provider_id, parent_id, parent_label)) = self.pending_repo_pickers.pop_front()
        {
            tracing::info!(
                "next_repo_step: building Loading for provider={provider_id} parent={parent_id}"
            );
            let step = RunnerStep::run(
                "Setup · repos",
                format!("Fetching {parent_label} repos…"),
                Effect::ListChildren {
                    provider_id: provider_id.clone(),
                    parent_id: parent_id.clone(),
                },
            );
            self.expecting = ExpectingStep::RepoLoadFor(provider_id, parent_id, parent_label);
            return step;
        }
        tracing::info!("next_repo_step: no pending repo pickers — flow finishing");
        RunnerStep::Finish(self.accumulator.clone())
    }

    /// Reset per-provider work queues to match the current
    /// `enabled_providers`. Called on `back` from filter/scope land
    /// so the forward path re-queues every enabled provider fresh.
    fn rebuild_pending_queues(&mut self) {
        self.pending_filters.clear();
        self.pending_scopes.clear();
        self.pending_repo_pickers.clear();
        let providers: Vec<String> = self.accumulator.enabled_providers.iter().cloned().collect();
        for pid in providers {
            self.pending_filters.push_back(pid.clone());
            self.pending_scopes.push_back(pid);
        }
    }

    // ── Screen descriptors (pure) ─────────────────────────────────────
    //
    // Each `screen_*` records the items behind the picker in
    // `current_choice` (so `step_choice_picked` can map indices back)
    // and returns a pure [`Screen`] for the renderer. No widgets, no IO.

    fn screen_providers(&mut self) -> Screen {
        let items = self.tools_in(Category::Provider);
        let enabled = &self.accumulator.enabled_providers;
        let selected = items.iter().map(|c| enabled.contains(&c.id)).collect();
        self.current_choice = Some(CurrentChoice::Providers(items.clone()));
        Screen::Providers { items, selected }
    }

    fn screen_agents(&mut self) -> Screen {
        let items = self.tools_in(Category::Agent);
        let enabled = &self.accumulator.enabled_agents;
        let selected = items.iter().map(|c| enabled.contains(&c.id)).collect();
        self.current_choice = Some(CurrentChoice::Agents(items.clone()));
        Screen::Agents { items, selected }
    }

    fn screen_filter(&mut self, provider_id: &str) -> Screen {
        let options = filter_options(provider_id);
        let active = self
            .accumulator
            .provider_filters
            .get(provider_id)
            .cloned()
            .unwrap_or_else(|| ProviderConfig::default_for(provider_id));
        let selected_keys = filter_keys_set(&active);
        let selected = options
            .iter()
            .map(|o| selected_keys.contains(&o.key))
            .collect();
        self.current_choice = Some(CurrentChoice::Filter(options.clone()));
        Screen::Filter {
            provider_id: provider_id.to_string(),
            options,
            selected,
        }
    }

    fn screen_scope_pick(&mut self, provider_id: &str, scopes: Vec<Scope>) -> Screen {
        // Pre-tick orgs the user already subscribed to. `selected_scopes`
        // mixes two id flavors: an org-level id like `github:acme`
        // (whole-org subscription) and repo-level ids like
        // `github:acme/widget` (narrowed). An org is "already selected"
        // if EITHER its own id is in the set OR any repo under it is.
        // Without the prefix check, an org you've narrowed to specific
        // repos would show up unchecked, and confirming the picker would
        // silently drop all those repos.
        let already = self.subscribed(provider_id);
        let selected = scopes
            .iter()
            .map(|s| {
                let prefix = format!("{}/", s.id);
                already.contains(&s.id) || already.iter().any(|id| id.starts_with(&prefix))
            })
            .collect();
        self.current_choice = Some(CurrentChoice::ScopePick(scopes.clone()));
        Screen::ScopePick {
            provider_id: provider_id.to_string(),
            scopes,
            selected,
        }
    }

    fn screen_repo_pick(
        &mut self,
        provider_id: &str,
        parent_label: &str,
        scopes: Vec<Scope>,
    ) -> Screen {
        // Pre-tick this provider's existing repo subscriptions. Pre-
        // seeding matters most for Settings → "Add / remove repos" —
        // opening the picker and seeing zero ticks looks like all your
        // repos vanished.
        let already = self.subscribed(provider_id);
        let selected = scopes.iter().map(|s| already.contains(&s.id)).collect();
        self.current_choice = Some(CurrentChoice::RepoPick(scopes.clone()));
        Screen::RepoPick {
            provider_id: provider_id.to_string(),
            parent_label: parent_label.to_string(),
            scopes,
            selected,
        }
    }

    /// Loading screen + the `ListScopes` effect that resolves it.
    fn scope_load_step(&self, provider_id: String) -> RunnerStep {
        RunnerStep::run(
            "Setup · scopes",
            format!("Fetching {provider_id} orgs…"),
            Effect::ListScopes { provider_id },
        )
    }

    fn tools_in(&self, category: Category) -> Vec<ToolChoice> {
        self.accumulator
            .report
            .tools
            .iter()
            .filter(|t| t.category == category)
            .map(ToolChoice::from_tool)
            .collect()
    }

    fn subscribed(&self, provider_id: &str) -> BTreeSet<String> {
        self.accumulator
            .selected_scopes
            .get(provider_id)
            .cloned()
            .unwrap_or_default()
    }
}

/// Loading screen + the `Detect` effect that resolves it. Shared by the
/// `r`-refresh paths from both the providers and agents pickers.
fn detect_step() -> RunnerStep {
    RunnerStep::run(
        "Setup · refreshing",
        "Re-detecting providers + agents…".to_string(),
        Effect::Detect,
    )
}

/// Classify a scope-load failure + build the dismiss-to-continue body.
/// Returned as a pure [`Screen::Info`] instead of silently advancing —
/// without it the user picks an org and sees the modal disappear with
/// no explanation.
fn scope_error_screen(provider_id: &str, what: &str, err: &ProviderError) -> Screen {
    let kind = if err.is_auth() {
        InfoKind::Auth
    } else if err.is_retryable() {
        InfoKind::Retryable
    } else {
        InfoKind::Permanent
    };
    let body = format!(
        "Failed to load {what} for {provider_id}.\n\n{}\n\n\
         Press any key to dismiss; setup will continue with what's been \
         configured so far.",
        err.diagnostic()
    );
    Screen::Info {
        title: provider_id.to_string(),
        kind,
        body,
    }
}

/// Info screen for "no repos visible under {parent}". Shown instead of
/// silently moving on so the user knows their org-level subscription is
/// still active but per-repo narrowing didn't happen.
fn empty_repos_screen(parent_label: &str) -> Screen {
    let body = format!(
        "No repositories visible under {parent_label}.\n\n\
         This usually means your token doesn't have repo-read scope, \
         or there are no repos in this org / account.\n\n\
         Setup will continue with the org-level subscription — lazybox \
         will poll for any items the token CAN see in {parent_label}.\n\n\
         Press any key to continue."
    );
    Screen::Info {
        title: parent_label.to_string(),
        kind: InfoKind::Notice,
        body,
    }
}

// Tests may block (tokio::test bodies run via block_on); the
// crate-wide blocking-call ban in clippy.toml targets the run loop.
#[allow(clippy::disallowed_methods)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::ToolState;

    fn report() -> SetupReport {
        SetupReport {
            tools: vec![
                ToolStatus {
                    id: "github",
                    display_name: "GitHub",
                    category: Category::Provider,
                    state: ToolState::Found {
                        detail: "gh".into(),
                    },
                    install_hint: "",
                },
                ToolStatus {
                    id: "claude",
                    display_name: "Claude",
                    category: Category::Agent,
                    state: ToolState::Found {
                        detail: "1.0".into(),
                    },
                    install_hint: "",
                },
            ],
        }
    }

    #[test]
    fn default_enabled_picks_all_found_tools() {
        let o = SetupOutcome::default_enabled(report());
        assert!(o.enabled_providers.contains("github"));
        assert!(o.enabled_agents.contains("claude"));
        assert!(o.provider_filters.contains_key("github"));
    }

    #[test]
    fn yaml_round_trip_preserves_setup_fields() {
        // Write a PersistedSetup to the YAML schema and read it
        // back via load_from_yaml. The round-trip should preserve
        // every populated field.
        use std::collections::{BTreeMap, BTreeSet};
        let original = PersistedSetup {
            enabled_providers: ["github", "linear"].iter().map(|s| s.to_string()).collect(),
            enabled_agents: ["claude", "codex"].iter().map(|s| s.to_string()).collect(),
            provider_filters: {
                let mut m = BTreeMap::new();
                m.insert(
                    "github".to_string(),
                    ProviderConfig {
                        enabled_keys: ["pr.author", "issue.author"]
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                    },
                );
                m
            },
            selected_scopes: {
                let mut m = BTreeMap::new();
                m.insert(
                    "github".to_string(),
                    ["acme/widget", "owner/repo"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<BTreeSet<_>>(),
                );
                m
            },
        };

        // Build the equivalent Config + serialize.
        let mut cfg = lazybox_config::Config::default();
        cfg.setup.providers = original.enabled_providers.clone();
        cfg.setup.agents = original.enabled_agents.clone();
        cfg.setup.filters = original
            .provider_filters
            .iter()
            .map(|(k, v)| (k.clone(), v.enabled_keys.clone()))
            .collect();
        cfg.setup.scopes = original.selected_scopes.clone();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        cfg.save_to(&path).unwrap();

        let loaded = load_from_yaml(&path).expect("non-empty yaml round-trips");
        assert_eq!(loaded.enabled_providers, original.enabled_providers);
        assert_eq!(loaded.enabled_agents, original.enabled_agents);
        assert_eq!(loaded.provider_filters, original.provider_filters);
        assert_eq!(loaded.selected_scopes, original.selected_scopes);
    }

    #[test]
    fn yaml_empty_setup_treated_as_first_run() {
        // An empty config.yaml (or one with only non-setup sections)
        // should return None — the first-run wizard then kicks.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let cfg = lazybox_config::Config::default();
        cfg.save_to(&path).unwrap();
        assert!(load_from_yaml(&path).is_none());
    }

    #[test]
    fn outcome_round_trips_via_persisted() {
        let o = SetupOutcome::default_enabled(report());
        let p = outcome_to_persisted(&o);
        let back = persisted_to_outcome(p, report());
        assert_eq!(o.enabled_providers, back.enabled_providers);
        assert_eq!(o.enabled_agents, back.enabled_agents);
    }

    #[test]
    fn runner_starts_at_splash() {
        let runner = SetupRunner::new(report(), BTreeSet::new());
        assert!(matches!(runner.expecting, ExpectingStep::Splash));
    }

    #[test]
    fn splash_advances_to_providers() {
        let mut runner = SetupRunner::new(report(), BTreeSet::new());
        match runner.step_splash_confirmed() {
            RunnerStep::Show { .. } => {
                assert!(matches!(runner.expecting, ExpectingStep::Providers));
            }
            _ => panic!("expected Next (providers)"),
        }
    }

    #[test]
    fn providers_pick_advances_to_agents() {
        let mut runner = SetupRunner::new(report(), BTreeSet::new());
        let _ = runner.step_splash_confirmed();
        // GitHub is index 0 (only provider in the report).
        match runner.step_choice_picked(vec![0]) {
            RunnerStep::Show { .. } => {
                assert!(matches!(runner.expecting, ExpectingStep::Agents));
            }
            _ => panic!("expected Next (agents)"),
        }
        assert!(runner.accumulator.enabled_providers.contains("github"));
    }

    #[test]
    fn agents_pick_with_no_provider_finishes() {
        let mut runner = SetupRunner::new(report(), BTreeSet::new());
        let _ = runner.step_splash_confirmed();
        // Untick every provider.
        let _ = runner.step_choice_picked(vec![]);
        // Tick claude (index 0 of agents).
        match runner.step_choice_picked(vec![0]) {
            RunnerStep::Finish(out) => {
                assert!(out.enabled_providers.is_empty());
                assert!(out.enabled_agents.contains("claude"));
            }
            other => panic!(
                "expected Finish, got {:?}",
                matches!(other, RunnerStep::Finish(_))
            ),
        }
    }

    #[test]
    fn back_from_providers_returns_to_splash() {
        let mut runner = SetupRunner::new(report(), BTreeSet::new());
        let _ = runner.step_splash_confirmed();
        match runner.step_choice_back() {
            RunnerStep::Show { .. } => {
                assert!(matches!(runner.expecting, ExpectingStep::Splash));
            }
            _ => panic!("expected Next (splash)"),
        }
    }

    #[test]
    fn back_from_splash_cancels() {
        let mut runner = SetupRunner::new(report(), BTreeSet::new());
        // expecting starts at Splash.
        assert!(matches!(runner.step_choice_back(), RunnerStep::Cancel));
    }

    #[test]
    fn back_from_agents_returns_to_providers_with_selection() {
        let mut runner = SetupRunner::new(report(), BTreeSet::new());
        let _ = runner.step_splash_confirmed();
        let _ = runner.step_choice_picked(vec![0]); // pick GitHub
        // Now expecting Agents. Back should rebuild Providers.
        match runner.step_choice_back() {
            RunnerStep::Show { .. } => {
                assert!(matches!(runner.expecting, ExpectingStep::Providers));
            }
            _ => panic!("expected Next (providers)"),
        }
        // GitHub stays selected in accumulator → next forward pass
        // uses with_selected_by to re-tick it.
        assert!(runner.accumulator.enabled_providers.contains("github"));
    }

    #[test]
    fn filter_options_for_known_providers() {
        let gh = filter_options("github");
        assert!(gh.iter().any(|f| f.key == "pr.author"));
        assert!(gh.iter().any(|f| f.key == "pr.reviewer"));
        assert!(gh.iter().any(|f| f.key == "issue.author"));
        assert!(
            !gh.iter().any(|f| f.key == "issue.reviewer"),
            "issues have no reviewer role"
        );
        assert!(
            filter_options("linear")
                .iter()
                .any(|f| f.key == "role.subscriber")
        );
        assert!(filter_options("nonexistent").is_empty());
    }

    /// Regression test for the "I added a repo via Settings and my
    /// existing subscriptions vanished" bug. The orgs picker's
    /// confirm path used to blindly overwrite
    /// `selected_scopes[provider]` with the picked org ids, dropping
    /// every prior narrowed repo entry.
    #[tokio::test(flavor = "current_thread")]
    async fn editing_orgs_preserves_existing_narrowed_repos() {
        // Start state: user is subscribed to ONE specific acme
        // repo and to the whole `acme` org. We simulate the Settings
        // → "Add a repo" flow by constructing a runner with this
        // existing state in its accumulator, then driving the orgs
        // picker confirm path.
        let mut prior: BTreeSet<String> = BTreeSet::new();
        prior.insert("github:acme/widget".into());
        prior.insert("github:acme".into());
        let mut outcome = SetupOutcome {
            enabled_providers: ["github"].iter().map(|s| s.to_string()).collect(),
            ..SetupOutcome::default_enabled(report())
        };
        outcome
            .selected_scopes
            .insert("github".into(), prior.clone());

        let mut runner = SetupRunner {
            accumulator: outcome,
            scope_providers: BTreeSet::new(),
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::ScopePickFor("github".into()),
            current_choice: None,
            edit_scopes: false,
        };
        // The picker offered three orgs; user kept acme +
        // acme ticked and added widget. The CurrentChoice
        // holds the items in the SAME order the picker rendered.
        let items = vec![
            Scope {
                id: "github:acme".into(),
                label: "acme".into(),
                parent: None,
                kind: lazybox_core::ScopeKind::Org,
            },
            Scope {
                id: "github:acme".into(),
                label: "acme".into(),
                parent: None,
                kind: lazybox_core::ScopeKind::Org,
            },
            Scope {
                id: "github:widget".into(),
                label: "widget".into(),
                parent: None,
                kind: lazybox_core::ScopeKind::Org,
            },
        ];
        runner.current_choice = Some(CurrentChoice::ScopePick(items));

        // Drive the confirm: indices 0, 1, 2 — i.e. all three picked.
        let _step = runner.step_choice_picked(vec![0, 1, 2]);

        let after = runner
            .accumulator
            .selected_scopes
            .get("github")
            .cloned()
            .unwrap_or_default();
        // The narrowed acme repo MUST still be there.
        assert!(
            after.contains("github:acme/widget"),
            "narrowed repo subscription dropped after partial flow: {after:?}"
        );
        // The acme whole-org entry stays.
        assert!(after.contains("github:acme"));
        // widget joins as a whole-org subscription (no narrowed
        // children yet).
        assert!(after.contains("github:widget"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn editing_orgs_removes_un_ticked_org_and_its_repos() {
        // If the user *unticks* an org in the picker, every entry
        // under it (whole-org or narrowed repos) should disappear.
        let mut prior: BTreeSet<String> = BTreeSet::new();
        prior.insert("github:doomed".into());
        prior.insert("github:doomed/repo-a".into());
        prior.insert("github:keepme/repo-x".into());
        let mut outcome = SetupOutcome::default_enabled(report());
        outcome.selected_scopes.insert("github".into(), prior);

        let mut runner = SetupRunner {
            accumulator: outcome,
            scope_providers: BTreeSet::new(),
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::ScopePickFor("github".into()),
            current_choice: None,
            edit_scopes: false,
        };
        // Picker shows both orgs; user keeps only `keepme`.
        let items = vec![
            Scope {
                id: "github:doomed".into(),
                label: "doomed".into(),
                parent: None,
                kind: lazybox_core::ScopeKind::Org,
            },
            Scope {
                id: "github:keepme".into(),
                label: "keepme".into(),
                parent: None,
                kind: lazybox_core::ScopeKind::Org,
            },
        ];
        runner.current_choice = Some(CurrentChoice::ScopePick(items));

        let _step = runner.step_choice_picked(vec![1]);

        let after = runner
            .accumulator
            .selected_scopes
            .get("github")
            .cloned()
            .unwrap_or_default();
        assert!(!after.contains("github:doomed"));
        assert!(!after.contains("github:doomed/repo-a"));
        assert!(after.contains("github:keepme/repo-x"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn removing_an_org_does_not_walk_kept_orgs_repos() {
        // Regression: un-ticking one org used to drag the user through
        // the repo picker of every *other* still-subscribed org. The
        // repo picker must only be queued for NEWLY-added orgs.
        let mut prior: BTreeSet<String> = BTreeSet::new();
        prior.insert("github:doomed".into());
        prior.insert("github:keepme/repo-x".into());
        let mut outcome = SetupOutcome::default_enabled(report());
        outcome.selected_scopes.insert("github".into(), prior);

        let mut runner = SetupRunner {
            accumulator: outcome,
            scope_providers: BTreeSet::new(),
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::ScopePickFor("github".into()),
            current_choice: None,
            edit_scopes: false,
        };
        let items = vec![
            Scope {
                id: "github:doomed".into(),
                label: "doomed".into(),
                parent: None,
                kind: lazybox_core::ScopeKind::Org,
            },
            Scope {
                id: "github:keepme".into(),
                label: "keepme".into(),
                parent: None,
                kind: lazybox_core::ScopeKind::Org,
            },
        ];
        runner.current_choice = Some(CurrentChoice::ScopePick(items));

        // Keep only `keepme` (already subscribed) — `doomed` removed.
        let _step = runner.step_choice_picked(vec![1]);

        // `keepme` was already subscribed, so no repo picker is queued.
        assert!(
            runner.pending_repo_pickers.is_empty(),
            "kept-but-unchanged org should not re-walk its repos: {:?}",
            runner.pending_repo_pickers
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn newly_added_org_queues_its_repo_picker() {
        // The flip side: adding a brand-new org still queues its repo
        // picker so the user can optionally narrow it.
        let mut prior: BTreeSet<String> = BTreeSet::new();
        prior.insert("github:keepme/repo-x".into());
        let mut outcome = SetupOutcome::default_enabled(report());
        outcome.selected_scopes.insert("github".into(), prior);

        let mut runner = SetupRunner {
            accumulator: outcome,
            scope_providers: BTreeSet::new(),
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::ScopePickFor("github".into()),
            current_choice: None,
            edit_scopes: false,
        };
        let items = vec![
            Scope {
                id: "github:keepme".into(),
                label: "keepme".into(),
                parent: None,
                kind: lazybox_core::ScopeKind::Org,
            },
            Scope {
                id: "github:fresh".into(),
                label: "fresh".into(),
                parent: None,
                kind: lazybox_core::ScopeKind::Org,
            },
        ];
        runner.current_choice = Some(CurrentChoice::ScopePick(items));

        let _step = runner.step_choice_picked(vec![0, 1]);

        // The flow immediately advances into the repo-loading step for
        // the first newly-added org (`fresh`); the already-subscribed
        // `keepme` is never walked. Anything still queued plus the
        // active step must reference only `fresh`.
        let mut walked: Vec<String> = runner
            .pending_repo_pickers
            .iter()
            .map(|(_, id, _)| id.clone())
            .collect();
        if let ExpectingStep::RepoLoadFor(_, parent_id, _) = &runner.expecting {
            walked.push(parent_id.clone());
        }
        assert_eq!(
            walked,
            vec!["github:fresh".to_string()],
            "only the freshly-added org should be walked through repos"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn edit_scopes_walks_repo_picker_for_already_subscribed_org() {
        // Regression: pressing `,` → GitHub to change which repos an
        // ALREADY-subscribed org tracks used to skip the repo picker
        // (the org counts as `already_subscribed`), so the flow jumped
        // straight to Finish and the modal vanished with no way to
        // re-narrow. In `edit_scopes` mode every kept org must walk its
        // repo picker.
        let mut prior: BTreeSet<String> = BTreeSet::new();
        prior.insert("github:acme".into());
        let mut outcome = SetupOutcome::default_enabled(report());
        outcome.selected_scopes.insert("github".into(), prior);

        let mut runner = SetupRunner {
            accumulator: outcome,
            scope_providers: BTreeSet::new(),
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::ScopePickFor("github".into()),
            current_choice: None,
            // The distinguishing bit: entered via Settings → Edit scopes.
            edit_scopes: true,
        };
        let items = vec![Scope {
            id: "github:acme".into(),
            label: "acme".into(),
            parent: None,
            kind: lazybox_core::ScopeKind::Org,
        }];
        runner.current_choice = Some(CurrentChoice::ScopePick(items));

        // Re-confirm the same (already-subscribed) org.
        let _step = runner.step_choice_picked(vec![0]);

        // The repo picker must be walked even though the org was already
        // subscribed — collect both what's still queued and the active
        // step (the first picker is popped immediately into RepoLoadFor).
        let mut walked: Vec<String> = runner
            .pending_repo_pickers
            .iter()
            .map(|(_, id, _)| id.clone())
            .collect();
        if let ExpectingStep::RepoLoadFor(_, parent_id, _) = &runner.expecting {
            walked.push(parent_id.clone());
        }
        assert_eq!(
            walked,
            vec!["github:acme".to_string()],
            "edit-scopes flow must walk the repo picker for the kept org"
        );
    }

    // ── Load-path tests ───────────────────────────────────────────────
    //
    // These exercise the load → resolve transitions that the old
    // widget-coupled runner could not test (they spawned tokio tasks and
    // returned opaque widgets). Now they're pure `fn(state, input) ->
    // RunnerStep`, so no runtime and no rendering — just assert on the
    // Screen / Effect data. This is the seam the repo bug hid behind.

    /// Helper: assert a step is a Loading screen carrying `effect`.
    fn assert_loading_with(step: &RunnerStep, effect: &Effect) {
        match step {
            RunnerStep::Show {
                screen: Screen::Loading { .. },
                effect: Some(e),
            } => assert_eq!(e, effect),
            _ => panic!("expected a Loading screen paired with {effect:?}"),
        }
    }

    #[test]
    fn edit_scopes_entry_emits_list_scopes_effect() {
        // Entering Settings → Edit scopes must show a spinner AND queue
        // the ListScopes effect — no widget, no tokio, fully inspectable.
        let outcome = SetupOutcome::default_enabled(report());
        let providers: BTreeSet<String> = ["github"].iter().map(|s| s.to_string()).collect();
        let (runner, step) = SetupRunner::at_partial(
            outcome,
            providers,
            PartialEntry::EditScopes("github".into()),
        );
        assert_loading_with(
            &step,
            &Effect::ListScopes {
                provider_id: "github".into(),
            },
        );
        assert!(matches!(runner.expecting, ExpectingStep::ScopeLoadFor(_)));
    }

    #[test]
    fn scope_load_resolving_with_orgs_shows_prechecked_pick() {
        // The org load resolves → we should land on a ScopePick screen
        // whose `selected` mask pre-ticks the org the user had narrowed.
        let mut prior: BTreeSet<String> = BTreeSet::new();
        prior.insert("github:acme/widget".into()); // narrowed repo under acme
        let mut outcome = SetupOutcome::default_enabled(report());
        outcome.selected_scopes.insert("github".into(), prior);
        let providers: BTreeSet<String> = ["github"].iter().map(|s| s.to_string()).collect();
        let (mut runner, _load) = SetupRunner::at_partial(
            outcome,
            providers,
            PartialEntry::EditScopes("github".into()),
        );

        let orgs = vec![
            Scope {
                id: "github:acme".into(),
                label: "acme".into(),
                parent: None,
                kind: lazybox_core::ScopeKind::Org,
            },
            Scope {
                id: "github:other".into(),
                label: "other".into(),
                parent: None,
                kind: lazybox_core::ScopeKind::Org,
            },
        ];
        let step = runner.step_loading_resolved(LoadResult::Scopes(Ok(orgs)));
        match step {
            RunnerStep::Show {
                screen: Screen::ScopePick { selected, .. },
                effect: None,
            } => {
                // acme is pre-ticked via its narrowed repo; other is not.
                assert_eq!(selected, vec![true, false]);
            }
            _ => panic!("expected ScopePick screen"),
        }
        assert!(matches!(runner.expecting, ExpectingStep::ScopePickFor(_)));
    }

    #[test]
    fn scope_load_failure_shows_info_not_silent_dismiss() {
        // An auth failure must surface an Info screen (classified Auth),
        // not silently drop the modal.
        let outcome = SetupOutcome::default_enabled(report());
        let providers: BTreeSet<String> = ["github"].iter().map(|s| s.to_string()).collect();
        let (mut runner, _load) = SetupRunner::at_partial(
            outcome,
            providers,
            PartialEntry::EditScopes("github".into()),
        );
        let err = ProviderError::auth("github", "token expired");
        let step = runner.step_loading_resolved(LoadResult::Scopes(Err(err)));
        match step {
            RunnerStep::Show {
                screen: Screen::Info { kind, .. },
                ..
            } => assert_eq!(kind, InfoKind::Auth),
            _ => panic!("expected an Info screen on load failure"),
        }
        // Dismissing the info resumes the flow rather than cancelling.
        assert!(matches!(runner.expecting, ExpectingStep::InfoFor(_)));
    }

    #[test]
    fn refresh_emits_detect_effect() {
        let mut runner = SetupRunner::new(report(), BTreeSet::new());
        let _ = runner.step_splash_confirmed(); // now at Providers
        let step = runner.step_choice_refresh();
        assert_loading_with(&step, &Effect::Detect);
        assert!(matches!(runner.expecting, ExpectingStep::ProvidersRefresh));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn un_ticking_a_repo_removes_it_from_scopes() {
        // Regression: narrowing an org's repos used to only ADD picks,
        // never clearing previously-subscribed repos — so un-ticking a
        // repo left its stale entry (and its sidebar header) behind.
        let mut prior: BTreeSet<String> = BTreeSet::new();
        prior.insert("github:acme/keep".into());
        prior.insert("github:acme/drop".into());
        let mut outcome = SetupOutcome::default_enabled(report());
        outcome.selected_scopes.insert("github".into(), prior);

        let mut runner = SetupRunner {
            accumulator: outcome,
            scope_providers: BTreeSet::new(),
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::RepoPickFor("github".into(), "github:acme".into()),
            current_choice: None,
            edit_scopes: false,
        };
        let items = vec![
            Scope {
                id: "github:acme/keep".into(),
                label: "keep".into(),
                parent: Some("acme".into()),
                kind: lazybox_core::ScopeKind::Repo,
            },
            Scope {
                id: "github:acme/drop".into(),
                label: "drop".into(),
                parent: Some("acme".into()),
                kind: lazybox_core::ScopeKind::Repo,
            },
        ];
        runner.current_choice = Some(CurrentChoice::RepoPick(items));

        // Keep only `keep`; `drop` is un-ticked.
        let _step = runner.step_choice_picked(vec![0]);

        let after = runner
            .accumulator
            .selected_scopes
            .get("github")
            .cloned()
            .unwrap_or_default();
        assert!(after.contains("github:acme/keep"));
        assert!(
            !after.contains("github:acme/drop"),
            "un-ticked repo should be removed: {after:?}"
        );
    }

    /// Regression: confirming the repo picker with EVERY repo
    /// un-ticked used to be silently ignored, keeping the previous
    /// narrowing as if nothing happened. An empty pick is a real
    /// submission — fall back to the org-level subscription.
    #[tokio::test(flavor = "current_thread")]
    async fn un_ticking_all_repos_falls_back_to_org_level() {
        let mut prior: BTreeSet<String> = BTreeSet::new();
        prior.insert("github:acme/keep".into());
        prior.insert("github:acme/drop".into());
        let mut outcome = SetupOutcome::default_enabled(report());
        outcome.selected_scopes.insert("github".into(), prior);

        let mut runner = SetupRunner {
            accumulator: outcome,
            scope_providers: BTreeSet::new(),
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::RepoPickFor("github".into(), "github:acme".into()),
            current_choice: None,
            edit_scopes: false,
        };
        let items = vec![
            Scope {
                id: "github:acme/keep".into(),
                label: "keep".into(),
                parent: Some("acme".into()),
                kind: lazybox_core::ScopeKind::Repo,
            },
            Scope {
                id: "github:acme/drop".into(),
                label: "drop".into(),
                parent: Some("acme".into()),
                kind: lazybox_core::ScopeKind::Repo,
            },
        ];
        runner.current_choice = Some(CurrentChoice::RepoPick(items));

        // Submit with nothing ticked.
        let _step = runner.step_choice_picked(vec![]);

        let after = runner
            .accumulator
            .selected_scopes
            .get("github")
            .cloned()
            .unwrap_or_default();
        assert!(
            after.contains("github:acme"),
            "empty pick must mean org-level subscription: {after:?}"
        );
        assert!(!after.contains("github:acme/keep"));
        assert!(!after.contains("github:acme/drop"));
    }

    /// A malformed config.yaml (hand-edit typo) + a completed wizard
    /// must NOT clobber the file: the original is moved to a
    /// timestamped .bak byte-for-byte, then a fresh config is
    /// written that parses and carries the saved setup.
    #[test]
    fn malformed_config_is_backed_up_not_clobbered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let malformed = "setup: [unterminated\nslack:\n  token: super-secret\n";
        std::fs::write(&path, malformed).unwrap();

        let p = PersistedSetup {
            enabled_providers: ["github"].iter().map(|s| s.to_string()).collect(),
            enabled_agents: ["claude"].iter().map(|s| s.to_string()).collect(),
            provider_filters: BTreeMap::new(),
            selected_scopes: BTreeMap::new(),
        };
        let backed_up = save_persisted_yaml(&p, &path)
            .expect("save succeeds")
            .expect("a backup path is reported");

        assert_eq!(
            std::fs::read_to_string(&backed_up).unwrap(),
            malformed,
            "the malformed original must be preserved byte-for-byte in the .bak"
        );
        assert!(
            backed_up
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("config.yaml.bak-"),
            "backup name carries the .bak-<timestamp> suffix: {backed_up:?}"
        );
        let loaded = load_from_yaml(&path).expect("the fresh file parses");
        assert_eq!(loaded.enabled_providers, p.enabled_providers);
        assert_eq!(loaded.enabled_agents, p.enabled_agents);
    }

    /// Completing the wizard with every provider/agent un-ticked
    /// persists empty sets — the `wizard_completed` marker must keep
    /// that from reading as first-run forever.
    #[test]
    fn completed_marker_prevents_first_run_loop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let p = PersistedSetup {
            enabled_providers: BTreeSet::new(),
            enabled_agents: BTreeSet::new(),
            provider_filters: BTreeMap::new(),
            selected_scopes: BTreeMap::new(),
        };
        let backed_up = save_persisted_yaml(&p, &path).expect("save succeeds");
        assert!(backed_up.is_none(), "no pre-existing file → no backup");
        assert!(
            load_from_yaml(&path).is_some(),
            "an all-empty but COMPLETED setup must not re-trigger the first-run wizard"
        );
    }
}
