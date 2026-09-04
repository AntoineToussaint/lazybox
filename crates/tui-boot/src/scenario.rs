//! `lazybox --demo` — a scripted **scenario-injection harness** that drives
//! the full UI from synthetic daemon events, with no PTY, no git worktrees,
//! no GitHub. It boots the real in-process daemon (so the Subscribe →
//! Snapshot handshake and the bus → client relay are exactly the production
//! paths) and then acts as a *second producer* on the process-wide event
//! bus, publishing a timed sequence of [`Event`]s.
//!
//! ## Why this exists
//!
//! Two jobs, one mechanism:
//!
//! 1. **Deterministic demos / screenshots.** A recorded VHS cast needs the
//!    inbox to come alive — agents working, terminals streaming, CI flipping
//!    — without credentials or a live fleet. A pure-bus producer replays a
//!    scripted timeline that renders identically every run.
//! 2. **Interface-completeness review.** If the whole UI can be brought to
//!    life by `bus.send(Event)` alone, the daemon→client `Event` contract is
//!    complete. Where it *can't* be (see the known gaps below), that's a real
//!    interface gap worth fixing — this harness is where such gaps surface.
//!
//! ## The one hard constraint: terminal sequence numbers
//!
//! The client terminal is a self-contained VT emulator; it renders injected
//! [`Event::TerminalOutput`] bytes with no PTY behind it. But `append_output`
//! requires each chunk's `first_seq == last_seq + 1` — a gap flips the slot
//! to *Desynced*, drops the bytes, and asks the daemon to resync. So the
//! driver keeps a per-terminal monotonic counter and assigns contiguous
//! seqs; scenario authors never hand-count.
//!
//! ## Two tiers (and the interface gaps Tier 2 closes)
//!
//! [`Stage::BusOnly`] is the pure-bus producer: it drives the whole *passive*
//! UI from `bus.send` alone. It has two inherent gaps, and characterizing them
//! is the whole point of the review:
//!
//! - **Terminal input.** A bus-injected terminal has no backend session, so a
//!   keystroke resolves to `BackendError::NotFound`. Playback-only.
//! - **Recovery durability.** A `Snapshot` (initial, or after a broadcast
//!   `Lagged`) rebuilds terminals from the daemon's registry, not client VT
//!   state, so bus-only terminals vanish on recovery. Workspaces survive
//!   (they live in the store); fake terminals do not.
//!
//! [`Stage::Backed`] closes both by spawning terminals the *real* way — a
//! genuine `Command::Spawn` through `dispatch_command` against a
//! [`MockBackend`]. The terminal is registered in the daemon's
//! `TerminalRegistry` (so it survives recovery) and its input lane resolves to
//! a real backend session (so keystrokes are accepted, not rejected). Output
//! is fed via `MockBackend::emit`, so the daemon's own pump owns the sequence
//! numbers. `--demo` uses `Backed`; it is a full interactive integration-test
//! surface, not just a screenshot reel.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use lazybox_core::{
    CiStatus, Label, Mergeable, ReviewStatus, SessionKey, SessionKind, Task, TaskId, TaskRole,
    TaskState, Workspace, WorkspaceSession,
};
use lazybox_ipc::{AgentState, Event, TerminalId, TerminalKind};
use lazybox_server::ServerConfig;
use lazybox_server::backend::MockBackend;
use lazybox_store::{MemoryStore, Store, WorkspaceRecord};
use std::path::PathBuf;
use tempfile::TempDir;
use tokio::sync::{broadcast, mpsc};

/// A seeded workspace plus the handles the scenario script needs to address
/// it: its `SessionKey` (for `AgentState` / terminal events), its display
/// repo, and the built `Workspace` itself so a scenario can re-upsert a
/// mutated copy (e.g. flip CI red → green mid-demo).
pub struct SeededWorkspace {
    pub key: SessionKey,
    pub repo: String,
    pub workspace: Arc<Workspace>,
}

/// Built artifacts of the demo fixture: the throwaway repo dir, the seeded
/// in-memory store the daemon serves, the client-side snippet catalog, and
/// the ordered roster of seeded workspaces the scenario drives.
pub struct DemoFixture {
    /// Owns the temp dir — drop = delete.
    pub repo: TempDir,
    pub store: Arc<dyn Store>,
    pub snippets: lazybox_config::Snippets,
    pub workspaces: Vec<SeededWorkspace>,
}

/// Parameters for one seeded workspace. Deliberately small — the scenario
/// varies only the handful of `Task` fields that change what the sidebar and
/// activity pane render.
struct WsSpec {
    repo: &'static str,
    number: u64,
    title: &'static str,
    is_pr: bool,
    ci: CiStatus,
    review: ReviewStatus,
    unread: u32,
    mergeable: Mergeable,
    behind: bool,
    labels: &'static [&'static str],
}

