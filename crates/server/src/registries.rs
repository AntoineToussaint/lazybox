use crate::{polling, terminal_io};
use lazybox_core::{SessionId, SessionKey};
use lazybox_ipc::{AgentRunAccess, AgentState, TerminalId, TerminalKind};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, Notify};

/// Typed inputs consumed by the per-terminal turn clock. Producers no longer
/// arbitrate hook-vs-PTY authority with unrelated wall clocks (#1169).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentTurnEvent {
    OutputChunk {
        backend_seq: u64,
        meaningful_progress: bool,
    },
    UserAnswered,
    HookArrived {
        waiting_on_user: bool,
    },
    Tick(AgentTurnTick),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentTurnTick {
    Quiet,
    Watchdog,
    Reclassify,
}

#[derive(Debug)]
struct AgentTurn {
    next_sequence: u64,
    last_output: Option<u64>,
    last_hook: Option<u64>,
    last_hook_waiting_on_user: bool,
    last_answer: Option<u64>,
    last_hook_at: Option<std::time::Instant>,
    quiet_after: Option<std::time::Duration>,
    quiet_deadline: Option<tokio::time::Instant>,
    watchdog_window: Option<std::time::Duration>,
    content_stable_since: tokio::time::Instant,
    watchdog_deadline: Option<tokio::time::Instant>,
    deadline_batch_remaining: Option<usize>,
    watchdog_ticks_since_hook: u8,
    last_output_at: tokio::time::Instant,
}

