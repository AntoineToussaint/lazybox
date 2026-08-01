use crate::{polling, terminal_io};
use lazybox_core::{SessionId, SessionKey};
use lazybox_ipc::{AgentRunAccess, AgentState, TerminalId, TerminalKind};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, Notify};

/// Per-terminal process state shared by terminal lifecycle handlers.
#[derive(Clone, Default)]
pub struct TerminalRegistry {
    /// Wire terminal id to backend session key.
    pub(crate) terminals: Arc<Mutex<HashMap<TerminalId, String>>>,
    /// Owning durable session used to freeze one session during worktree moves.
    pub(crate) terminal_sessions: Arc<Mutex<HashMap<TerminalId, SessionId>>>,
    /// Last durable state broadcast for each agent terminal.
    pub(crate) agent_states: Arc<Mutex<HashMap<TerminalId, AgentState>>>,
    /// Process generation that prevents recovered state bleeding into a reused key.
    pub(crate) agent_state_generations: Arc<Mutex<HashMap<TerminalId, u64>>>,
    /// Workspace session and kind used to rebuild reconnect snapshots.
    pub(crate) terminal_meta: Arc<Mutex<HashMap<TerminalId, (SessionKey, TerminalKind)>>>,
    /// Host-access policy used to decide whether a singleton can be reused.
    terminal_access: Arc<Mutex<HashMap<TerminalId, AgentRunAccess>>>,
    /// Reconnect-visible marker for terminals with permission prompts bypassed.
    pub(crate) no_permission_terminals: Arc<Mutex<HashSet<TerminalId>>>,
    /// Distinguishes shared-main agents from isolated-worktree singletons.
    pub(crate) on_main_terminals: Arc<Mutex<HashSet<TerminalId>>>,
    /// Model-tier label replayed in reconnect snapshots.
    pub(crate) terminal_models: Arc<Mutex<HashMap<TerminalId, String>>>,
    /// Old side of an in-flight exact terminal replacement. The backend
    /// remains registered until teardown, but reconnect snapshots hide it as
    /// soon as the replacement becomes visible.
    pub(crate) superseded_terminals: Arc<Mutex<HashSet<TerminalId>>>,
    /// Provider-owned login terminals. Kept in the terminal registry for
    /// interactive input routing, but their output is connection-private.
    pub(crate) authenticating_terminals: Arc<Mutex<HashSet<TerminalId>>>,
    /// Recovered agents that require restart for the current PTY compatibility generation.
    pub(crate) outdated_agent_terminals: Arc<Mutex<HashSet<TerminalId>>>,
    /// Detection buffers to clear after an answer so stale prompt chrome cannot re-fire.
    pub(crate) agent_detect_resets: Arc<Mutex<HashSet<TerminalId>>>,
    /// Latest structured hook arrival, used to fall back to PTY detection when hooks go stale.
    pub(crate) hook_driven_terminals: Arc<Mutex<HashMap<TerminalId, std::time::Instant>>>,
    /// Distinguishes one-key chooser answers from free-text input requests.
    pub(crate) input_needed_shapes: Arc<Mutex<HashMap<TerminalId, lazybox_agents::PromptShape>>>,
    /// View and submission epochs used to keep client-provoked repaint output
    /// out of agent lifecycle detection.
    pub(crate) agent_terminal_activities: terminal_io::AgentTerminalActivities,
    /// Prevents a delayed prompt write from resurrecting state after teardown.
    terminal_persistence_locks: Arc<parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Prevents concurrent keyboard, chat, and injection writers corrupting a PTY stream.
    terminal_io_locks: Arc<parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

pub(crate) struct TerminalRegistrationGuard {
    terminals: tokio::sync::OwnedMutexGuard<HashMap<TerminalId, String>>,
    terminal_meta: tokio::sync::OwnedMutexGuard<HashMap<TerminalId, (SessionKey, TerminalKind)>>,
    agent_state_generations: tokio::sync::OwnedMutexGuard<HashMap<TerminalId, u64>>,
    superseded_terminals: tokio::sync::OwnedMutexGuard<HashSet<TerminalId>>,
    authenticating_terminals: tokio::sync::OwnedMutexGuard<HashSet<TerminalId>>,
}

impl TerminalRegistrationGuard {
    pub(crate) fn register(
        &mut self,
        id: TerminalId,
        backend_key: String,
        session_key: SessionKey,
        kind: TerminalKind,
        generation: Option<u64>,
    ) {
        self.terminal_meta.insert(id, (session_key, kind));
        self.terminals.insert(id, backend_key);
        if let Some(generation) = generation {
            self.agent_state_generations.insert(id, generation);
        }
    }