impl DemoFixture {
    /// Seed a multi-repo, multi-owner inbox into a fresh `MemoryStore`. The
    /// owners (`obin-ai/*`, `acme/*`, `personal/*`) auto-group into Spaces;
    /// the varied CI / unread / review states give the filter menu and
    /// sidebar something real to render. Every workspace gets a shell session
    /// rooted at the shared throwaway repo so it has on-disk presence.
    pub fn seed() -> anyhow::Result<Self> {
        let repo = tempfile::Builder::new()
            .prefix("lazybox-demo-")
            .tempdir()
            .map_err(|e| anyhow::anyhow!("create tempdir: {e}"))?;
        crate::test_mode::run_git_init(repo.path())?;

        let store = Arc::new(MemoryStore::new()) as Arc<dyn Store>;
        crate::test_mode::seed_skip_setup(&*store)?;

        let specs = [
            WsSpec {
                repo: "obin-ai/lazybox",
                number: 1332,
                title: "probe author:@me to surface self/agent PRs fast",
                is_pr: true,
                ci: CiStatus::Success,
                review: ReviewStatus::Approved,
                unread: 0,
                mergeable: Mergeable::Mergeable,
                behind: false,
                labels: &["backend"],
            },
            WsSpec {
                repo: "obin-ai/lazybox",
                number: 1340,
                title: "footer notices auto-fade on a severity timer",
                is_pr: true,
                ci: CiStatus::Failure,
                review: ReviewStatus::ChangesRequested,
                unread: 3,
                mergeable: Mergeable::Mergeable,
                behind: true,
                labels: &["ui", "bug"],
            },
            WsSpec {
                repo: "obin-ai/web",
                number: 88,
                title: "refresh landing page for v0.1.13",
                is_pr: true,
                ci: CiStatus::Running,
                review: ReviewStatus::None,
                unread: 1,
                mergeable: Mergeable::Mergeable,
                behind: false,
                labels: &["web"],
            },
            WsSpec {
                repo: "acme/api",
                number: 210,
                title: "token-bucket rate limiter for the gateway",
                is_pr: true,
                ci: CiStatus::Success,
                review: ReviewStatus::Approved,
                unread: 0,
                mergeable: Mergeable::Mergeable,
                behind: false,
                labels: &["perf"],
            },
            WsSpec {
                repo: "acme/api",
                number: 211,
                title: "cache resolved credentials across polls",
                is_pr: false,
                ci: CiStatus::None,
                review: ReviewStatus::None,
                unread: 2,
                mergeable: Mergeable::Unknown,
                behind: false,
                labels: &["enhancement"],
            },
            WsSpec {
                repo: "acme/cli",
                number: 45,
                title: "shell tab-completion for the `acme` binary",
                is_pr: true,
                ci: CiStatus::Success,
                review: ReviewStatus::Pending,
                unread: 4,
                mergeable: Mergeable::Conflicting,
                behind: true,
                labels: &["cli"],
            },
            WsSpec {
                repo: "personal/dotfiles",
                number: 7,
                title: "faster zsh prompt with async git status",
                is_pr: true,
                ci: CiStatus::Success,
                review: ReviewStatus::None,
                unread: 1,
                mergeable: Mergeable::Mergeable,
                behind: false,
                labels: &[],
            },
        ];

        let mut workspaces = Vec::new();
        for spec in specs {
            let ws = build_workspace(&spec, repo.path());
            let key = SessionKey::from(&ws.key);
            let json = serde_json::to_string(&ws)?;
            store
                .save_workspace(&WorkspaceRecord {
                    key: ws.key.as_str().to_string(),
                    created_at: ws.created_at,
                    workspace_json: Some(json),
                })
                .map_err(|e| anyhow::anyhow!("save_workspace: {e}"))?;
            workspaces.push(SeededWorkspace {
                key,
                repo: spec.repo.to_string(),
                workspace: Arc::new(ws),
            });
        }

        Ok(Self {
            repo,
            store,
            snippets: lazybox_config::Snippets::builtin(),
            workspaces,
        })
    }
}

/// Construct one synthetic PR/issue `Workspace` from a spec, rooted at the
/// shared throwaway repo. Mirrors `test_mode::seed_one_session`'s shape so
/// the classifier treats it as a real PR/issue with a usable worktree.
fn build_workspace(spec: &WsSpec, worktree: &Path) -> Workspace {
    let kind_seg = if spec.is_pr { "pull" } else { "issues" };
    let task = Task {
        author: "octo-agent".into(),
        id: TaskId {
            source: "github".into(),
            key: format!("{}#{}", spec.repo, spec.number),
        },
        title: spec.title.into(),
        body: Some(format!(
            "Synthetic {} seeded by `lazybox --demo`.",
            if spec.is_pr { "pull request" } else { "issue" }
        )),
        state: TaskState::Open,
        role: TaskRole::Author,
        ci: spec.ci,
        review: spec.review,
        checks: vec![],
        unread_count: spec.unread,
        url: format!(
            "https://github.com/{}/{}/{}",
            spec.repo, kind_seg, spec.number
        ),
        repo: Some(spec.repo.into()),
        branch: Some(format!("feature/{}", spec.number)),
        base_branch: Some("main".into()),
        updated_at: Utc::now(),
        created_at: None,
        closed_at: None,
        labels: spec.labels.iter().map(|l| Label::new(*l)).collect(),
        reviewers: vec![],
        reviews: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: spec.mergeable,
        is_behind_base: spec.behind,
        merge_blocked: matches!(spec.mergeable, Mergeable::Conflicting),
        approval_policy: Default::default(),
        node_id: None,
        needs_reply: spec.unread > 0,
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
    };
    let mut workspace = Workspace::from_task(task, Utc::now());
    let now = Utc::now();
    workspace.add_session(WorkspaceSession::new(
        workspace.key.clone(),
        SessionKind::Shell,
        worktree.to_path_buf(),
        now,
    ));
    workspace
}

// ── Scenario vocabulary ─────────────────────────────────────────────────

/// One scripted beat: wait `after` (relative to the previous beat), then
/// apply `action`. Relative delays keep a scenario readable as a timeline.
pub struct Step {
    pub after: Duration,
    pub action: Action,
}

impl Step {
    pub fn new(after_ms: u64, action: Action) -> Self {
        Self {
            after: Duration::from_millis(after_ms),
            action,
        }
    }
}

/// The scenario vocabulary. Terminals are addressed by a stable *slot*
/// number the author assigns; the driver resolves each slot to a concrete
/// terminal at run time — a synthetic `TerminalId` in Tier 1, or a real
/// daemon terminal (+ backend key) in Tier 2 (see [`Stage`]).
pub enum Action {
    /// Register a terminal in `slot` for workspace `key`, running `kind`.
    Spawn {
        slot: u64,
        key: SessionKey,
        kind: TerminalKind,
    },
    /// Feed raw (optionally ANSI) bytes into `slot`'s terminal.
    Out { slot: u64, bytes: Vec<u8> },
    /// Flip a workspace's agent state (sidebar pill + terminal tab badge).
    State {
        slot: u64,
        key: SessionKey,
        state: AgentState,
    },
    /// Flash a footer notice.
    Notify { title: String, body: String },
    /// Re-broadcast a full workspace (e.g. flip CI red → green mid-demo).
    UpsertWorkspace(Arc<Workspace>),
}

