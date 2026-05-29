//! First-run / re-run setup as a state machine over realm modals.
//!
//! The runner detects available tools, asks the user to enable
//! integrations (providers + agents), configure per-provider filters,
//! and pick scopes (orgs + repos) for providers that support them.
//! Each step is a generic `Choice` / `Loading` / `ErrorModal` from
//! `crate::realm::components::*` — pilot-specific knowledge lives in
//! `SetupRunner` which decides which step comes next.

use crate::realm::components::{
    choice::Choice,
    error::{Accent, ErrorModal},
    loading::Loading,
    splash::Splash,
};
use crate::realm::{Msg, UserEvent};
use crate::setup::{self, Category, SetupReport, ToolStatus};
use pilot_core::{KV_KEY_SETUP, PersistedSetup, ProviderConfig, ProviderError, Scope, ScopeSource};
use pilot_store::Store;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use tuirealm::component::AppComponent;

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
/// via the shared `pilot_core::paths` resolver.
fn config_yaml_path() -> std::path::PathBuf {
    pilot_core::paths::config_yaml()
}

/// Load the persisted setup from `~/.pilot/config.yaml::setup`.
///
/// Migration: an older release wrote setup state to the SQLite kv
/// blob (`KV_KEY_SETUP`). We still check that as a fallback so
/// existing users don't lose their wizard answers on upgrade — the
/// next save will rewrite them to YAML and `polling::sources_for`
/// will find them there going forward.
pub fn load_persisted(store: &dyn Store) -> Option<PersistedSetup> {
    if let Some(p) = load_from_yaml(&config_yaml_path()) {
        return Some(p);
    }
    // Legacy kv fallback. Migrates by side-effect on the next save.
    match store.get_kv(KV_KEY_SETUP) {
        Ok(Some(raw)) if !raw.is_empty() => match serde_json::from_str::<PersistedSetup>(&raw) {
            Ok(mut p) => {
                p.migrate_legacy_keys();
                tracing::info!(
                    "setup loaded from legacy kv blob; will migrate to YAML on next save"
                );
                Some(p)
            }
            Err(e) => {
                tracing::warn!("legacy setup blob corrupt: {e}");
                None
            }
        },
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("legacy setup read failed: {e}");
            None
        }
    }
}