    pub(crate) fn register_replacement(
        &mut self,
        old_id: TerminalId,
        id: TerminalId,
        backend_key: String,
        session_key: SessionKey,
        kind: TerminalKind,
        generation: Option<u64>,
        authenticating: bool,
    ) {
        if self.terminals.contains_key(&old_id) {
            self.superseded_terminals.insert(old_id);
        }
        if authenticating {
            self.authenticating_terminals.insert(id);
        }
        self.register(id, backend_key, session_key, kind, generation);
    }
}

pub(crate) struct RecoveredTerminalRegistrationGuard {
    registration: TerminalRegistrationGuard,
    agent_states: tokio::sync::OwnedMutexGuard<HashMap<TerminalId, AgentState>>,
}

impl RecoveredTerminalRegistrationGuard {
    pub(crate) fn register(
        &mut self,
        id: TerminalId,
        backend_key: String,
        session_key: SessionKey,
        kind: TerminalKind,
        recovered_agent: Option<(u64, AgentState)>,
    ) -> Option<AgentState> {
        self.registration.register(
            id,
            backend_key,
            session_key,
            kind,
            recovered_agent.map(|(generation, _)| generation),
        );
        recovered_agent.and_then(|(_, state)| self.agent_states.insert(id, state))
    }
}

impl TerminalRegistry {
    /// Bind a wire terminal id to a backend session for I/O routing.
    pub async fn bind_backend(&self, id: TerminalId, backend_key: String) {
        self.terminals.lock().await.insert(id, backend_key);
    }

    /// Register the two identities that make a terminal live.
    pub async fn register_terminal(
        &self,
        id: TerminalId,
        backend_key: String,
        session_key: SessionKey,
        kind: TerminalKind,
    ) {
        self.lock_registration()
            .await
            .register(id, backend_key, session_key, kind, None);
    }

    /// Remove a terminal and all of its in-memory lifecycle bookkeeping.
    pub async fn remove_terminal(&self, id: TerminalId) -> Option<String> {
        let backend_key = self.terminals.lock().await.remove(&id);
        self.terminal_sessions.lock().await.remove(&id);
        self.agent_states.lock().await.remove(&id);
        self.agent_state_generations.lock().await.remove(&id);
        self.terminal_meta.lock().await.remove(&id);
        self.terminal_access.lock().await.remove(&id);
        self.no_permission_terminals.lock().await.remove(&id);
        self.on_main_terminals.lock().await.remove(&id);
        self.terminal_models.lock().await.remove(&id);
        self.superseded_terminals.lock().await.remove(&id);
        self.authenticating_terminals.lock().await.remove(&id);
        self.outdated_agent_terminals.lock().await.remove(&id);
        self.agent_detect_resets.lock().await.remove(&id);
        self.hook_driven_terminals.lock().await.remove(&id);
        self.input_needed_shapes.lock().await.remove(&id);
        self.agent_terminal_activities.lock().await.remove(&id);
        backend_key
    }

    /// Snapshot the backend key without holding the map lock in the caller.
    pub async fn backend_key_for(&self, id: TerminalId) -> Option<String> {
        self.terminals.lock().await.get(&id).cloned()
    }

    /// Snapshot terminal metadata without holding the map lock in the caller.
    pub async fn terminal_meta_for(&self, id: TerminalId) -> Option<(SessionKey, TerminalKind)> {
        self.terminal_meta.lock().await.get(&id).cloned()
    }

    /// Snapshot the cached agent state without holding the map lock in the caller.
    pub async fn agent_state_for(&self, id: TerminalId) -> Option<AgentState> {
        self.agent_states.lock().await.get(&id).copied()
    }

    /// Snapshot the durable workspace session associated with a terminal.
    pub async fn terminal_session_for(&self, id: TerminalId) -> Option<SessionId> {
        self.terminal_sessions.lock().await.get(&id).copied()
    }

    pub(crate) async fn access_for(&self, id: TerminalId) -> AgentRunAccess {
        self.terminal_access
            .lock()
            .await
            .get(&id)
            .copied()
            .unwrap_or_default()
    }