/// Convenience: spawn a Claude agent in `slot`.
pub fn spawn_claude(slot: u64, key: &SessionKey) -> Action {
    Action::Spawn {
        slot,
        key: key.clone(),
        kind: TerminalKind::Agent("claude".into()),
    }
}

/// Convenience: spawn a Codex agent in `slot`.
pub fn spawn_codex(slot: u64, key: &SessionKey) -> Action {
    Action::Spawn {
        slot,
        key: key.clone(),
        kind: TerminalKind::Agent("codex".into()),
    }
}

/// Convenience: stream a chunk of terminal output into `slot`.
pub fn output(slot: u64, text: impl Into<String>) -> Action {
    Action::Out {
        slot,
        bytes: text.into().into_bytes(),
    }
}

/// Convenience: an agent-state flip on `slot`'s workspace.
pub fn agent(slot: u64, key: &SessionKey, state: AgentState) -> Action {
    Action::State {
        slot,
        key: key.clone(),
        state,
    }
}

// ── Driver ──────────────────────────────────────────────────────────────

/// How the driver realizes terminals — the two tiers from the interface
/// review.
// `Backed` carries a full `ServerConfig` (a bundle of `Arc`s) so it dwarfs the
// unit `BusOnly`; the enum is instantiated exactly once per process, so the
// size asymmetry is irrelevant and boxing would only add indirection.
#[allow(clippy::large_enum_variant)]
pub enum Stage {
    /// Tier 1: pure bus producer. Terminals live only in each client's VT
    /// state — perfect playback, but not durable across a recovery `Snapshot`
    /// and not typeable (a keystroke has no backend session). Needs no daemon
    /// handle — this is the tier that demonstrates the interface gaps, and the
    /// one unit tests drive; `--demo` itself uses `Backed`.
    #[allow(dead_code)]
    BusOnly,
    /// Tier 2: real daemon terminals via [`MockBackend`]. Each `Spawn` issues
    /// a genuine `Command::Spawn` through [`lazybox_server::dispatch_command`],
    /// so the terminal
    /// is registered in the daemon's `TerminalRegistry` — it survives recovery
    /// snapshots AND accepts input (keystrokes land as backend writes, not
    /// `NotFound`). Output is fed through `MockBackend::emit`, so the daemon's
    /// own pump owns the sequence numbers. This is what closes both interface
    /// gaps; `--demo` uses it.
    Backed {
        config: ServerConfig,
        mock: MockBackend,
        /// The throwaway repo the spawns run in (overrides worktree
        /// provisioning so no bare clone is needed).
        cwd: PathBuf,
    },
}

/// Publishes scenario actions, resolving each slot to a concrete terminal
/// per the active [`Stage`].
struct Driver {
    bus: broadcast::Sender<Event>,
    stage: Stage,
    /// slot → resolved terminal id (both tiers).
    slot_tid: HashMap<u64, TerminalId>,
    /// slot → backend session key (Tier 2 only).
    slot_key: HashMap<u64, String>,
    /// slot → next chunk seq (Tier 1 only — Tier 2's pump owns seqs).
    seqs: HashMap<u64, u64>,
    /// Sink for `dispatch_command` replies (Tier 2). Kept alive so the
    /// unbounded channel never reports closed; its contents are ignored.
    sink: lazybox_ipc::EventSender,
    _sink_rx: mpsc::UnboundedReceiver<Event>,
}

impl Driver {
    fn new(bus: broadcast::Sender<Event>, stage: Stage) -> Self {
        let (tx, _sink_rx) = mpsc::unbounded_channel();
        Self {
            bus,
            stage,
            slot_tid: HashMap::new(),
            slot_key: HashMap::new(),
            seqs: HashMap::new(),
            sink: lazybox_ipc::EventSender::from_unbounded(tx),
            _sink_rx,
        }
    }

    fn send(&self, event: Event) {
        // A send fails only if there are zero subscribers — during a demo the
        // TUI is always attached, so a drop just means the beat is invisible;
        // never fatal.
        let _ = self.bus.send(event);
    }

    async fn apply(&mut self, action: Action) {
        match action {
            Action::Spawn { slot, key, kind } => self.spawn(slot, key, kind).await,
            Action::Out { slot, bytes } => self.out(slot, bytes).await,
            Action::State { slot, key, state } => {
                let terminal = self
                    .slot_tid
                    .get(&slot)
                    .copied()
                    .unwrap_or(TerminalId(slot));
                self.send(Event::AgentState {
                    session_key: key,
                    terminal_id: terminal,
                    state,
                });
            }
            Action::Notify { title, body } => self.send(Event::Notification { title, body }),
            Action::UpsertWorkspace(ws) => self.send(Event::WorkspaceUpserted(ws)),
        }
    }