impl Default for AgentTurn {
    fn default() -> Self {
        let now = tokio::time::Instant::now();
        Self {
            next_sequence: 0,
            last_output: None,
            last_hook: None,
            last_hook_waiting_on_user: false,
            last_answer: None,
            last_hook_at: None,
            quiet_after: None,
            quiet_deadline: None,
            watchdog_window: None,
            content_stable_since: now,
            watchdog_deadline: None,
            deadline_batch_remaining: None,
            watchdog_ticks_since_hook: 0,
            last_output_at: now,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AgentTurnSchedule {
    pub quiet_deadline: Option<tokio::time::Instant>,
    pub watchdog_deadline: Option<tokio::time::Instant>,
    pub watchdog_due: bool,
    pub receiver_enabled: bool,
    pub last_output_at: tokio::time::Instant,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AgentTurnWatchdogFire {
    pub window: std::time::Duration,
    pub content_stable: std::time::Duration,
    pub elapsed_since_output: std::time::Duration,
}

impl AgentTurn {
    fn configure(
        &mut self,
        quiet_after: std::time::Duration,
        watchdog_window: Option<std::time::Duration>,
    ) {
        let now = tokio::time::Instant::now();
        self.quiet_after = Some(quiet_after);
        self.watchdog_window = watchdog_window;
        self.content_stable_since = now;
        self.watchdog_deadline = watchdog_window.map(|window| now + window);
        self.deadline_batch_remaining = None;
        self.last_output_at = now;
    }

    fn record(&mut self, event: AgentTurnEvent) -> u64 {
        self.next_sequence = self.next_sequence.saturating_add(1);
        let sequence = self.next_sequence;
        match event {
            AgentTurnEvent::OutputChunk {
                backend_seq,
                meaningful_progress,
            } => {
                let _ = backend_seq;
                self.last_output = Some(sequence);
                let now = tokio::time::Instant::now();
                self.last_output_at = now;
                self.quiet_deadline = self.quiet_after.map(|quiet| now + quiet);
                if meaningful_progress {
                    self.content_stable_since = now;
                    self.watchdog_deadline = self.watchdog_window.map(|window| now + window);
                    self.deadline_batch_remaining = None;
                }
            }
            AgentTurnEvent::UserAnswered => self.last_answer = Some(sequence),
            AgentTurnEvent::HookArrived { waiting_on_user } => {
                self.last_hook = Some(sequence);
                self.last_hook_waiting_on_user = waiting_on_user;
                self.last_hook_at = Some(std::time::Instant::now());
                self.watchdog_ticks_since_hook = 0;
            }
            AgentTurnEvent::Tick(tick) => {
                if tick == AgentTurnTick::Watchdog && self.last_hook.is_some() {
                    self.watchdog_ticks_since_hook =
                        self.watchdog_ticks_since_hook.saturating_add(1);
                }
            }
        }
        sequence
    }

    fn hook_authority(&self, pty_evidence: Option<u64>) -> lazybox_agents::HookAuthority {
        let Some(hook) = self.last_hook else {
            return lazybox_agents::HookAuthority::None;
        };
        let answer_supersedes_hook = self.last_answer.is_some_and(|answer| answer > hook);
        if self.last_hook_waiting_on_user
            && !answer_supersedes_hook
            && self.watchdog_ticks_since_hook < 2
        {
            return lazybox_agents::HookAuthority::HookNewer;
        }
        if pty_evidence.is_some_and(|pty| pty > hook) {
            lazybox_agents::HookAuthority::PtyNewer
        } else {
            lazybox_agents::HookAuthority::HookNewer
        }
    }

    fn prepare_schedule(&mut self, queued_chunks: usize) -> AgentTurnSchedule {
        let now = tokio::time::Instant::now();
        let watchdog_due = self
            .watchdog_deadline
            .is_some_and(|deadline| now >= deadline);
        if watchdog_due {
            self.deadline_batch_remaining.get_or_insert(queued_chunks);
        } else {
            self.deadline_batch_remaining = None;
        }
        AgentTurnSchedule {
            quiet_deadline: self.quiet_deadline,
            watchdog_deadline: self.watchdog_deadline,
            watchdog_due,
            receiver_enabled: !watchdog_due
                || self
                    .deadline_batch_remaining
                    .is_some_and(|remaining| remaining > 0),
            last_output_at: self.last_output_at,
        }
    }

    fn note_received(&mut self, watchdog_due: bool) {
        if watchdog_due && let Some(remaining) = &mut self.deadline_batch_remaining {
            *remaining = remaining.saturating_sub(1);
        }
    }

    fn fire_quiet(&mut self) -> std::time::Duration {
        self.quiet_deadline = None;
        tokio::time::Instant::now().saturating_duration_since(self.content_stable_since)
    }

    fn fire_watchdog(&mut self) -> Option<AgentTurnWatchdogFire> {
        let now = tokio::time::Instant::now();
        let window = self.watchdog_window?;
        self.watchdog_deadline = Some(now + window);
        self.deadline_batch_remaining = None;
        Some(AgentTurnWatchdogFire {
            window,
            content_stable: now.saturating_duration_since(self.content_stable_since),
            elapsed_since_output: now.saturating_duration_since(self.last_output_at),
        })
    }
}

/// Every mutable fact about one terminal. A terminal is inserted, observed,
/// marked finishing, and removed under the registry's single lock; readers can
/// no longer see a backend mapping disappear while stale metadata survives in
/// another map (#1168).
#[derive(Default)]
pub(crate) struct TerminalEntry {
    pub backend_key: Option<String>,
    pub session_id: Option<SessionId>,
    pub agent_state: Option<AgentState>,
    pub agent_state_generation: Option<u64>,
    pub meta: Option<(SessionKey, TerminalKind)>,
    pub access: AgentRunAccess,
    pub no_permission: bool,
    pub on_main: bool,
    pub model_label: Option<String>,
    pub superseded: bool,
    pub authenticating: bool,
    pub outdated_agent: bool,
    pub agent_detect_reset: bool,
    turn: AgentTurn,
    pub input_needed_shape: Option<lazybox_agents::PromptShape>,
    pub reclassify_request: Option<Arc<Notify>>,
    pub activity: terminal_io::AgentTerminalActivity,
    /// Teardown has atomically claimed this entry. It stays present long
    /// enough to emit Exited from its metadata/state, but live routing and
    /// reconnect snapshots treat it as absent.
    pub finishing: bool,
}

/// Immutable facts captured by the atomic teardown claim.
pub(crate) struct TerminalTeardownClaim {
    pub meta: Option<(SessionKey, TerminalKind)>,
    pub access: AgentRunAccess,
    pub authenticating: bool,
    pub agent_state_generation: Option<u64>,
}

/// Per-terminal process state shared by terminal lifecycle handlers.
#[derive(Clone, Default)]
pub struct TerminalRegistry {
    pub(crate) entries: Arc<Mutex<HashMap<TerminalId, TerminalEntry>>>,
    /// Per-backend serialization primitives outlive a registry entry briefly
    /// while teardown drains writers/persistence, so they remain keyed by the
    /// backend session rather than stored inside the terminal entry.
    terminal_persistence_locks: Arc<parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    terminal_io_locks: Arc<parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

pub(crate) struct TerminalRegistrationGuard {
    entries: tokio::sync::OwnedMutexGuard<HashMap<TerminalId, TerminalEntry>>,
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
        let entry = self.entries.entry(id).or_default();
        entry.backend_key = Some(backend_key);
        entry.meta = Some((session_key, kind));
        entry.agent_state_generation = generation;
        entry.finishing = false;
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
        if let Some(old) = self.entries.get_mut(&old_id)
            && old.backend_key.is_some()
        {
            old.superseded = true;
        }
        self.register(id, backend_key, session_key, kind, generation);
        if authenticating {
            self.entries.entry(id).or_default().authenticating = true;
        }
    }
}

pub(crate) struct RecoveredTerminalRegistrationGuard {
    registration: TerminalRegistrationGuard,
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
        recovered_agent.and_then(|(_, state)| {
            self.registration
                .entries
                .get_mut(&id)
                .and_then(|entry| entry.agent_state.replace(state))
        })
    }
}

impl TerminalRegistry {
    pub(crate) async fn metadata_map(&self) -> HashMap<TerminalId, (SessionKey, TerminalKind)> {
        self.entries
            .lock()
            .await
            .iter()
            .filter(|(_, entry)| !entry.finishing)
            .filter_map(|(id, entry)| entry.meta.clone().map(|meta| (*id, meta)))
            .collect()
    }

    pub(crate) async fn session_bindings(&self) -> HashMap<TerminalId, SessionId> {
        self.entries
            .lock()
            .await
            .iter()
            .filter_map(|(id, entry)| entry.session_id.map(|session| (*id, session)))
            .collect()
    }

    pub(crate) async fn agent_state_map(&self) -> HashMap<TerminalId, AgentState> {
        self.entries
            .lock()
            .await
            .iter()
            .filter_map(|(id, entry)| entry.agent_state.map(|state| (*id, state)))
            .collect()
    }

    #[cfg(test)]
    pub(crate) async fn insert_meta(
        &self,
        id: TerminalId,
        session_key: SessionKey,
        kind: TerminalKind,
    ) {
        self.entries.lock().await.entry(id).or_default().meta = Some((session_key, kind));
    }

    #[cfg(test)]
    pub(crate) async fn model_label_for(&self, id: TerminalId) -> Option<String> {
        self.entries
            .lock()
            .await
            .get(&id)
            .and_then(|entry| entry.model_label.clone())
    }

    pub(crate) async fn input_needed_shape_for(
        &self,
        id: TerminalId,
    ) -> Option<lazybox_agents::PromptShape> {
        self.entries
            .lock()
            .await
            .get(&id)
            .and_then(|entry| entry.input_needed_shape)
    }

    pub(crate) async fn has_detect_reset(&self, id: TerminalId) -> bool {
        self.entries
            .lock()
            .await
            .get(&id)
            .is_some_and(|entry| entry.agent_detect_reset)
    }

    pub(crate) async fn mark_detect_reset(&self, id: TerminalId) {
        self.entries
            .lock()
            .await
            .entry(id)
            .or_default()
            .agent_detect_reset = true;
    }

    pub(crate) async fn take_detect_reset(&self, id: TerminalId) -> bool {
        self.entries
            .lock()
            .await
            .get_mut(&id)
            .is_some_and(|entry| std::mem::take(&mut entry.agent_detect_reset))
    }

    /// Bind a wire terminal id to a backend session for I/O routing.
    pub async fn bind_backend(&self, id: TerminalId, backend_key: String) {
        self.entries.lock().await.entry(id).or_default().backend_key = Some(backend_key);
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
        self.entries
            .lock()
            .await
            .remove(&id)
            .and_then(|entry| entry.backend_key)
    }

    /// Snapshot the backend key without holding the map lock in the caller.
    pub async fn backend_key_for(&self, id: TerminalId) -> Option<String> {
        self.entries
            .lock()
            .await
            .get(&id)
            .filter(|entry| !entry.finishing)
            .and_then(|entry| entry.backend_key.clone())
    }

    /// Snapshot terminal metadata without holding the map lock in the caller.
    pub async fn terminal_meta_for(&self, id: TerminalId) -> Option<(SessionKey, TerminalKind)> {
        self.entries
            .lock()
            .await
            .get(&id)
            .filter(|entry| !entry.finishing)
            .and_then(|entry| entry.meta.clone())
    }

    /// Snapshot the cached agent state without holding the map lock in the caller.
    pub async fn agent_state_for(&self, id: TerminalId) -> Option<AgentState> {
        self.entries
            .lock()
            .await
            .get(&id)
            .and_then(|entry| entry.agent_state)
    }

    /// Snapshot the durable workspace session associated with a terminal.
    pub async fn terminal_session_for(&self, id: TerminalId) -> Option<SessionId> {
        self.entries
            .lock()
            .await
            .get(&id)
            .and_then(|entry| entry.session_id)
    }

    pub(crate) async fn access_for(&self, id: TerminalId) -> AgentRunAccess {
        self.entries
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.access)
            .unwrap_or_default()
    }

    pub(crate) async fn record_access(&self, id: TerminalId, access: AgentRunAccess) {
        if access != AgentRunAccess::Default {
            self.entries.lock().await.entry(id).or_default().access = access;
        }
    }

    /// Associate a live terminal with its durable workspace session.
    pub async fn associate_session(&self, id: TerminalId, session_id: SessionId) {
        self.entries.lock().await.entry(id).or_default().session_id = Some(session_id);
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
            self.entries.lock().await.entry(id).or_default().session_id = Some(session_id);
        }
        let mut entries = self.entries.lock().await;
        let entry = entries.entry(id).or_default();
        entry.access = access;
        entry.no_permission = no_permission;
        entry.on_main = on_main;
        entry.model_label = model_label.map(str::to_owned);
    }

    /// Record the last durable state emitted for an agent terminal.
    pub async fn record_agent_state(&self, id: TerminalId, state: AgentState) {
        self.entries.lock().await.entry(id).or_default().agent_state = Some(state);
    }

    /// Record the process generation used to reject stale recovered state.
    pub async fn record_agent_state_generation(&self, id: TerminalId, generation: u64) {
        self.entries
            .lock()
            .await
            .entry(id)
            .or_default()
            .agent_state_generation = Some(generation);
    }

    pub(crate) async fn record_turn_event(&self, id: TerminalId, event: AgentTurnEvent) -> u64 {
        self.entries
            .lock()
            .await
            .entry(id)
            .or_default()
            .turn
            .record(event)
    }

    pub(crate) async fn configure_agent_turn(
        &self,
        id: TerminalId,
        quiet_after: std::time::Duration,
        watchdog_window: Option<std::time::Duration>,
    ) {
        self.entries
            .lock()
            .await
            .entry(id)
            .or_default()
            .turn
            .configure(quiet_after, watchdog_window);
    }

    pub(crate) async fn agent_turn_schedule(
        &self,
        id: TerminalId,
        queued_chunks: usize,
    ) -> AgentTurnSchedule {
        self.entries
            .lock()
            .await
            .entry(id)
            .or_default()
            .turn
            .prepare_schedule(queued_chunks)
    }

    pub(crate) async fn note_agent_turn_received(&self, id: TerminalId, watchdog_due: bool) {
        if let Some(entry) = self.entries.lock().await.get_mut(&id) {
            entry.turn.note_received(watchdog_due);
        }
    }

    pub(crate) async fn fire_agent_turn_quiet(&self, id: TerminalId) -> std::time::Duration {
        self.entries
            .lock()
            .await
            .entry(id)
            .or_default()
            .turn
            .fire_quiet()
    }

    pub(crate) async fn fire_agent_turn_watchdog(
        &self,
        id: TerminalId,
    ) -> Option<AgentTurnWatchdogFire> {
        self.entries
            .lock()
            .await
            .get_mut(&id)
            .and_then(|entry| entry.turn.fire_watchdog())
    }

    pub(crate) async fn latest_output_turn_event(&self, id: TerminalId) -> Option<u64> {
        self.entries
            .lock()
            .await
            .get(&id)
            .and_then(|entry| entry.turn.last_output)
    }

    pub(crate) async fn hook_authority_for(
        &self,
        id: TerminalId,
        pty_evidence: Option<u64>,
    ) -> lazybox_agents::HookAuthority {
        self.entries
            .lock()
            .await
            .get(&id)
            .map_or(lazybox_agents::HookAuthority::None, |entry| {
                entry.turn.hook_authority(pty_evidence)
            })
    }

    /// Compatibility seam for integration fixtures that predate the causal
    /// turn clock. Production hook ingest records `AgentTurnEvent::HookArrived`
    /// directly; an intentionally stale fixture is represented by a newer PTY
    /// event, preserving its old test meaning without using time for runtime
    /// arbitration.
    pub async fn record_hook_activity(&self, id: TerminalId, at: std::time::Instant) {
        let mut entries = self.entries.lock().await;
        let clock = &mut entries.entry(id).or_default().turn;
        clock.record(AgentTurnEvent::HookArrived {
            waiting_on_user: false,
        });
        clock.last_hook_at = Some(at);
        if at.elapsed() >= lazybox_agents::HOOK_STALENESS {
            clock.record(AgentTurnEvent::OutputChunk {
                backend_seq: 0,
                meaningful_progress: false,
            });
        }
    }

    pub async fn hook_activity_for(&self, id: TerminalId) -> Option<std::time::Instant> {
        self.entries
            .lock()
            .await
            .get(&id)
            .and_then(|entry| entry.turn.last_hook_at)
    }

    /// Record the input shape currently presented by an agent.
    pub async fn record_input_needed_shape(
        &self,
        id: TerminalId,
        shape: lazybox_agents::PromptShape,
    ) {
        self.entries
            .lock()
            .await
            .entry(id)
            .or_default()
            .input_needed_shape = Some(shape);
    }

    /// Register the reclassify poke channel a terminal's PTY pump listens on,
    /// returning the `Notify` the pump selects against. Kept in a shared map so
    /// a deferred inject can find it by terminal id (issue #869).
    pub(crate) async fn register_reclassify(&self, id: TerminalId) -> Arc<Notify> {
        let notify = Arc::new(Notify::new());
        self.entries
            .lock()
            .await
            .entry(id)
            .or_default()
            .reclassify_request = Some(notify.clone());
        notify
    }

    /// Ask a terminal's PTY pump to re-read the live screen now. A no-op when
    /// no pump is registered (a recovered/shell terminal, or a unit test with
    /// no pump) — the caller falls back to its own cached-state re-check.
    pub(crate) async fn request_reclassify(&self, id: TerminalId) {
        if let Some(notify) = self
            .entries
            .lock()
            .await
            .get(&id)
            .and_then(|entry| entry.reclassify_request.clone())
        {
            notify.notify_one();
        }
    }

    /// Report whether any registered agent is currently working.
    pub async fn any_agent_working(&self) -> bool {
        self.entries
            .lock()
            .await
            .values()
            .any(|entry| !entry.finishing && matches!(entry.agent_state, Some(AgentState::Working)))
    }

    /// Return the number of live terminal-to-backend registrations.
    pub async fn terminal_count(&self) -> usize {
        self.entries
            .lock()
            .await
            .values()
            .filter(|entry| !entry.finishing && entry.backend_key.is_some())
            .count()
    }

    /// Snapshot the ids of all live terminal-to-backend registrations.
    pub async fn terminal_ids(&self) -> Vec<TerminalId> {
        self.entries
            .lock()
            .await
            .iter()
            .filter_map(|(id, entry)| {
                (!entry.finishing && entry.backend_key.is_some()).then_some(*id)
            })
            .collect()
    }

    /// Report whether any terminal-to-backend registration is live.
    pub async fn is_empty(&self) -> bool {
        self.terminal_count().await == 0
    }

    /// Snapshot reconnect metadata without exposing the registry's lock.
    pub async fn terminal_metadata(&self) -> Vec<(TerminalId, SessionKey, TerminalKind)> {
        self.entries
            .lock()
            .await
            .iter()
            .filter(|(_, entry)| !entry.finishing)
            .filter_map(|(id, entry)| {
                entry
                    .meta
                    .as_ref()
                    .map(|(session_key, kind)| (*id, session_key.clone(), kind.clone()))
            })
            .collect()
    }

    /// The live agent terminal a workspace-addressed injection or
    /// output read should target: the lowest-id `Agent` terminal
    /// registered for `session_key`, mirroring the TUI's
    /// `agent_terminal_for` tie-break. `None` when the workspace has no
    /// running agent (only shells, or nothing at all).
    pub async fn running_agent_terminal(&self, session_key: &SessionKey) -> Option<TerminalId> {
        self.entries
            .lock()
            .await
            .iter()
            .filter(|(_, entry)| !entry.finishing)
            .filter(|(_, entry)| {
                entry.meta.as_ref().is_some_and(|(owner, kind)| {
                    owner == session_key && matches!(kind, TerminalKind::Agent(_))
                })
            })
            .map(|(id, _)| *id)
            .min_by_key(|id| id.0)
    }

    pub(crate) async fn agent_terminals_for_review(
        &self,
        session_key: &SessionKey,
        session_id: Option<SessionId>,
    ) -> Vec<TerminalId> {
        let entries = self.entries.lock().await;
        let mut ids = entries
            .iter()
            .filter_map(|(terminal_id, entry)| {
                if entry.finishing {
                    return None;
                }
                let (owner, kind) = entry.meta.as_ref()?;
                if owner != session_key || !matches!(kind, TerminalKind::Agent(_)) {
                    return None;
                }
                if session_id.is_some_and(|expected| entry.session_id != Some(expected)) {
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
        self.entries
            .lock()
            .await
            .get(&id)
            .is_some_and(|entry| entry.outdated_agent)
    }

    /// Mark a recovered agent as requiring a compatibility restart.
    pub async fn mark_outdated_agent(&self, id: TerminalId) {
        self.entries
            .lock()
            .await
            .entry(id)
            .or_default()
            .outdated_agent = true;
    }

    /// Return the number of recovered agents requiring a compatibility restart.
    pub async fn outdated_agent_count(&self) -> usize {
        self.entries
            .lock()
            .await
            .values()
            .filter(|entry| entry.outdated_agent)
            .count()
    }

    /// Report whether teardown removed every per-terminal bookkeeping entry.
    pub async fn bookkeeping_is_empty(&self) -> bool {
        self.entries.lock().await.is_empty()
    }

    /// Atomically make a terminal non-live while retaining the metadata needed
    /// to emit its final lifecycle state. A second teardown sees `None`; a key
    /// mismatch is returned without touching the entry.
    pub(crate) async fn claim_teardown(
        &self,
        id: TerminalId,
        expected_backend_key: &str,
    ) -> Result<Option<TerminalTeardownClaim>, String> {
        let mut entries = self.entries.lock().await;
        let Some(entry) = entries.get_mut(&id) else {
            return Ok(None);
        };
        let Some(actual) = entry.backend_key.as_deref() else {
            return Ok(None);
        };
        if actual != expected_backend_key {
            return Err(actual.to_string());
        }
        if entry.finishing {
            return Ok(None);
        }
        entry.finishing = true;
        Ok(Some(TerminalTeardownClaim {
            meta: entry.meta.clone(),
            access: entry.access,
            authenticating: entry.authenticating,
            agent_state_generation: entry.agent_state_generation,
        }))
    }

    /// Complete a claimed teardown with one atomic removal.
    pub(crate) async fn finish_teardown(&self, id: TerminalId) {
        self.entries.lock().await.remove(&id);
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
        TerminalRegistrationGuard {
            entries: self.entries.clone().lock_owned().await,
        }
    }

    pub(crate) async fn lock_recovered_registration(&self) -> RecoveredTerminalRegistrationGuard {
        RecoveredTerminalRegistrationGuard {
            registration: self.lock_registration().await,
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
    /// Set only by an explicit user refresh (`Command::Refresh`) to force
    /// providers that run on their own slow cadence (Linear) to poll this
    /// tick. Kept separate from `poll_warm_requested`, which the many
    /// post-mutation / subscribe `wake(true)` calls also set — folding
    /// Linear's force into that flag would re-poll Linear on every GitHub
    /// mutation, defeating its decoupled cadence (#1032).
    poll_force_refresh_requested: Arc<AtomicBool>,
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

    /// Request that the next tick force every slow-cadence provider
    /// (Linear) to poll now, regardless of its own schedule. For an
    /// explicit user refresh only — kept separate from the warm-poll flag
    /// so post-mutation/subscribe wakes don't force Linear (#1032).
    pub fn request_force_refresh(&self) {
        self.poll_force_refresh_requested
            .store(true, Ordering::Release);
    }

    pub(crate) fn take_force_refresh(&self) -> bool {
        self.poll_force_refresh_requested
            .swap(false, Ordering::AcqRel)
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
        terminals
            .record_turn_event(
                TerminalId(1),
                AgentTurnEvent::HookArrived {
                    waiting_on_user: false,
                },
            )
            .await;
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
            terminals.hook_authority_for(TerminalId(1), None).await,
            lazybox_agents::HookAuthority::HookNewer
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

    /// #1168: registration and teardown publish one complete terminal entry.
    /// A reconnect reader racing heavy lifecycle churn may observe the old
    /// entry, the finishing entry, or no entry, but never a backend binding
    /// whose metadata/state was already swept from a parallel map.
    #[tokio::test]
    async fn terminal_entry_is_atomic_during_registration_and_teardown_churn() {
        let terminals = TerminalRegistry::default();
        let reader_registry = terminals.clone();
        let done = Arc::new(AtomicBool::new(false));
        let reader_done = done.clone();
        let reader = tokio::spawn(async move {
            while !reader_done.load(Ordering::Acquire) {
                let entries = reader_registry.entries.lock().await;
                if let Some(entry) = entries.get(&TerminalId(1168)) {
                    assert!(entry.backend_key.is_some(), "published entry has routing");
                    assert!(entry.meta.is_some(), "published entry has metadata");
                }
                drop(entries);
                tokio::task::yield_now().await;
            }
        });

        for generation in 1..=1_000 {
            {
                let mut registration = terminals.lock_registration().await;
                registration.register(
                    TerminalId(1168),
                    format!("backend-{generation}"),
                    SessionKey::from("test:atomic-terminal"),
                    TerminalKind::Agent("codex".into()),
                    Some(generation),
                );
            }
            let expected = format!("backend-{generation}");
            let claim = terminals
                .claim_teardown(TerminalId(1168), &expected)
                .await
                .expect("matching backend")
                .expect("one teardown owner");
            assert!(claim.meta.is_some());
            terminals.finish_teardown(TerminalId(1168)).await;
        }
        done.store(true, Ordering::Release);
        reader.await.expect("reader task");
        assert!(terminals.bookkeeping_is_empty().await);
    }

    #[tokio::test]
    async fn reclassify_request_pokes_a_registered_pump_and_no_ops_otherwise() {
        let registry = TerminalRegistry::default();
        let id = TerminalId(869);
        // No pump registered yet: a poke must be a harmless no-op, so a
        // deferred inject falls back to its own cached-state re-check.
        registry.request_reclassify(id).await;

        let notify = registry.register_reclassify(id).await;
        // A poke stores a permit even with no waiter parked yet, so the pump's
        // next `notified()` returns immediately (no lost reclassify).
        registry.request_reclassify(id).await;
        tokio::time::timeout(std::time::Duration::from_millis(200), notify.notified())
            .await
            .expect("a registered reclassify channel must receive the poke");

        // Teardown drops the registration; a later poke is a no-op again.
        registry.remove_terminal(id).await;
        assert!(registry.bookkeeping_is_empty().await);
        registry.request_reclassify(id).await;
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