    pub(crate) async fn record_access(&self, id: TerminalId, access: AgentRunAccess) {
        if access != AgentRunAccess::Default {
            self.terminal_access.lock().await.insert(id, access);
        }
    }

    pub(crate) async fn forget_access(&self, id: TerminalId) {
        self.terminal_access.lock().await.remove(&id);
    }

    /// Associate a live terminal with its durable workspace session.
    pub async fn associate_session(&self, id: TerminalId, session_id: SessionId) {
        self.terminal_sessions.lock().await.insert(id, session_id);
    }

    pub(crate) async fn record_spawn_attributes(
        &self,
        id: TerminalId,
        owning_session: Option<SessionId>,
        access: AgentRunAccess,
        no_permission: bool,
        on_main: bool,
        model_label: Option<&str>,
    ) {
        if let Some(session_id) = owning_session {
            self.terminal_sessions.lock().await.insert(id, session_id);
        }
        self.record_access(id, access).await;
        if no_permission {
            self.no_permission_terminals.lock().await.insert(id);
        }
        if on_main {
            self.on_main_terminals.lock().await.insert(id);
        }
        if let Some(label) = model_label {
            self.terminal_models.lock().await.insert(id, label.into());
        }
    }

    /// Record the last durable state emitted for an agent terminal.
    pub async fn record_agent_state(&self, id: TerminalId, state: AgentState) {
        self.agent_states.lock().await.insert(id, state);
    }

    /// Record the process generation used to reject stale recovered state.
    pub async fn record_agent_state_generation(&self, id: TerminalId, generation: u64) {
        self.agent_state_generations
            .lock()
            .await
            .insert(id, generation);
    }

    /// Record activity from a structured agent hook.
    pub async fn record_hook_activity(&self, id: TerminalId, at: std::time::Instant) {
        self.hook_driven_terminals.lock().await.insert(id, at);
    }

    /// Record the input shape currently presented by an agent.
    pub async fn record_input_needed_shape(
        &self,
        id: TerminalId,
        shape: lazybox_agents::PromptShape,
    ) {
        self.input_needed_shapes.lock().await.insert(id, shape);
    }

    /// Report whether any registered agent is currently working.
    pub async fn any_agent_working(&self) -> bool {
        self.agent_states
            .lock()
            .await
            .values()
            .any(|state| matches!(state, AgentState::Working))
    }

    /// Return the number of live terminal-to-backend registrations.
    pub async fn terminal_count(&self) -> usize {
        self.terminals.lock().await.len()
    }

    /// Snapshot the ids of all live terminal-to-backend registrations.
    pub async fn terminal_ids(&self) -> Vec<TerminalId> {
        self.terminals.lock().await.keys().copied().collect()
    }

    /// Report whether any terminal-to-backend registration is live.
    pub async fn is_empty(&self) -> bool {
        self.terminals.lock().await.is_empty()
    }

    /// Snapshot reconnect metadata without exposing the registry's lock.
    pub async fn terminal_metadata(&self) -> Vec<(TerminalId, SessionKey, TerminalKind)> {
        self.terminal_meta
            .lock()
            .await
            .iter()
            .map(|(id, (session_key, kind))| (*id, session_key.clone(), kind.clone()))
            .collect()
    }

    /// The live agent terminal a workspace-addressed injection or
    /// output read should target: the lowest-id `Agent` terminal
    /// registered for `session_key`, mirroring the TUI's
    /// `agent_terminal_for` tie-break. `None` when the workspace has no
    /// running agent (only shells, or nothing at all).
    pub async fn running_agent_terminal(&self, session_key: &SessionKey) -> Option<TerminalId> {
        self.terminal_meta
            .lock()
            .await
            .iter()
            .filter(|(_, (owner, kind))| {
                owner == session_key && matches!(kind, TerminalKind::Agent(_))
            })
            .map(|(id, _)| *id)
            .min_by_key(|id| id.0)
    }

