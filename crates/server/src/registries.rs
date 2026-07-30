use crate::polling;
use lazybox_core::{SessionId, SessionKey};
use lazybox_ipc::{AgentState, TerminalId, TerminalKind};
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
    /// Reconnect-visible marker for terminals with permission prompts bypassed.
    pub(crate) no_permission_terminals: Arc<Mutex<HashSet<TerminalId>>>,
    /// Distinguishes shared-main agents from isolated-worktree singletons.
    pub(crate) on_main_terminals: Arc<Mutex<HashSet<TerminalId>>>,
    /// Model-tier label replayed in reconnect snapshots.
    pub(crate) terminal_models: Arc<Mutex<HashMap<TerminalId, String>>>,
    /// Recovered agents that require restart for the current PTY compatibility generation.
    pub(crate) outdated_agent_terminals: Arc<Mutex<HashSet<TerminalId>>>,
    /// Detection buffers to clear after an answer so stale prompt chrome cannot re-fire.
    pub(crate) agent_detect_resets: Arc<Mutex<HashSet<TerminalId>>>,
    /// Latest structured hook arrival, used to fall back to PTY detection when hooks go stale.
    pub(crate) hook_driven_terminals: Arc<Mutex<HashMap<TerminalId, std::time::Instant>>>,
    /// Distinguishes one-key chooser answers from free-text input requests.
    pub(crate) input_needed_shapes: Arc<Mutex<HashMap<TerminalId, lazybox_agents::PromptShape>>>,
    /// Prevents a delayed prompt write from resurrecting state after teardown.
    terminal_persistence_locks: Arc<parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Prevents concurrent keyboard, chat, and injection writers corrupting a PTY stream.
    terminal_io_locks: Arc<parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

pub(crate) struct TerminalRegistrationGuard {
    terminals: tokio::sync::OwnedMutexGuard<HashMap<TerminalId, String>>,
    terminal_meta: tokio::sync::OwnedMutexGuard<HashMap<TerminalId, (SessionKey, TerminalKind)>>,
    agent_state_generations: tokio::sync::OwnedMutexGuard<HashMap<TerminalId, u64>>,
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
        self.no_permission_terminals.lock().await.remove(&id);
        self.on_main_terminals.lock().await.remove(&id);
        self.terminal_models.lock().await.remove(&id);
        self.outdated_agent_terminals.lock().await.remove(&id);
        self.agent_detect_resets.lock().await.remove(&id);
        self.hook_driven_terminals.lock().await.remove(&id);
        self.input_needed_shapes.lock().await.remove(&id);
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

    /// Associate a live terminal with its durable workspace session.
    pub async fn associate_session(&self, id: TerminalId, session_id: SessionId) {
        self.terminal_sessions.lock().await.insert(id, session_id);
    }

    pub(crate) async fn record_spawn_attributes(
        &self,
        id: TerminalId,
        owning_session: Option<SessionId>,
        no_permission: bool,
        on_main: bool,
        model_label: Option<&str>,
    ) {
        if let Some(session_id) = owning_session {
            self.terminal_sessions.lock().await.insert(id, session_id);
        }
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
        if !self.no_permission_terminals.lock().await.is_empty() {
            return false;
        }
        if !self.on_main_terminals.lock().await.is_empty() {
            return false;
        }
        if !self.terminal_models.lock().await.is_empty() {
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
        TerminalRegistrationGuard {
            terminals,
            terminal_meta,
            agent_state_generations,
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
    pub(crate) gh_client_cache: Arc<parking_lot::Mutex<Option<lazybox_gh::GhClient>>>,
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
        self.gh_client_cache.lock().clone()
    }

    /// Replace the cached GitHub client used across polling ticks.
    pub fn cache_gh_client(&self, client: lazybox_gh::GhClient) {
        *self.gh_client_cache.lock() = Some(client);
    }

    /// Report whether a reusable GitHub client is currently cached.
    pub fn has_cached_gh_client(&self) -> bool {
        self.gh_client_cache.lock().is_some()
    }

    /// Drop the cached GitHub client after an authentication failure.
    pub fn clear_cached_gh_client(&self) {
        *self.gh_client_cache.lock() = None;
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
}

impl SpawnCoordinator {
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
