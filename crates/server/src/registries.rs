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
    pub terminals: Arc<Mutex<HashMap<TerminalId, String>>>,
    /// Owning durable session used to freeze one session during worktree moves.
    pub terminal_sessions: Arc<Mutex<HashMap<TerminalId, SessionId>>>,
    /// Last durable state broadcast for each agent terminal.
    pub agent_states: Arc<Mutex<HashMap<TerminalId, AgentState>>>,
    /// Process generation that prevents recovered state bleeding into a reused key.
    pub agent_state_generations: Arc<Mutex<HashMap<TerminalId, u64>>>,
    /// Workspace session and kind used to rebuild reconnect snapshots.
    pub terminal_meta: Arc<Mutex<HashMap<TerminalId, (SessionKey, TerminalKind)>>>,
    /// Reconnect-visible marker for terminals with permission prompts bypassed.
    pub no_permission_terminals: Arc<Mutex<HashSet<TerminalId>>>,
    /// Distinguishes shared-main agents from isolated-worktree singletons.
    pub on_main_terminals: Arc<Mutex<HashSet<TerminalId>>>,
    /// Model-tier label replayed in reconnect snapshots.
    pub terminal_models: Arc<Mutex<HashMap<TerminalId, String>>>,
    /// Recovered agents that require restart for the current PTY compatibility generation.
    pub outdated_agent_terminals: Arc<Mutex<HashSet<TerminalId>>>,
    /// Detection buffers to clear after an answer so stale prompt chrome cannot re-fire.
    pub agent_detect_resets: Arc<Mutex<HashSet<TerminalId>>>,
    /// Latest structured hook arrival, used to fall back to PTY detection when hooks go stale.
    pub hook_driven_terminals: Arc<Mutex<HashMap<TerminalId, std::time::Instant>>>,
    /// Distinguishes one-key chooser answers from free-text input requests.
    pub input_needed_shapes: Arc<Mutex<HashMap<TerminalId, lazybox_agents::PromptShape>>>,
    /// Prevents a delayed prompt write from resurrecting state after teardown.
    terminal_persistence_locks: Arc<parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Prevents concurrent keyboard, chat, and injection writers corrupting a PTY stream.
    terminal_io_locks: Arc<parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl TerminalRegistry {
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
}

/// Cross-tick polling state and caches.
///
/// The fields keep their separate lock domains because the upsert path must
/// not re-enter the cross-tick `tick_state` lock through merge prompt or
/// auto-merge bookkeeping.
#[derive(Clone, Default)]
pub struct PollState {
    /// Provider-error debounce and prompt memory shared by refresh and the poll loop.
    pub tick_state: Arc<Mutex<polling::TickState>>,
    /// Bounded engagement set written by focus changes and read by poll scheduling.
    pub engagement: Arc<parking_lot::RwLock<polling::PollEngagement>>,
    /// Long-lived GitHub client whose shared rate budget must survive across ticks.
    pub gh_client_cache: Arc<parking_lot::Mutex<Option<lazybox_gh::GhClient>>>,
    /// Issue-to-PR prompt dedupe kept outside `tick_state` to avoid upsert re-entry.
    pub merge_prompts: Arc<Mutex<polling::MergePromptMemory>>,
    /// Auto-merge latches kept outside `tick_state` because commit paths update them.
    pub auto_merge: Arc<parking_lot::Mutex<polling::AutoMergeMemory>>,
    /// Removal prompt memory kept outside `tick_state` because upsert paths update it.
    pub removal_prompts: Arc<Mutex<polling::RemovalPromptMemory>>,
    /// Authenticated logins replayed to reconnecting clients.
    pub viewer_identities: Arc<parking_lot::Mutex<Vec<(String, String)>>>,
    /// Interrupts the poll sleep for refresh, reconnect, and lazy mergeable retries.
    pub wake_signal: Arc<Notify>,
    poll_warm_requested: Arc<AtomicBool>,
}

impl PollState {
    pub(crate) fn wake(&self, poll_notifications: bool) {
        if poll_notifications {
            self.poll_warm_requested.store(true, Ordering::Release);
        }
        self.wake_signal.notify_one();
    }

    pub(crate) fn take_warm_request(&self) -> bool {
        self.poll_warm_requested.swap(false, Ordering::AcqRel)
    }
}

/// Synchronization state for terminal spawn and prompt injection.
#[derive(Clone, Default)]
pub struct SpawnCoordinator {
    /// Lets an inject task verify submit via the structured hook and retry Enter once.
    pub prompt_submit_signals: Arc<Mutex<HashMap<TerminalId, Arc<Notify>>>>,
    /// Enforces one readiness-gated injection per terminal.
    pub pending_prompt_injections: Arc<parking_lot::Mutex<HashSet<TerminalId>>>,
    /// Closes the provisioning gap before terminal maps can enforce singleton spawns.
    pub inflight_spawns: Arc<parking_lot::Mutex<HashMap<(String, String), Arc<Notify>>>>,
    /// Wakes duplicate spawns and teardown without busy-polling.
    pub inflight_spawn_changed: Arc<Notify>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registries_are_independently_constructible() {
        let terminals = TerminalRegistry::default();
        terminals
            .terminals
            .lock()
            .await
            .insert(TerminalId(1), "backend".to_string());

        let polling = PollState::default();
        polling.wake(true);

        let spawns = SpawnCoordinator::default();
        spawns
            .pending_prompt_injections
            .lock()
            .insert(TerminalId(2));

        assert_eq!(
            terminals.backend_key_for(TerminalId(1)).await.as_deref(),
            Some("backend")
        );
        assert!(polling.take_warm_request());
        assert!(
            spawns
                .pending_prompt_injections
                .lock()
                .contains(&TerminalId(2))
        );
    }

    #[tokio::test]
    async fn cloned_registry_shares_its_lock_domains() {
        let registry = TerminalRegistry::default();
        let clone = registry.clone();
        registry
            .terminals
            .lock()
            .await
            .insert(TerminalId(1), "backend".to_string());

        assert_eq!(
            clone.backend_key_for(TerminalId(1)).await.as_deref(),
            Some("backend")
        );
    }
}