    pub(crate) async fn agent_terminals_for_review(
        &self,
        session_key: &SessionKey,
        session_id: Option<SessionId>,
    ) -> Vec<TerminalId> {
        let terminal_sessions = self.terminal_sessions.lock().await.clone();
        let terminal_meta = self.terminal_meta.lock().await;
        let mut ids = terminal_meta
            .iter()
            .filter_map(|(terminal_id, (owner, kind))| {
                if owner != session_key || !matches!(kind, TerminalKind::Agent(_)) {
                    return None;
                }
                if session_id.is_some_and(|expected| {
                    terminal_sessions.get(terminal_id).copied() != Some(expected)
                }) {
                    return None;
                }
                Some(*terminal_id)
            })
            .collect::<Vec<_>>();
        ids.sort_unstable_by_key(|id| id.0);
        ids
    }

    /// Report whether a recovered agent requires a compatibility restart.
    pub async fn is_outdated_agent(&self, id: TerminalId) -> bool {
        self.outdated_agent_terminals.lock().await.contains(&id)
    }

    /// Mark a recovered agent as requiring a compatibility restart.
    pub async fn mark_outdated_agent(&self, id: TerminalId) {
        self.outdated_agent_terminals.lock().await.insert(id);
    }

    /// Return the number of recovered agents requiring a compatibility restart.
    pub async fn outdated_agent_count(&self) -> usize {
        self.outdated_agent_terminals.lock().await.len()
    }

    /// Return the latest structured hook arrival recorded for a terminal.
    pub async fn hook_activity_for(&self, id: TerminalId) -> Option<std::time::Instant> {
        self.hook_driven_terminals.lock().await.get(&id).copied()
    }

    /// Report whether teardown removed every per-terminal bookkeeping entry.
    pub async fn bookkeeping_is_empty(&self) -> bool {
        if !self.terminals.lock().await.is_empty() {
            return false;
        }
        if !self.terminal_meta.lock().await.is_empty() {
            return false;
        }
        if !self.terminal_sessions.lock().await.is_empty() {
            return false;
        }
        if !self.agent_state_generations.lock().await.is_empty() {
            return false;
        }
        if !self.agent_states.lock().await.is_empty() {
            return false;
        }
        if !self.terminal_access.lock().await.is_empty() {
            return false;
        }
        if !self.no_permission_terminals.lock().await.is_empty() {
            return false;
        }
        if !self.on_main_terminals.lock().await.is_empty() {
            return false;
        }
        if !self.terminal_models.lock().await.is_empty() {
            return false;
        }
        if !self.superseded_terminals.lock().await.is_empty() {
            return false;
        }
        if !self.authenticating_terminals.lock().await.is_empty() {
            return false;
        }
        if !self.outdated_agent_terminals.lock().await.is_empty() {
            return false;
        }
        if !self.agent_detect_resets.lock().await.is_empty() {
            return false;
        }
        if !self.hook_driven_terminals.lock().await.is_empty() {
            return false;
        }
        if !self.input_needed_shapes.lock().await.is_empty() {
            return false;
        }
        if !self.agent_terminal_activities.lock().await.is_empty() {
            return false;
        }
        true
    }

    pub(crate) async fn lock_terminal_persistence(
        &self,
        backend_key: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let entry = {
            let mut locks = self.terminal_persistence_locks.lock();
            locks.entry(backend_key.to_string()).or_default().clone()
        };
        entry.lock_owned().await
    }

    pub(crate) fn forget_terminal_persistence_lock(&self, backend_key: &str) {
        self.terminal_persistence_locks.lock().remove(backend_key);
    }

    pub(crate) async fn lock_terminal_io(
        &self,
        backend_key: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let entry = {
            let mut locks = self.terminal_io_locks.lock();
            locks.entry(backend_key.to_string()).or_default().clone()
        };
        entry.lock_owned().await
    }

    pub(crate) fn forget_terminal_io_lock(&self, backend_key: &str) {
        self.terminal_io_locks.lock().remove(backend_key);
    }

    pub(crate) async fn lock_registration(&self) -> TerminalRegistrationGuard {
        let terminals = self.terminals.clone().lock_owned().await;
        let terminal_meta = self.terminal_meta.clone().lock_owned().await;
        let agent_state_generations = self.agent_state_generations.clone().lock_owned().await;
        let superseded_terminals = self.superseded_terminals.clone().lock_owned().await;
        let authenticating_terminals = self.authenticating_terminals.clone().lock_owned().await;
        TerminalRegistrationGuard {
            terminals,
            terminal_meta,
            agent_state_generations,
            superseded_terminals,
            authenticating_terminals,
        }
    }