    async fn spawn(&mut self, slot: u64, key: SessionKey, kind: TerminalKind) {
        // Clone the daemon handles out first so `self.stage` isn't borrowed
        // while we mutate the slot maps below.
        let (config, cwd) = match &self.stage {
            Stage::BusOnly => {
                let tid = TerminalId(slot);
                self.slot_tid.insert(slot, tid);
                self.seqs.insert(slot, 0);
                self.send(Event::TerminalSpawned {
                    terminal_id: tid,
                    session_key: key,
                    kind,
                    no_permission: false,
                    on_main: false,
                    model_label: None,
                });
                return;
            }
            Stage::Backed { config, cwd, .. } => (config.clone(), cwd.clone()),
        };

        // Tier 2: spawn the real way. Snapshot the id set, issue the command,
        // then find the id that appeared.
        let before: std::collections::HashSet<u64> = config
            .terminal
            .terminal_ids()
            .await
            .into_iter()
            .map(|t| t.0)
            .collect();
        let cmd = lazybox_ipc::Command::Spawn {
            session_key: key,
            session_id: None,
            client_request_id: None,
            kind,
            cwd: Some(cwd.to_string_lossy().into_owned()),
            initial_prompt: None,
            initial_snippet: None,
            on_main: false,
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
            force_new: true,
        };
        lazybox_server::dispatch_command(&config, &self.sink, cmd).await;

        // Registration can lag the command return (SpawnCoordinator). Poll
        // briefly for the new id rather than racing it.
        let mut tid = None;
        for _ in 0..40 {
            if let Some(found) = config
                .terminal
                .terminal_ids()
                .await
                .into_iter()
                .find(|t| !before.contains(&t.0))
            {
                tid = Some(found);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let Some(tid) = tid else {
            tracing::warn!("demo spawn for slot {slot} never registered a terminal");
            return;
        };
        self.slot_tid.insert(slot, tid);
        if let Some(backend_key) = config.terminal.backend_key_for(tid).await {
            self.slot_key.insert(slot, backend_key);
        }
    }

    async fn out(&mut self, slot: u64, bytes: Vec<u8>) {
        match &self.stage {
            Stage::BusOnly => {
                let tid = self
                    .slot_tid
                    .get(&slot)
                    .copied()
                    .unwrap_or(TerminalId(slot));
                let seq = self.seqs.entry(slot).or_insert(0);
                *seq += 1;
                let seq = *seq;
                self.send(Event::TerminalOutput {
                    terminal_id: tid,
                    bytes: bytes.into(),
                    first_seq: seq,
                    seq,
                });
            }
            Stage::Backed { mock, .. } => {
                if let Some(key) = self.slot_key.get(&slot) {
                    // The daemon's pump reads this and emits an authoritative
                    // TerminalOutput with its own contiguous seq.
                    mock.emit(key, &bytes).await;
                }
            }
        }
    }
}

/// Run a scenario. Sleeps to each beat, then applies it. An initial settle
/// delay lets the TUI's Subscribe → Snapshot complete before the first live
/// event, so nothing is broadcast into the void.
pub async fn run(bus: broadcast::Sender<Event>, stage: Stage, settle: Duration, steps: Vec<Step>) {
    tokio::time::sleep(settle).await;
    let mut driver = Driver::new(bus, stage);
    for step in steps {
        tokio::time::sleep(step.after).await;
        driver.apply(step.action).await;
    }
}

// ── Reactor ──────────────────────────────────────────────────────────────

/// The practice-mode **reactor** (#1459): what turns the scripted movie into
/// a simulator. Where [`run`] plays a fixed timeline, the reactor *subscribes
/// to the daemon bus* and makes the world respond to what the user actually
/// does — spawn an agent and it comes alive; type a reply and it answers.
///
/// It runs only in practice mode. `--demo` keeps its scripted [`fleet_scenario`]
/// and no reactor, so the two never both drive the same terminal.
///
/// ## Why a bus subscriber, not a command tap
///
/// The daemon exposes no command-observer hook — a second component sees only
/// the [`Event`]s a command's handlers broadcast (`config.bus.subscribe()`),
/// exactly as the working-watchdog and auto-wait tasks do. So the reactor
/// keys off `TerminalSpawned` to bring a fresh agent to life. User *input*,
/// though, is a `Command::Write` that broadcasts no event — a real gap in the
/// daemon→client contract this harness exists to surface — so replies are
/// detected by polling the [`MockBackend`]'s recorded writes, the one place
/// that input lands with no backend process to echo it.
pub struct Reactor {
    config: ServerConfig,
    mock: MockBackend,
    rx: broadcast::Receiver<Event>,
    /// Agent terminals the reactor has taken over, keyed by terminal id.
    tracked: HashMap<TerminalId, TrackedAgent>,
}

struct TrackedAgent {
    backend_key: String,
    /// Count of backend writes already answered, so a poll only reacts to
    /// input that arrived since the last reply.
    answered_writes: usize,
    /// Serialized per-terminal play queue. Each job (intro, or a reply)
    /// runs to completion — streaming its lines and its final `AgentState`
    /// — before the next starts, so two beats on the SAME terminal can never
    /// interleave their output or land their state flips out of order. The
    /// dedicated task ends when this sender is dropped (terminal untracked).
    jobs: mpsc::UnboundedSender<PlayJob>,
}

/// One serialized beat on a terminal: optional cooked-mode echo of the
/// user's input, a canned transcript, then a settle state.
struct PlayJob {
    echo: Option<Vec<u8>>,
    lines: &'static [&'static str],
    end: AgentState,
}

impl Reactor {
    pub fn new(config: ServerConfig, mock: MockBackend) -> Self {
        let rx = config.bus.subscribe();
        Self {
            config,
            mock,
            rx,
            tracked: HashMap::new(),
        }
    }

    /// Drive the reactor until the bus closes (daemon shutdown). Interleaves
    /// bus events (new agent terminals) with a slow poll of recorded input
    /// (user replies), so neither starves the other.
    pub async fn run(mut self) {
        let mut poll = tokio::time::interval(Duration::from_millis(250));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                event = self.rx.recv() => match event {
                    Ok(event) => self.on_event(event).await,
                    Err(broadcast::error::RecvError::Closed) => return,
                    // A lagged reactor just misses a beat; the next spawn
                    // still gets picked up.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                },
                _ = poll.tick() => self.poll_replies().await,
            }
        }
    }

    async fn on_event(&mut self, event: Event) {
        match event {
            // A dead terminal (kill / archive / close) drops its play queue —
            // ending the per-terminal task — and stops us polling a backend key
            // that no longer exists, so `tracked` can't grow without bound over
            // a long session.
            Event::TerminalExited { terminal_id, .. } => {
                self.tracked.remove(&terminal_id);
            }
            // Only agent spawns get a reaction; a plain shell is left as-is.
            Event::TerminalSpawned {
                terminal_id,
                session_key,
                kind: TerminalKind::Agent(_),
                ..
            } => self.on_agent_spawned(terminal_id, session_key).await,
            _ => {}
        }
    }