fn load_from_yaml(path: &std::path::Path) -> Option<PersistedSetup> {
    let raw = std::fs::read_to_string(path).ok()?;
    let cfg: pilot_config::Config = match serde_yaml::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("config.yaml parse failed: {e}");
            return None;
        }
    };
    let s = cfg.setup;
    // The YAML schema keeps the same shape as PersistedSetup but
    // expressed as plain BTrees (no JSON parsing). Convert.
    if s.providers.is_empty() && s.agents.is_empty() {
        // Treat fully-empty as "no persisted setup" — first-run.
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

/// Persist setup state by merging into `~/.pilot/config.yaml`.
///
/// Reads the existing file (if any), updates only the `setup:`
/// section, writes back. Hand-edited sections (worktree mounts,
/// hooks, custom editors, etc.) are preserved.
pub fn save_persisted(store: &dyn Store, p: &PersistedSetup) {
    let path = config_yaml_path();
    let mut cfg: pilot_config::Config = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_yaml::from_str(&raw).ok())
        .unwrap_or_default();

    cfg.setup.providers = p.enabled_providers.clone();
    cfg.setup.agents = p.enabled_agents.clone();
    cfg.setup.filters = p
        .provider_filters
        .iter()
        .map(|(k, v)| (k.clone(), v.enabled_keys.clone()))
        .collect();
    cfg.setup.scopes = p.selected_scopes.clone();

    let serialized = match serde_yaml::to_string(&cfg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("config.yaml serialize failed: {e}");
            return;
        }
    };

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Atomic-ish write: tmp + rename.
    let tmp = path.with_extension("yaml.tmp");
    if let Err(e) = std::fs::write(&tmp, serialized) {
        tracing::warn!("config.yaml write tmp failed: {e}");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        tracing::warn!("config.yaml rename failed: {e}");
        return;
    }

    // Clean up the legacy kv blob so the YAML stays the only source
    // of truth on subsequent loads.
    let _ = store.set_kv(KV_KEY_SETUP, "");
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

    fn label(&self) -> String {
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
fn provider_display(id: &str) -> String {
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

// ── Async entry shim ────────────────────────────────────────────────────

/// Run detection — convenience wrapper around `setup::detect_all` that
/// `run_embedded_realm` calls before constructing `SetupRunner`.
pub async fn detect() -> SetupReport {
    setup::detect_all().await
}

// ── SetupRunner ─────────────────────────────────────────────────────────

/// What the runner wants the orchestrator to do next.
pub enum RunnerStep {
    /// Mount this component as the next modal step.
    Next(Box<dyn AppComponent<Msg, UserEvent>>),
    /// Setup completed; fire on_complete with this outcome.
    Finish(SetupOutcome),
    /// User cancelled. Drop the wizard, return to defaults.
    Cancel,
    /// No-op / stay on the current modal.
    Stay,
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
    sources: Arc<Vec<Box<dyn ScopeSource>>>,
    pending_filters: VecDeque<String>,
    pending_scopes: VecDeque<String>,
    pending_repo_pickers: VecDeque<(String, String, String)>,
    expecting: ExpectingStep,
    current_choice: Option<CurrentChoice>,
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
    pub fn new(report: SetupReport, sources: Arc<Vec<Box<dyn ScopeSource>>>) -> Self {
        Self {
            accumulator: SetupOutcome::default_enabled(report),
            sources,
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::Splash,
            current_choice: None,
        }
    }

    /// Build a runner pre-seeded with the user's existing persisted
    /// setup, jumping directly to a specific wizard step. Used by
    /// the in-session Settings palette ("Add a repo" / "Edit
    /// agents" / etc.) to avoid re-walking the full wizard. Returns
    /// the runner + the first `RunnerStep` to mount.
    pub fn at_partial(
        outcome: SetupOutcome,
        sources: Arc<Vec<Box<dyn ScopeSource>>>,
        entry: PartialEntry,
    ) -> (Self, RunnerStep) {
        let mut runner = Self {
            accumulator: outcome,
            sources,
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::Splash,
            current_choice: None,
        };
        let step = match entry {
            PartialEntry::EditProviders => {
                let modal = runner.build_providers_modal();
                runner.expecting = ExpectingStep::Providers;
                RunnerStep::Next(modal)
            }
            PartialEntry::EditAgents => {
                let modal = runner.build_agents_modal();
                runner.expecting = ExpectingStep::Agents;
                RunnerStep::Next(modal)
            }
            PartialEntry::EditFilter(provider_id) => {
                let modal = runner.build_filter_modal(&provider_id);
                runner.expecting = ExpectingStep::FilterFor(provider_id);
                RunnerStep::Next(modal)
            }
            PartialEntry::EditScopes(provider_id) => {
                let modal = runner.build_scope_load_modal(provider_id.clone());
                runner.expecting = ExpectingStep::ScopeLoadFor(provider_id);
                RunnerStep::Next(modal)
            }
        };
        (runner, step)
    }

    // ── Entry points called by Model::update ──────────────────────────

    /// Splash already mounted at construction — `SplashConfirmed`
    /// advances to Providers.
    pub fn step_splash_confirmed(&mut self) -> RunnerStep {
        let modal = self.build_providers_modal();
        self.expecting = ExpectingStep::Providers;
        RunnerStep::Next(modal)
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
                let modal = self.build_agents_modal();
                self.expecting = ExpectingStep::Agents;
                RunnerStep::Next(modal)
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
                // Queue the repo picker only for NEWLY-added orgs so the
                // user can optionally narrow them. Orgs that were already
                // subscribed keep their settled (possibly narrowed)
                // scopes and are not re-walked.
                for s in &picked {
                    if already_subscribed.contains(&s.id) {
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
                if !picked.is_empty() {
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
                let modal = build_detect_modal();
                self.expecting = ExpectingStep::ProvidersRefresh;
                RunnerStep::Next(modal)
            }
            ExpectingStep::Agents => {
                let modal = build_detect_modal();
                self.expecting = ExpectingStep::AgentsRefresh;
                RunnerStep::Next(modal)
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
                RunnerStep::Next(Box::new(Splash::new()))
            }
            ExpectingStep::Agents => {
                let modal = self.build_providers_modal();
                self.expecting = ExpectingStep::Providers;
                RunnerStep::Next(modal)
            }
            ExpectingStep::ProvidersRefresh => {
                let modal = self.build_providers_modal();
                self.expecting = ExpectingStep::Providers;
                RunnerStep::Next(modal)
            }
            ExpectingStep::AgentsRefresh => {
                let modal = self.build_agents_modal();
                self.expecting = ExpectingStep::Agents;
                RunnerStep::Next(modal)
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
                let modal = self.build_agents_modal();
                self.expecting = ExpectingStep::Agents;
                RunnerStep::Next(modal)
            }
            ExpectingStep::InfoFor(_) => RunnerStep::Stay,
        }
    }

    /// Loading modal resolved its background task — payload is the
    /// boxed value the producer sent.
    pub fn step_loading_resolved(&mut self, payload: Box<dyn std::any::Any + Send>) -> RunnerStep {
        match self.expecting.clone() {
            ExpectingStep::ProvidersRefresh => {
                if let Ok(report) = payload.downcast::<crate::setup::SetupReport>() {
                    self.accumulator.report = *report;
                }
                let modal = self.build_providers_modal();
                self.expecting = ExpectingStep::Providers;
                RunnerStep::Next(modal)
            }
            ExpectingStep::AgentsRefresh => {
                if let Ok(report) = payload.downcast::<crate::setup::SetupReport>() {
                    self.accumulator.report = *report;
                }
                let modal = self.build_agents_modal();
                self.expecting = ExpectingStep::Agents;
                RunnerStep::Next(modal)
            }
            ExpectingStep::ScopeLoadFor(provider_id) => {
                match downcast_load_result::<Scope>(payload) {
                    LoadOutcome::Items(scopes) if scopes.is_empty() => self.next_scope_step(),
                    LoadOutcome::Items(scopes) => {
                        let modal = self.build_scope_pick_modal(&provider_id, scopes);
                        self.expecting = ExpectingStep::ScopePickFor(provider_id);
                        RunnerStep::Next(modal)
                    }
                    LoadOutcome::Failed(err) => {
                        let info = scope_error_modal(&provider_id, "orgs", &err);
                        self.expecting = ExpectingStep::InfoFor(Box::new(self.expecting.clone()));
                        RunnerStep::Next(info)
                    }
                    LoadOutcome::BadType => RunnerStep::Cancel,
                }
            }
            ExpectingStep::RepoLoadFor(provider_id, parent_id, parent_label) => {
                match downcast_load_result::<Scope>(payload) {
                    LoadOutcome::Items(scopes) if scopes.is_empty() => {
                        let info = empty_repos_modal(&parent_label);
                        self.expecting = ExpectingStep::InfoFor(Box::new(self.expecting.clone()));
                        RunnerStep::Next(info)
                    }
                    LoadOutcome::Items(scopes) => {
                        let modal = self.build_repo_pick_modal(&provider_id, &parent_label, scopes);
                        self.expecting = ExpectingStep::RepoPickFor(provider_id, parent_id);
                        RunnerStep::Next(modal)
                    }
                    LoadOutcome::Failed(err) => {
                        let info = scope_error_modal(
                            &provider_id,
                            &format!("repos for {parent_label}"),
                            &err,
                        );
                        self.expecting = ExpectingStep::InfoFor(Box::new(self.expecting.clone()));
                        RunnerStep::Next(info)
                    }
                    LoadOutcome::BadType => RunnerStep::Cancel,
                }
            }
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
            let modal = self.build_filter_modal(&provider_id);
            self.expecting = ExpectingStep::FilterFor(provider_id);
            return RunnerStep::Next(modal);
        }
        self.next_scope_step()
    }

    fn next_scope_step(&mut self) -> RunnerStep {
        // Skip providers without a registered ScopeSource — we can't
        // enumerate their orgs, and "no narrowing" is the right
        // default for those.
        while let Some(provider_id) = self.pending_scopes.pop_front() {
            let has_source = self.sources.iter().any(|s| s.provider_id() == provider_id);
            if !has_source {
                continue;
            }
            let modal = self.build_scope_load_modal(provider_id.clone());
            self.expecting = ExpectingStep::ScopeLoadFor(provider_id);
            return RunnerStep::Next(modal);
        }
        self.next_repo_step()
    }

    fn next_repo_step(&mut self) -> RunnerStep {
        if let Some((provider_id, parent_id, parent_label)) = self.pending_repo_pickers.pop_front()
        {
            tracing::info!(
                "next_repo_step: building Loading for provider={provider_id} parent={parent_id}"
            );
            let modal = self.build_repo_load_modal(
                provider_id.clone(),
                parent_id.clone(),
                parent_label.clone(),
            );
            self.expecting = ExpectingStep::RepoLoadFor(provider_id, parent_id, parent_label);
            return RunnerStep::Next(modal);
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

    // ── Realm modal construction ──────────────────────────────────────

    fn build_providers_modal(&mut self) -> Box<dyn AppComponent<Msg, UserEvent>> {
        let items: Vec<ToolChoice> = self
            .accumulator
            .report
            .tools
            .iter()
            .filter(|t| t.category == Category::Provider)
            .map(ToolChoice::from_tool)
            .collect();
        let enabled_ids = self.accumulator.enabled_providers.clone();
        self.current_choice = Some(CurrentChoice::Providers(items.clone()));
        Box::new(
            Choice::multi(
                "Where do your tasks come from?  Pilot polls these for new \
                 PRs, issues, and tickets so you don't have to refresh.",
                items,
            )
            .title("Setup · providers")
            .label(|c: &ToolChoice| c.label())
            .selectable(|c: &ToolChoice| c.found)
            .with_selected_by(move |c: &ToolChoice| enabled_ids.contains(&c.id))
            .with_refresh(true)
            .with_back(true),
        )
    }

    fn build_agents_modal(&mut self) -> Box<dyn AppComponent<Msg, UserEvent>> {
        let items: Vec<ToolChoice> = self
            .accumulator
            .report
            .tools
            .iter()
            .filter(|t| t.category == Category::Agent)
            .map(ToolChoice::from_tool)
            .collect();
        let enabled_ids = self.accumulator.enabled_agents.clone();
        self.current_choice = Some(CurrentChoice::Agents(items.clone()));
        Box::new(
            Choice::multi(
                "Which AI coding agents should pilot let you spawn into a \
                 worktree?  Press `c`/`x`/`u` on a row to drop into them.",
                items,
            )
            .title("Setup · agents")
            .label(|c: &ToolChoice| c.label())
            .selectable(|c: &ToolChoice| c.found)
            .with_selected_by(move |c: &ToolChoice| enabled_ids.contains(&c.id))
            .with_refresh(true)
            .with_back(true),
        )
    }

    fn build_filter_modal(&mut self, provider_id: &str) -> Box<dyn AppComponent<Msg, UserEvent>> {
        let opts = filter_options(provider_id);
        let active = self
            .accumulator
            .provider_filters
            .get(provider_id)
            .cloned()
            .unwrap_or_else(|| ProviderConfig::default_for(provider_id));
        let selected_keys: BTreeSet<String> = filter_keys_set(&active);
        let display = provider_display(provider_id);
        // Section headers: GitHub splits roles by item type (PR vs
        // Issue); Linear shows a single flat role list.
        let section_for: fn(&FilterOption) -> &'static str = match provider_id {
            "github" => |f: &FilterOption| {
                if f.key.starts_with("pr.") {
                    "Pull Requests  ·  your relationship"
                } else if f.key.starts_with("issue.") {
                    "Issues  ·  your relationship"
                } else {
                    ""
                }
            },
            _ => |_| "",
        };
        self.current_choice = Some(CurrentChoice::Filter(opts.clone()));
        Box::new(
            Choice::multi(
                format!(
                    "Which {display} items show up in your inbox?  \
                     Untick everything in a section to skip that item type entirely."
                ),
                opts,
            )
            .title(format!("Setup · {display} · filters"))
            .label(|f: &FilterOption| f.label.clone())
            .section_for(section_for)
            .with_selected_by(move |f: &FilterOption| selected_keys.contains(&f.key))
            .with_back(true),
        )
    }

    fn build_scope_load_modal(&self, provider_id: String) -> Box<dyn AppComponent<Msg, UserEvent>> {
        let sources = self.sources.clone();
        let pid = provider_id.clone();
        let (modal, result) = Loading::pending(format!("Fetching {provider_id} orgs…"));
        tokio::spawn(async move {
            let value = match sources.iter().find(|s| s.provider_id() == pid) {
                Some(src) => src.list_scopes().await,
                None => Ok(Vec::<Scope>::new()),
            };
            let _ = result.send(value);
        });
        Box::new(modal.title("Setup · scopes"))
    }

    fn build_scope_pick_modal(
        &mut self,
        provider_id: &str,
        scopes: Vec<Scope>,
    ) -> Box<dyn AppComponent<Msg, UserEvent>> {
        self.current_choice = Some(CurrentChoice::ScopePick(scopes.clone()));
        // Pre-tick orgs the user already subscribed to. `selected_scopes`
        // mixes two id flavors: an org-level id like `github:acme`
        // (whole-org subscription) and repo-level ids like
        // `github:acme/widget` (narrowed). An org is "already
        // selected" if EITHER its own id is in the set OR any repo
        // under it is. Without the prefix check, an org you've narrowed
        // to specific repos would show up unchecked, and confirming the
        // picker would silently drop all those repos.
        let already: std::collections::BTreeSet<String> = self
            .accumulator
            .selected_scopes
            .get(provider_id)
            .cloned()
            .unwrap_or_default();
        Box::new(
            Choice::multi(format!("{provider_id} · pick orgs (none = all)"), scopes)
                .title("Setup · scopes")
                .label(|s: &Scope| match &s.parent {
                    Some(p) => format!("{p} / {}", s.label),
                    None => s.label.clone(),
                })
                .with_selected_by(move |s: &Scope| {
                    let org_id = &s.id;
                    let prefix = format!("{org_id}/");
                    already.contains(org_id) || already.iter().any(|id| id.starts_with(&prefix))
                })
                .allow_empty(true)
                .with_back(true),
        )
    }

    fn build_repo_load_modal(
        &self,
        provider_id: String,
        parent_id: String,
        parent_label: String,
    ) -> Box<dyn AppComponent<Msg, UserEvent>> {
        let sources = self.sources.clone();
        let pid = provider_id.clone();
        let parent = parent_id.clone();
        let (modal, result) = Loading::pending(format!("Fetching {parent_label} repos…"));
        tokio::spawn(async move {
            let value = match sources.iter().find(|s| s.provider_id() == pid) {
                Some(src) => src.list_children(&parent).await,
                None => Ok(Vec::<Scope>::new()),
            };
            let _ = result.send(value);
        });
        Box::new(modal.title("Setup · repos"))
    }

    fn build_repo_pick_modal(
        &mut self,
        provider_id: &str,
        parent_label: &str,
        scopes: Vec<Scope>,
    ) -> Box<dyn AppComponent<Msg, UserEvent>> {
        self.current_choice = Some(CurrentChoice::RepoPick(scopes.clone()));
        // Find this provider's existing repo subscriptions and
        // pre-tick the ones we recognize. Pre-seeding matters most
        // for Settings → "Add / remove repos" — opening the picker
        // and seeing zero ticks looks like all your repos vanished.
        let already: std::collections::BTreeSet<String> = self
            .accumulator
            .selected_scopes
            .get(provider_id)
            .cloned()
            .unwrap_or_default();
        Box::new(
            Choice::multi(
                format!(
                    "Pick the {parent_label} repos to subscribe to.\n\n\
                     Space toggles a repo. Enter confirms.\n\
                     Backspace goes back without changing the existing \
                     subscription.",
                ),
                scopes,
            )
            .title(format!("Setup · {parent_label} repos"))
            .label(|s: &Scope| s.label.clone())
            .with_selected_by(move |s: &Scope| already.contains(&s.id))
            .with_back(true),
        )
    }
}

/// `Loading` modal running fresh `detect_all()`. Used by both
/// `ProvidersRefresh` and `AgentsRefresh`.
fn build_detect_modal() -> Box<dyn AppComponent<Msg, UserEvent>> {
    let (modal, result) = Loading::pending("Re-detecting providers + agents…");
    tokio::spawn(async move {
        let _ = result.send(setup::detect_all().await);
    });
    Box::new(modal.title("Setup · refreshing"))
}

// ── Loading payload helpers ─────────────────────────────────────────────

enum LoadOutcome<T> {
    Items(Vec<T>),
    Failed(ProviderError),
    /// Programming error — payload type didn't match.
    BadType,
}

fn downcast_load_result<T: std::any::Any + Send + 'static>(
    payload: Box<dyn std::any::Any + Send>,
) -> LoadOutcome<T> {
    let result = match payload.downcast::<Result<Vec<T>, ProviderError>>() {
        Ok(r) => *r,
        Err(_) => return LoadOutcome::BadType,
    };
    match result {
        Ok(v) => LoadOutcome::Items(v),
        Err(e) => LoadOutcome::Failed(e),
    }
}

/// Build an `ErrorModal` for a scope-load failure. Pushed instead of
/// silently advancing — without this the user picks an org and sees
/// the modal disappear with no explanation.
fn scope_error_modal(
    provider_id: &str,
    what: &str,
    err: &ProviderError,
) -> Box<dyn AppComponent<Msg, UserEvent>> {
    let accent = if err.is_auth() {
        Accent::new("auth", crate::theme::current().hover)
    } else if err.is_retryable() {
        Accent::warn("retryable")
    } else {
        Accent::error("permanent")
    };
    let body = format!(
        "Failed to load {what} for {provider_id}.\n\n{}\n\n\
         Press any key to dismiss; setup will continue with what's been \
         configured so far.",
        err.diagnostic()
    );
    Box::new(ErrorModal::new(provider_id, accent, body))
}

/// Info modal for "no repos visible under {parent}". Pushed instead
/// of silently moving on so the user knows their org-level
/// subscription is still active but per-repo narrowing didn't happen.
fn empty_repos_modal(parent_label: &str) -> Box<dyn AppComponent<Msg, UserEvent>> {
    let body = format!(
        "No repositories visible under {parent_label}.\n\n\
         This usually means your token doesn't have repo-read scope, \
         or there are no repos in this org / account.\n\n\
         Setup will continue with the org-level subscription — pilot \
         will poll for any items the token CAN see in {parent_label}.\n\n\
         Press any key to continue."
    );
    Box::new(ErrorModal::new(parent_label, Accent::warn("notice"), body))
}

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
        let mut cfg = pilot_config::Config::default();
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
        let cfg = pilot_config::Config::default();
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
        let runner = SetupRunner::new(report(), Arc::new(Vec::new()));
        assert!(matches!(runner.expecting, ExpectingStep::Splash));
    }

    #[test]
    fn splash_advances_to_providers() {
        let mut runner = SetupRunner::new(report(), Arc::new(Vec::new()));
        match runner.step_splash_confirmed() {
            RunnerStep::Next(_) => {
                assert!(matches!(runner.expecting, ExpectingStep::Providers));
            }
            _ => panic!("expected Next (providers)"),
        }
    }

    #[test]
    fn providers_pick_advances_to_agents() {
        let mut runner = SetupRunner::new(report(), Arc::new(Vec::new()));
        let _ = runner.step_splash_confirmed();
        // GitHub is index 0 (only provider in the report).
        match runner.step_choice_picked(vec![0]) {
            RunnerStep::Next(_) => {
                assert!(matches!(runner.expecting, ExpectingStep::Agents));
            }
            _ => panic!("expected Next (agents)"),
        }
        assert!(runner.accumulator.enabled_providers.contains("github"));
    }

    #[test]
    fn agents_pick_with_no_provider_finishes() {
        let mut runner = SetupRunner::new(report(), Arc::new(Vec::new()));
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
        let mut runner = SetupRunner::new(report(), Arc::new(Vec::new()));
        let _ = runner.step_splash_confirmed();
        match runner.step_choice_back() {
            RunnerStep::Next(_) => {
                assert!(matches!(runner.expecting, ExpectingStep::Splash));
            }
            _ => panic!("expected Next (splash)"),
        }
    }

    #[test]
    fn back_from_splash_cancels() {
        let mut runner = SetupRunner::new(report(), Arc::new(Vec::new()));
        // expecting starts at Splash.
        assert!(matches!(runner.step_choice_back(), RunnerStep::Cancel));
    }

    #[test]
    fn back_from_agents_returns_to_providers_with_selection() {
        let mut runner = SetupRunner::new(report(), Arc::new(Vec::new()));
        let _ = runner.step_splash_confirmed();
        let _ = runner.step_choice_picked(vec![0]); // pick GitHub
        // Now expecting Agents. Back should rebuild Providers.
        match runner.step_choice_back() {
            RunnerStep::Next(_) => {
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
        use std::sync::Arc;

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

        let sources: Arc<Vec<Box<dyn ScopeSource>>> = Arc::new(Vec::new());
        let mut runner = SetupRunner {
            accumulator: outcome,
            sources,
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::ScopePickFor("github".into()),
            current_choice: None,
        };
        // The picker offered three orgs; user kept acme +
        // acme ticked and added widget. The CurrentChoice
        // holds the items in the SAME order the picker rendered.
        let items = vec![
            Scope {
                id: "github:acme".into(),
                label: "acme".into(),
                parent: None,
                kind: pilot_core::ScopeKind::Org,
            },
            Scope {
                id: "github:acme".into(),
                label: "acme".into(),
                parent: None,
                kind: pilot_core::ScopeKind::Org,
            },
            Scope {
                id: "github:widget".into(),
                label: "widget".into(),
                parent: None,
                kind: pilot_core::ScopeKind::Org,
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
        use std::sync::Arc;
        let mut prior: BTreeSet<String> = BTreeSet::new();
        prior.insert("github:doomed".into());
        prior.insert("github:doomed/repo-a".into());
        prior.insert("github:keepme/repo-x".into());
        let mut outcome = SetupOutcome::default_enabled(report());
        outcome.selected_scopes.insert("github".into(), prior);

        let sources: Arc<Vec<Box<dyn ScopeSource>>> = Arc::new(Vec::new());
        let mut runner = SetupRunner {
            accumulator: outcome,
            sources,
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::ScopePickFor("github".into()),
            current_choice: None,
        };
        // Picker shows both orgs; user keeps only `keepme`.
        let items = vec![
            Scope {
                id: "github:doomed".into(),
                label: "doomed".into(),
                parent: None,
                kind: pilot_core::ScopeKind::Org,
            },
            Scope {
                id: "github:keepme".into(),
                label: "keepme".into(),
                parent: None,
                kind: pilot_core::ScopeKind::Org,
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
        use std::sync::Arc;
        let mut prior: BTreeSet<String> = BTreeSet::new();
        prior.insert("github:doomed".into());
        prior.insert("github:keepme/repo-x".into());
        let mut outcome = SetupOutcome::default_enabled(report());
        outcome.selected_scopes.insert("github".into(), prior);

        let sources: Arc<Vec<Box<dyn ScopeSource>>> = Arc::new(Vec::new());
        let mut runner = SetupRunner {
            accumulator: outcome,
            sources,
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::ScopePickFor("github".into()),
            current_choice: None,
        };
        let items = vec![
            Scope {
                id: "github:doomed".into(),
                label: "doomed".into(),
                parent: None,
                kind: pilot_core::ScopeKind::Org,
            },
            Scope {
                id: "github:keepme".into(),
                label: "keepme".into(),
                parent: None,
                kind: pilot_core::ScopeKind::Org,
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
        use std::sync::Arc;
        let mut prior: BTreeSet<String> = BTreeSet::new();
        prior.insert("github:keepme/repo-x".into());
        let mut outcome = SetupOutcome::default_enabled(report());
        outcome.selected_scopes.insert("github".into(), prior);

        let sources: Arc<Vec<Box<dyn ScopeSource>>> = Arc::new(Vec::new());
        let mut runner = SetupRunner {
            accumulator: outcome,
            sources,
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::ScopePickFor("github".into()),
            current_choice: None,
        };
        let items = vec![
            Scope {
                id: "github:keepme".into(),
                label: "keepme".into(),
                parent: None,
                kind: pilot_core::ScopeKind::Org,
            },
            Scope {
                id: "github:fresh".into(),
                label: "fresh".into(),
                parent: None,
                kind: pilot_core::ScopeKind::Org,
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
    async fn un_ticking_a_repo_removes_it_from_scopes() {
        // Regression: narrowing an org's repos used to only ADD picks,
        // never clearing previously-subscribed repos — so un-ticking a
        // repo left its stale entry (and its sidebar header) behind.
        use std::sync::Arc;
        let mut prior: BTreeSet<String> = BTreeSet::new();
        prior.insert("github:acme/keep".into());
        prior.insert("github:acme/drop".into());
        let mut outcome = SetupOutcome::default_enabled(report());
        outcome.selected_scopes.insert("github".into(), prior);

        let sources: Arc<Vec<Box<dyn ScopeSource>>> = Arc::new(Vec::new());
        let mut runner = SetupRunner {
            accumulator: outcome,
            sources,
            pending_filters: VecDeque::new(),
            pending_scopes: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
            expecting: ExpectingStep::RepoPickFor("github".into(), "github:acme".into()),
            current_choice: None,
        };
        let items = vec![
            Scope {
                id: "github:acme/keep".into(),
                label: "keep".into(),
                parent: Some("acme".into()),
                kind: pilot_core::ScopeKind::Repo,
            },
            Scope {
                id: "github:acme/drop".into(),
                label: "drop".into(),
                parent: Some("acme".into()),
                kind: pilot_core::ScopeKind::Repo,
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
}