    pub(crate) async fn lock_recovered_registration(&self) -> RecoveredTerminalRegistrationGuard {
        let registration = self.lock_registration().await;
        let agent_states = self.agent_states.clone().lock_owned().await;
        RecoveredTerminalRegistrationGuard {
            registration,
            agent_states,
        }
    }
}

#[derive(Clone, Default)]
pub struct GithubClientCache {
    client: Arc<parking_lot::Mutex<Option<lazybox_gh::GhClient>>>,
    initialization: Arc<Mutex<()>>,
}

impl GithubClientCache {
    pub(crate) fn cached(&self) -> Option<lazybox_gh::GhClient> {
        self.client.lock().clone()
    }

    pub(crate) fn store(&self, client: lazybox_gh::GhClient) {
        *self.client.lock() = Some(client);
    }

    pub(crate) fn clear(&self) {
        *self.client.lock() = None;
    }

    pub(crate) async fn lock_initialization(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.initialization.clone().lock_owned().await
    }
}

/// Cross-tick polling state and caches.
///
/// The fields keep their separate lock domains because the upsert path must
/// not re-enter the cross-tick `tick_state` lock through merge prompt or
/// auto-merge bookkeeping.
#[derive(Clone, Default)]
pub struct PollState {
    /// Provider-error debounce and prompt memory shared by refresh and the poll loop.
    pub(crate) tick_state: Arc<Mutex<polling::TickState>>,
    /// Bounded engagement set written by focus changes and read by poll scheduling.
    pub(crate) engagement: Arc<parking_lot::RwLock<polling::PollEngagement>>,
    /// Long-lived GitHub client whose shared rate budget must survive across ticks.
    pub(crate) gh_client_cache: GithubClientCache,
    /// Issue-to-PR prompt dedupe kept outside `tick_state` to avoid upsert re-entry.
    pub(crate) merge_prompts: Arc<Mutex<polling::MergePromptMemory>>,
    /// Auto-merge latches kept outside `tick_state` because commit paths update them.
    pub(crate) auto_merge: Arc<parking_lot::Mutex<polling::AutoMergeMemory>>,
    /// Removal prompt memory kept outside `tick_state` because upsert paths update it.
    pub(crate) removal_prompts: Arc<Mutex<polling::RemovalPromptMemory>>,
    /// Authenticated logins replayed to reconnecting clients.
    pub(crate) viewer_identities: Arc<parking_lot::Mutex<Vec<(String, String)>>>,
    /// Interrupts the poll sleep for refresh, reconnect, and lazy mergeable retries.
    pub(crate) wake_signal: Arc<Notify>,
    poll_warm_requested: Arc<AtomicBool>,
}

impl PollState {
    /// Wake the polling loop, optionally requesting a warm notification sweep.
    pub fn wake(&self, poll_notifications: bool) {
        if poll_notifications {
            self.poll_warm_requested.store(true, Ordering::Release);
        }
        self.wake_signal.notify_one();
    }

    /// Wait until a refresh request wakes the polling loop.
    pub async fn wait_for_wake(&self) {
        self.wake_signal.notified().await;
    }

    /// Snapshot the workspace currently driving engagement scheduling.
    pub fn focused_workspace(&self) -> Option<String> {
        self.engagement
            .read()
            .focused_workspace()
            .map(str::to_string)
    }

    /// Snapshot the engagement tiers used to schedule provider polling.
    pub fn engagement_snapshot(&self) -> polling::EngagementSnapshot {
        self.engagement.read().snapshot()
    }

    /// Return the polling tier assigned to one workspace.
    pub fn engagement_tier_for(&self, key: &lazybox_core::WorkspaceKey) -> polling::EngagementTier {
        self.engagement.read().tier_for(key)
    }

    /// Snapshot authenticated provider identities for reconnecting clients.
    pub fn viewer_identities(&self) -> Vec<(String, String)> {
        self.viewer_identities.lock().clone()
    }

    /// Snapshot the cached GitHub client without exposing its lock.
    pub fn cached_gh_client(&self) -> Option<lazybox_gh::GhClient> {
        self.gh_client_cache.cached()
    }

    /// Replace the cached GitHub client used across polling ticks.
    pub fn cache_gh_client(&self, client: lazybox_gh::GhClient) {
        self.gh_client_cache.store(client);
    }