    async fn on_agent_spawned(&mut self, terminal_id: TerminalId, session_key: SessionKey) {
        if self.tracked.contains_key(&terminal_id) {
            return;
        }
        let Some(backend_key) = self.config.terminal.backend_key_for(terminal_id).await else {
            return;
        };
        let jobs = self.spawn_agent_task(terminal_id, session_key, backend_key.clone());
        // Start the counter at zero — NOT past whatever is already written.
        // A `w w`-style spawn injects the user's brief as a write that can
        // land before OR after this event, so anchoring past it would race and
        // silently drop the brief. From zero, poll_replies answers the brief
        // the moment it arrives, whichever side of the spawn it lands.
        self.tracked.insert(
            terminal_id,
            TrackedAgent {
                backend_key,
                answered_writes: 0,
                jobs: jobs.clone(),
            },
        );
        let _ = jobs.send(PlayJob {
            echo: None,
            lines: AGENT_INTRO,
            end: AgentState::InputNeeded,
        });
    }

    /// Scan tracked agents for input that arrived since the last reply and,
    /// where the user has submitted a line, queue an answer.
    async fn poll_replies(&mut self) {
        for agent in self.tracked.values_mut() {
            let writes = self.mock.writes_for(&agent.backend_key).await;
            if writes.len() <= agent.answered_writes {
                continue;
            }
            let fresh = &writes[agent.answered_writes..];
            // Only respond once the user has committed a line — a bare
            // submit (Enter) or any newline in the fresh bytes.
            let submitted = fresh
                .iter()
                .any(|w| w.iter().any(|b| *b == b'\r' || *b == b'\n'));
            if !submitted {
                continue;
            }
            agent.answered_writes = writes.len();
            // The mock backend has no child to echo the keystrokes, so a real
            // PTY's cooked-mode echo is reproduced here (inside the same job as
            // the reply, so the echo can't interleave with an intro still
            // streaming on this terminal).
            let _ = agent.jobs.send(PlayJob {
                echo: Some(fresh.concat()),
                lines: AGENT_REPLY,
                end: AgentState::Done,
            });
        }
    }

    #[cfg(test)]
    fn tracked_ids(&self) -> Vec<TerminalId> {
        self.tracked.keys().copied().collect()
    }

    /// Spawn the dedicated, serialized play task for one terminal and return
    /// the sender that feeds it. Dropping the sender ends the task.
    fn spawn_agent_task(
        &self,
        terminal_id: TerminalId,
        session_key: SessionKey,
        backend_key: String,
    ) -> mpsc::UnboundedSender<PlayJob> {
        let (tx, mut rx) = mpsc::unbounded_channel::<PlayJob>();
        let bus = self.config.bus.clone();
        let mock = self.mock.clone();
        tokio::spawn(async move {
            let state = |state| Event::AgentState {
                session_key: session_key.clone(),
                terminal_id,
                state,
            };
            while let Some(job) = rx.recv().await {
                if let Some(echo) = job.echo {
                    mock.emit(&backend_key, &echo).await;
                }
                tokio::time::sleep(Duration::from_millis(350)).await;
                let _ = bus.send(state(AgentState::Working));
                for line in job.lines {
                    tokio::time::sleep(Duration::from_millis(280)).await;
                    mock.emit(&backend_key, line.as_bytes()).await;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                let _ = bus.send(state(job.end));
            }
        });
        tx
    }
}

/// Canned first-contact transcript a freshly-spawned practice agent streams,
/// ending on a prompt so the user is invited to reply.
const AGENT_INTRO: &[&str] = &[
    "\x1b[38;5;208m✻\x1b[0m Waking up in the practice sandbox…\r\n",
    "  \x1b[36m⎿\x1b[0m Read the workspace (nothing here is real)\r\n",
    "\x1b[32m●\x1b[0m I've had a look. \x1b[1mWhat would you like me to do?\x1b[0m\r\n",
    "  Type a message and press Enter — you can't break anything.\r\n",
];

/// Canned reply a practice agent streams after the user submits a line.
const AGENT_REPLY: &[&str] = &[
    "\r\n\x1b[38;5;208m✻\x1b[0m On it…\r\n",
    "  \x1b[36m⎿\x1b[0m (practice) pretending to edit files\r\n",
    "\x1b[32m●\x1b[0m Done — in a real session I'd have pushed a commit and opened a PR.\r\n",
];

/// The built-in "fleet" scenario: brings the seeded inbox to life — several
/// agents working across repos, one asking, one done, one rate-limited — with
/// live terminals streaming canned Claude/Codex output for the focus-mode
/// grid. Designed to exercise the six v0.1.13 features on camera.
pub fn fleet_scenario(fx: &DemoFixture) -> Vec<Step> {
    // Stable terminal-id assignment per seeded workspace index.
    let ws = &fx.workspaces;
    let k = |i: usize| ws[i].key.clone();

    // ── Phase 1: light up the whole fleet fast ───────────────────────────
    // Spawn every terminal and set every agent state within ~1.5s so the
    // sidebar comes alive at once and all terminals exist for the focus
    // grid, rather than trickling in over 12s. Terminal ids: 1=lazybox
    // (working claude), 2=web (working codex), 3=footer-fade (asking),
    // 4=rate-limiter (done), 5=cli (rate-limited).
    let mut steps = vec![
        Step::new(150, spawn_claude(1, &k(0))),
        Step::new(100, spawn_codex(2, &k(2))),
        Step::new(100, spawn_claude(3, &k(1))),
        Step::new(100, spawn_claude(4, &k(3))),
        Step::new(100, spawn_claude(5, &k(5))),
        Step::new(150, agent(1, &k(0), AgentState::Working)),
        Step::new(80, agent(2, &k(2), AgentState::Working)),
        Step::new(80, agent(3, &k(1), AgentState::InputNeeded)),
        Step::new(80, agent(4, &k(3), AgentState::Done)),
        Step::new(80, agent(5, &k(5), AgentState::LimitReached)),
        // Seed the resting terminals with their one-shot content immediately.
        Step::new(120, output(3, ASK_TRANSCRIPT)),
        Step::new(120, output(4, DONE_TRANSCRIPT)),
        Step::new(120, output(5, LIMIT_TRANSCRIPT)),
    ];

    // ── Phase 2: stream the two working agents round-robin ───────────────
    // Interleave Claude (t1) and Codex (t2) so both terminals visibly churn
    // at the same time — the multi-agent fleet in motion.
    let max = CLAUDE_TRANSCRIPT.len().max(CODEX_TRANSCRIPT.len());
    for i in 0..max {
        if let Some(line) = CLAUDE_TRANSCRIPT.get(i) {
            steps.push(Step::new(420, output(1, *line)));
        }
        if let Some(line) = CODEX_TRANSCRIPT.get(i) {
            steps.push(Step::new(180, output(2, *line)));
        }
    }

    // The reactive inbox surfacing a change: the footer-fade PR's CI, red at
    // seed time, flips green. Re-upsert a mutated copy — the CI indicator is
    // store/payload-carried, so a fresh WorkspaceUpserted is how it changes.
    let mut greened = (*ws[1].workspace).clone();
    if let Some(task) = greened.primary_task_mut() {
        task.ci = CiStatus::Success;
        task.review = ReviewStatus::Approved;
        task.is_behind_base = false;
        task.updated_at = Utc::now();
    }
    steps.push(Step::new(1200, Action::UpsertWorkspace(Arc::new(greened))));
    steps.push(Step::new(
        150,
        Action::Notify {
            title: "lazybox".into(),
            body: "obin-ai/lazybox #1340 · CI passed · changes addressed".into(),
        },
    ));

    steps
}

/// Canned Claude Code output — a compact, realistic working transcript with
/// ANSI color so the rendered terminal looks alive.
const CLAUDE_TRANSCRIPT: &[&str] = &[
    "\x1b[38;5;208m✻\x1b[0m Working on \x1b[1mprobe author:@me\x1b[0m…\r\n",
    "  \x1b[36m⎿\x1b[0m Read crates/gh-provider/src/client.rs (2841 lines)\r\n",
    "  \x1b[36m⎿\x1b[0m Read crates/gh-provider/src/notifications.rs\r\n",
    "\x1b[32m●\x1b[0m Adding the author-probe cadence to NotificationsState\r\n",
    "  \x1b[36m⎿\x1b[0m Edited notifications.rs (+18 -0)\r\n",
    "\x1b[32m●\x1b[0m Wiring the probe into the fetch dispatch\r\n",
    "  \x1b[36m⎿\x1b[0m Edited polling/sources/mod.rs (+27 -1)\r\n",
    "\x1b[90m$ cargo build -p lazybox-gh\x1b[0m\r\n",
    "   \x1b[32mCompiling\x1b[0m lazybox-gh v0.1.13\r\n",
    "    \x1b[32mFinished\x1b[0m in 41.2s\r\n",
];

/// Canned Codex output — a different agent voice, so the grid reads as a
/// genuine multi-agent fleet.
const CODEX_TRANSCRIPT: &[&str] = &[
    "\x1b[35mcodex\x1b[0m refreshing the landing hero\r\n",
    "  updating web/src/pages/index.astro\r\n",
    "  \x1b[32m+\x1b[0m added feature grid for v0.1.13\r\n",
    "\x1b[90m$ pnpm build\x1b[0m\r\n",
    "  \x1b[36mvite\x1b[0m building for production…\r\n",
    "  \x1b[32m✓\x1b[0m 214 modules transformed\r\n",
    "  \x1b[32m✓\x1b[0m built in 3.71s\r\n",
];

const ASK_TRANSCRIPT: &str = "\x1b[32m●\x1b[0m The fade timer could key off severity or a flat 45s.\r\n\
     \x1b[1mWhich should permanent-but-dismissable notices use?\x1b[0m\r\n\
     \x1b[36m❯ 1.\x1b[0m Severity-scaled (retryable faster than auth)\r\n  \
     2. Flat 45s for everything\r\n";

const DONE_TRANSCRIPT: &str = "\x1b[32m●\x1b[0m Rate limiter landed — token bucket at 150 req/s.\r\n  \
     \x1b[36m⎿\x1b[0m All 41 tests pass.\r\n\
     \x1b[90m─ done (4m 12s) ─\x1b[0m\r\n";

const LIMIT_TRANSCRIPT: &str = "\x1b[33m⏳ Claude usage limit reached.\x1b[0m\r\n  \
     Resets at 4:00 PM. \x1b[1mWait, or switch account?\x1b[0m\r\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_seeds_multiple_owners_for_spaces() {
        let fx = DemoFixture::seed().expect("fixture builds");
        let rows = fx.store.list_workspaces().expect("list");
        assert_eq!(rows.len(), 7, "seven seeded workspaces");
        let owners: std::collections::BTreeSet<_> = fx
            .workspaces
            .iter()
            .map(|w| w.repo.split('/').next().unwrap().to_string())
            .collect();
        assert_eq!(
            owners.len(),
            3,
            "three distinct owners so Spaces auto-group: {owners:?}"
        );
    }

    #[test]
    fn seeded_workspaces_are_real_prs_with_a_worktree() {
        let fx = DemoFixture::seed().unwrap();
        let rows = fx.store.list_workspaces().unwrap();
        let ws: Workspace = serde_json::from_str(rows[0].workspace_json.as_ref().unwrap()).unwrap();
        assert_eq!(ws.session_count(), 1, "one shell session");
        assert!(ws.primary_task().is_some(), "classified as a real task");
    }

    #[tokio::test]
    async fn driver_assigns_contiguous_terminal_seqs() {
        // The Tier-1 invariant: TerminalOutput seqs must be 1,2,3… per
        // terminal or the client desyncs. Drive a spawn + three outputs and
        // assert the emitted seqs.
        let (bus, mut rx) = broadcast::channel(64);
        let mut driver = Driver::new(bus, Stage::BusOnly);
        let key = SessionKey::from("demo/repo#1");
        driver
            .apply(Action::Spawn {
                slot: 7,
                key: key.clone(),
                kind: TerminalKind::Agent("claude".into()),
            })
            .await;
        for _ in 0..3 {
            driver
                .apply(Action::Out {
                    slot: 7,
                    bytes: b"x".to_vec(),
                })
                .await;
        }
        // First event is the spawn.
        assert!(matches!(rx.try_recv(), Ok(Event::TerminalSpawned { .. })));
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let Event::TerminalOutput { first_seq, seq, .. } = ev {
                assert_eq!(first_seq, seq, "single-chunk first_seq == seq");
                seen.push(seq);
            }
        }
        assert_eq!(seen, vec![1, 2, 3], "contiguous per-terminal seqs from 1");
    }

    #[tokio::test]
    async fn backed_stage_terminal_is_durable_and_typeable() {
        // The whole point of Tier 2: a demo terminal spawned via the real
        // daemon path is (a) registered in the terminal registry — so a
        // recovery Snapshot includes it — and (b) input-accepting — a Write
        // lands as a backend write, not `NotFound`. This test proves both
        // interface gaps are closed.
        use lazybox_ipc::{Command, EventSender, TerminalInputIntent};

        let fx = DemoFixture::seed().unwrap();
        let mock = MockBackend::new();
        let config = ServerConfig::with_store_and_backend(fx.store.clone(), mock.as_backend());
        let key = fx.workspaces[0].key.clone();
        let mut driver = Driver::new(
            config.bus.clone(),
            Stage::Backed {
                config: config.clone(),
                mock: mock.clone(),
                cwd: fx.repo.path().to_path_buf(),
            },
        );

        driver
            .apply(Action::Spawn {
                slot: 1,
                key,
                kind: TerminalKind::Agent("claude".into()),
            })
            .await;
        driver
            .apply(Action::Out {
                slot: 1,
                bytes: b"hello from the agent\n".to_vec(),
            })
            .await;

        let ids = config.terminal.terminal_ids().await;
        assert_eq!(ids.len(), 1, "spawn registered exactly one real terminal");
        let tid = ids[0];
        let backend_key = config
            .terminal
            .backend_key_for(tid)
            .await
            .expect("real terminal has a backend session");

        // Output flowed through the daemon's backend, not a synthetic bus event.
        let snap = config
            .backend
            .snapshot(&backend_key)
            .await
            .expect("backend snapshot");
        assert!(
            snap.replay.windows(5).any(|w| w == b"hello"),
            "emitted output reached the backend replay buffer"
        );

        // Gap 1 (input): a Write resolves to the backend session and is
        // recorded — no `NotFound`.
        let (tx, _rx) = mpsc::unbounded_channel();
        let sink = EventSender::from_unbounded(tx);
        lazybox_server::dispatch_command(
            &config,
            &sink,
            Command::Write {
                terminal_id: tid,
                bytes: b"typed input\n".to_vec(),
                intent: TerminalInputIntent::Submit,
            },
        )
        .await;
        let mut writes = Vec::new();
        for _ in 0..40 {
            writes = mock.writes_for(&backend_key).await;
            if !writes.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            writes.iter().any(|w| w == b"typed input\n"),
            "keystroke reached the backend — input gap closed: {writes:?}"
        );

        // Gap 2 (durability): a fresh recovery Subscribe rebuilds terminals
        // from the daemon registry, and this terminal is present.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSender::from_unbounded(tx);
        lazybox_server::dispatch_command(&config, &sink, Command::Subscribe).await;
        let mut in_snapshot = false;
        while let Ok(ev) = rx.try_recv() {
            if let Event::Snapshot { terminals, .. } = ev {
                in_snapshot = terminals.iter().any(|t| t.terminal_id == tid);
                break;
            }
        }
        assert!(
            in_snapshot,
            "recovery Snapshot includes the terminal — durability gap closed"
        );
    }

    /// Drain the bus until `pred` matches an event or `deadline` elapses.
    async fn wait_for_event(
        rx: &mut broadcast::Receiver<Event>,
        deadline: Duration,
        mut pred: impl FnMut(&Event) -> bool,
    ) -> bool {
        let stop = tokio::time::Instant::now() + deadline;
        loop {
            let left = stop.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Ok(ev)) if pred(&ev) => return true,
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return false,
            }
        }
    }

    #[tokio::test]
    async fn reactor_brings_a_spawned_agent_to_life_and_answers_a_reply() {
        // The heart of "simulator, not movie" (#1459 criterion 5): with the
        // reactor running, the user's own spawn brings the agent to life, and
        // a submitted reply gets answered — all on the real daemon paths, no
        // script driving these terminals.
        use lazybox_ipc::{Command, EventSender, TerminalInputIntent};

        let fx = DemoFixture::seed().unwrap();
        let mock = MockBackend::new();
        let config = ServerConfig::with_store_and_backend(fx.store.clone(), mock.as_backend());
        let mut rx = config.bus.subscribe();
        tokio::spawn(Reactor::new(config.clone(), mock.clone()).run());

        let (tx, _keep) = mpsc::unbounded_channel();
        let sink = EventSender::from_unbounded(tx);
        let key = fx.workspaces[0].key.clone();
        lazybox_server::dispatch_command(
            &config,
            &sink,
            Command::Spawn {
                session_key: key.clone(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: Some(fx.repo.path().to_string_lossy().into_owned()),
                initial_prompt: None,
                initial_snippet: None,
                on_main: false,
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
                force_new: true,
            },
        )
        .await;

        // The reactor spun the fresh agent up and settled it on a question.
        let asked = wait_for_event(&mut rx, Duration::from_secs(8), |ev| {
            matches!(
                ev,
                Event::AgentState {
                    state: AgentState::InputNeeded,
                    ..
                }
            )
        })
        .await;
        assert!(asked, "reactor flips a spawned agent to InputNeeded");

        let tid = config.terminal.terminal_ids().await[0];
        let backend_key = config.terminal.backend_key_for(tid).await.unwrap();
        let intro = config.backend.snapshot(&backend_key).await.unwrap().replay;
        assert!(
            String::from_utf8_lossy(&intro).contains("practice sandbox"),
            "the intro transcript streamed into the terminal"
        );

        // The user replies; the reactor answers and marks the agent Done.
        lazybox_server::dispatch_command(
            &config,
            &sink,
            Command::Write {
                terminal_id: tid,
                bytes: b"look at the failing test\r".to_vec(),
                intent: TerminalInputIntent::Submit,
            },
        )
        .await;

        let done = wait_for_event(&mut rx, Duration::from_secs(8), |ev| {
            matches!(
                ev,
                Event::AgentState {
                    state: AgentState::Done,
                    ..
                }
            )
        })
        .await;
        assert!(
            done,
            "reactor answers a reply and settles the agent to Done"
        );

        let replay = config.backend.snapshot(&backend_key).await.unwrap().replay;
        let text = String::from_utf8_lossy(&replay);
        assert!(
            text.contains("look at the failing test"),
            "the user's typed line was echoed back into the terminal"
        );
        assert!(
            text.contains("pushed a commit"),
            "the agent's canned reply streamed after the user's line"
        );
    }

    #[tokio::test]
    async fn reactor_serializes_an_intro_and_an_early_reply() {
        // Regression for the play-ordering race: a reply that lands WHILE the
        // intro is still streaming must not interleave its output with the
        // intro nor let its `Done` be overtaken by the intro's later
        // `InputNeeded`. The per-terminal job queue serializes them.
        use lazybox_ipc::{Command, EventSender, TerminalInputIntent};

        let fx = DemoFixture::seed().unwrap();
        let mock = MockBackend::new();
        let config = ServerConfig::with_store_and_backend(fx.store.clone(), mock.as_backend());
        let mut rx = config.bus.subscribe();
        tokio::spawn(Reactor::new(config.clone(), mock.clone()).run());

        let (tx, _keep) = mpsc::unbounded_channel();
        let sink = EventSender::from_unbounded(tx);
        let key = fx.workspaces[0].key.clone();
        lazybox_server::dispatch_command(
            &config,
            &sink,
            Command::Spawn {
                session_key: key.clone(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: Some(fx.repo.path().to_string_lossy().into_owned()),
                initial_prompt: None,
                initial_snippet: None,
                on_main: false,
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
                force_new: true,
            },
        )
        .await;

        // Reply IMMEDIATELY — before the intro (~1.6s) can settle.
        let mut tid = None;
        for _ in 0..40 {
            if let Some(found) = config.terminal.terminal_ids().await.first().copied() {
                tid = Some(found);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let tid = tid.expect("terminal registered");
        lazybox_server::dispatch_command(
            &config,
            &sink,
            Command::Write {
                terminal_id: tid,
                bytes: b"go\r".to_vec(),
                intent: TerminalInputIntent::Submit,
            },
        )
        .await;

        // Record the order of the settle states for this session.
        let mut states = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        while tokio::time::Instant::now() < deadline {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Ok(Event::AgentState { state, .. })) => {
                    if matches!(state, AgentState::InputNeeded | AgentState::Done) {
                        states.push(state);
                        if matches!(state, AgentState::Done) {
                            break;
                        }
                    }
                }
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(_)) | Err(_) => break,
            }
        }
        // Serialized: the intro settles to InputNeeded, THEN the reply settles
        // to Done — Done is last and never precedes the intro's InputNeeded.
        assert_eq!(
            states.last(),
            Some(&AgentState::Done),
            "the reply's Done is the final state, not overtaken by the intro: {states:?}"
        );
        assert!(
            states.iter().any(|s| matches!(s, AgentState::InputNeeded)),
            "the intro still settled to InputNeeded before the reply: {states:?}"
        );

        // And the transcripts don't interleave: every intro line precedes the
        // reply.
        let backend_key = config.terminal.backend_key_for(tid).await.unwrap();
        let text: String =
            String::from_utf8_lossy(&config.backend.snapshot(&backend_key).await.unwrap().replay)
                .into_owned();
        let intro_end = text
            .find("you can't break anything")
            .expect("intro streamed");
        let reply_start = text.find("pushed a commit").expect("reply streamed");
        assert!(
            intro_end < reply_start,
            "intro fully precedes the reply — no interleave: {text:?}"
        );
    }

    #[tokio::test]
    async fn reactor_untracks_a_terminal_when_it_exits() {
        // Regression for the unbounded-`tracked` leak: a killed/archived
        // terminal must be dropped so the reactor stops polling a dead backend
        // key and the map can't grow without bound over a long session.
        use lazybox_ipc::{Command, EventSender};

        let fx = DemoFixture::seed().unwrap();
        let mock = MockBackend::new();
        let config = ServerConfig::with_store_and_backend(fx.store.clone(), mock.as_backend());
        let mut reactor = Reactor::new(config.clone(), mock.clone());

        let (tx, _keep) = mpsc::unbounded_channel();
        let sink = EventSender::from_unbounded(tx);
        let key = fx.workspaces[0].key.clone();
        lazybox_server::dispatch_command(
            &config,
            &sink,
            Command::Spawn {
                session_key: key.clone(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: Some(fx.repo.path().to_string_lossy().into_owned()),
                initial_prompt: None,
                initial_snippet: None,
                on_main: false,
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
                force_new: true,
            },
        )
        .await;
        let tid = config.terminal.terminal_ids().await[0];

        reactor
            .on_event(Event::TerminalSpawned {
                terminal_id: tid,
                session_key: key,
                kind: TerminalKind::Agent("claude".into()),
                no_permission: false,
                on_main: false,
                model_label: None,
            })
            .await;
        assert_eq!(reactor.tracked_ids(), vec![tid], "spawn tracks the agent");

        reactor
            .on_event(Event::TerminalExited {
                terminal_id: tid,
                exit_code: Some(0),
                last_output: None,
            })
            .await;
        assert!(
            reactor.tracked_ids().is_empty(),
            "an exited terminal is untracked, so tracked can't grow unbounded"
        );
    }

    #[test]
    fn fleet_scenario_covers_every_agent_state() {
        let fx = DemoFixture::seed().unwrap();
        let steps = fleet_scenario(&fx);
        let mut states = std::collections::BTreeSet::new();
        for step in &steps {
            if let Action::State { state, .. } = &step.action {
                states.insert(format!("{state:?}"));
            }
        }
        for want in ["Working", "InputNeeded", "Done", "LimitReached"] {
            assert!(
                states.contains(want),
                "scenario exercises {want}: {states:?}"
            );
        }
    }
}