    /// Report whether a reusable GitHub client is currently cached.
    pub fn has_cached_gh_client(&self) -> bool {
        self.gh_client_cache.cached().is_some()
    }

    /// Drop the cached GitHub client after an authentication failure.
    pub fn clear_cached_gh_client(&self) {
        self.gh_client_cache.clear();
    }

    /// Report whether the tick-state lock is currently available.
    pub fn tick_state_is_available(&self) -> bool {
        self.tick_state.try_lock().is_ok()
    }

    /// Snapshot the poll scheduler's monotonic tick counter.
    pub async fn round_robin_tick(&self) -> u64 {
        self.tick_state.lock().await.round_robin.tick
    }

    /// Snapshot the repository currently prioritized by the poll scheduler.
    pub async fn focused_repo(&self) -> Option<String> {
        self.tick_state
            .lock()
            .await
            .round_robin
            .focused_repo
            .clone()
    }

    pub(crate) fn take_warm_request(&self) -> bool {
        self.poll_warm_requested.swap(false, Ordering::AcqRel)
    }
}

/// Synchronization state for terminal spawn and prompt injection.
#[derive(Clone, Default)]
pub struct SpawnCoordinator {
    /// Lets an inject task verify submit via the structured hook and retry Enter once.
    pub(crate) prompt_submit_signals: Arc<Mutex<HashMap<TerminalId, Arc<Notify>>>>,
    /// Enforces one readiness-gated injection per terminal.
    pub(crate) pending_prompt_injections: Arc<parking_lot::Mutex<HashSet<TerminalId>>>,
    /// Closes the provisioning gap before terminal maps can enforce singleton spawns.
    /// Each claim carries its cancellation signal and whether it targets
    /// the shared main checkout.
    pub(crate) inflight_spawns:
        Arc<parking_lot::Mutex<HashMap<(String, String), (Arc<Notify>, bool)>>>,
    /// Wakes duplicate spawns and teardown without busy-polling.
    pub(crate) inflight_spawn_changed: Arc<Notify>,
    /// Serializes agent target selection and spawn registration per workspace.
    workspace_agent_actions: Arc<parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl SpawnCoordinator {
    pub(crate) async fn lock_workspace_agent(
        &self,
        session_key: &SessionKey,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.workspace_agent_actions.lock();
            locks.entry(session_key.to_string()).or_default().clone()
        };
        lock.lock_owned().await
    }

    /// Register a waiter that is notified when an agent submits its prompt.
    pub async fn register_prompt_confirmation(&self, id: TerminalId) -> Arc<Notify> {
        let signal = Arc::new(Notify::new());
        self.prompt_submit_signals
            .lock()
            .await
            .insert(id, signal.clone());
        signal
    }

    /// Remove a prompt waiter only when it is still the active registration.
    pub async fn remove_prompt_confirmation(&self, id: TerminalId, signal: &Arc<Notify>) {
        let mut signals = self.prompt_submit_signals.lock().await;
        if signals
            .get(&id)
            .is_some_and(|registered| Arc::ptr_eq(registered, signal))
        {
            signals.remove(&id);
        }
    }

    /// Report whether prompt-submission confirmation has no registered waiters.
    pub async fn prompt_confirmations_are_empty(&self) -> bool {
        self.prompt_submit_signals.lock().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn running_agent_terminal_prefers_the_lowest_id_agent_and_skips_shells() {
        let terminals = TerminalRegistry::default();
        let workspace = SessionKey::from("github:owner/repo#7");
        // A shell in the same workspace must never be an inject target.
        terminals
            .register_terminal(
                TerminalId(1),
                "shell".to_string(),
                workspace.clone(),
                TerminalKind::Shell,
            )
            .await;
        terminals
            .register_terminal(
                TerminalId(9),
                "agent-late".to_string(),
                workspace.clone(),
                TerminalKind::Agent("claude".into()),
            )
            .await;
        terminals
            .register_terminal(
                TerminalId(5),
                "agent-early".to_string(),
                workspace.clone(),
                TerminalKind::Agent("claude".into()),
            )
            .await;

        assert_eq!(
            terminals.running_agent_terminal(&workspace).await,
            Some(TerminalId(5))
        );
        assert_eq!(
            terminals
                .running_agent_terminal(&SessionKey::from("github:owner/repo#404"))
                .await,
            None
        );
    }

    #[tokio::test]
    async fn registries_are_independently_constructible() {
        let terminals = TerminalRegistry::default();
        terminals
            .register_terminal(
                TerminalId(1),
                "backend".to_string(),
                SessionKey::from("test:workspace"),
                TerminalKind::Agent("codex".into()),
            )
            .await;
        let session_id = SessionId::new();
        terminals.associate_session(TerminalId(1), session_id).await;
        terminals
            .record_agent_state(TerminalId(1), AgentState::Working)
            .await;
        terminals
            .record_agent_state_generation(TerminalId(1), 7)
            .await;
        let hook_at = std::time::Instant::now();
        terminals.record_hook_activity(TerminalId(1), hook_at).await;
        terminals
            .record_input_needed_shape(TerminalId(1), lazybox_agents::PromptShape::FreeText)
            .await;
        terminals.mark_outdated_agent(TerminalId(1)).await;

        assert_eq!(terminals.terminal_count().await, 1);
        assert_eq!(terminals.terminal_ids().await, vec![TerminalId(1)]);
        assert!(!terminals.is_empty().await);
        assert_eq!(
            terminals.backend_key_for(TerminalId(1)).await.as_deref(),
            Some("backend")
        );
        assert!(
            matches!(
                terminals.terminal_meta_for(TerminalId(1)).await,
                Some((_, TerminalKind::Agent(agent))) if agent == "codex"
            ),
            "terminal metadata must be published with the backend binding"
        );
        assert_eq!(
            terminals.terminal_session_for(TerminalId(1)).await,
            Some(session_id)
        );
        assert_eq!(
            terminals.agent_state_for(TerminalId(1)).await,
            Some(AgentState::Working)
        );
        assert!(terminals.any_agent_working().await);
        assert_eq!(
            terminals.hook_activity_for(TerminalId(1)).await,
            Some(hook_at)
        );
        assert!(terminals.is_outdated_agent(TerminalId(1)).await);
        assert_eq!(terminals.outdated_agent_count().await, 1);
        assert_eq!(terminals.terminal_metadata().await.len(), 1);

        assert_eq!(
            terminals.remove_terminal(TerminalId(1)).await.as_deref(),
            Some("backend")
        );
        assert!(terminals.bookkeeping_is_empty().await);

        let polling = PollState::default();
        polling.wake(true);
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            polling.wait_for_wake(),
        )
        .await
        .expect("wake permit");
        assert!(polling.take_warm_request());
        assert!(polling.focused_workspace().is_none());
        assert_eq!(
            polling.engagement_tier_for(&lazybox_core::WorkspaceKey::new("test:workspace")),
            polling::EngagementTier::Warm
        );
        assert_eq!(polling.engagement_snapshot().hot_count(), 0);
        assert!(polling.viewer_identities().is_empty());
        assert!(polling.tick_state_is_available());
        assert_eq!(polling.round_robin_tick().await, 0);
        assert!(polling.focused_repo().await.is_none());
        let client =
            lazybox_gh::GhClient::stub_for_tests("test", "fingerprint").expect("stub client");
        polling.cache_gh_client(client);
        assert!(polling.has_cached_gh_client());
        assert!(polling.cached_gh_client().is_some());
        polling.clear_cached_gh_client();
        assert!(!polling.has_cached_gh_client());

        let spawns = SpawnCoordinator::default();
        let stale = spawns.register_prompt_confirmation(TerminalId(2)).await;
        let signal = spawns.register_prompt_confirmation(TerminalId(2)).await;
        assert!(!spawns.prompt_confirmations_are_empty().await);
        spawns
            .remove_prompt_confirmation(TerminalId(2), &stale)
            .await;
        assert!(
            !spawns.prompt_confirmations_are_empty().await,
            "removing a stale waiter must preserve its replacement"
        );
        spawns
            .remove_prompt_confirmation(TerminalId(2), &signal)
            .await;
        assert!(spawns.prompt_confirmations_are_empty().await);
    }

    #[tokio::test]
    async fn cloned_registry_shares_its_lock_domains() {
        let registry = TerminalRegistry::default();
        let clone = registry.clone();
        registry
            .bind_backend(TerminalId(1), "backend".to_string())
            .await;

        assert_eq!(
            clone.backend_key_for(TerminalId(1)).await.as_deref(),
            Some("backend")
        );
    }
}
