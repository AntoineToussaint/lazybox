//! Opt-in auto-"Wait" on a Claude usage-limit block (`ui.auto_wait_on_limit`,
//! issue #847).
//!
//! When several agents share one Claude account they all hit the usage /
//! monthly limit at once, each parking on the "limit reached — Wait?"
//! prompt. Visiting every terminal to press Wait is pure toil. With this
//! opt-in policy on, the daemon watches the event bus and, the moment an
//! agent transitions into `AgentState::LimitReached`, sends a submit
//! keystroke to accept the prompt's highlighted default (Wait) — the same
//! byte a user pressing Enter on that chooser would send.
//!
//! Why the bus, not the detection hot path: the state machine already
//! dedups, so `Event::AgentState { state: LimitReached }` fires exactly
//! once per episode (a later re-entry is a fresh transition, hence a fresh
//! event) — the rising edge is free, no per-terminal latch needed. And the
//! flag is re-read from YAML on every event, mirroring [`crate::keep_awake`],
//! so toggling it takes effect without a daemon restart.
//!
//! Detecting + surfacing the block is always on; this only automates the
//! keystroke, and only when explicitly opted in. lazybox still can't do the
//! re-auth — that's the user switching account / API key externally — but
//! it removes the two manual sweeps around it (press Wait, then `Shift-K`
//! to resume all).
//!
//! ## Calm status while parked
//!
//! Pressing Wait also relabels the block from the alerting
//! [`AgentState::LimitReached`] (`⏳`, reads as "needs you") to the calm
//! [`AgentState::AwaitingReset`] (`💤`, "parked, will resume") — the block
//! is handled now, so it drops out of the alert count, resume-all set, and
//! desktop/Slack notifications. The relabel is daemon-asserted and held by
//! the state machine against the lingering limit banner.
//!
//! ## Resuming the interrupted work when the wait clears
//!
//! Pressing Wait only parks the agent — once the limit resets Claude comes
//! to rest at an empty composer instead of picking the interrupted task
//! back up, so the toil re-appears at the far end (visit each terminal,
//! type "continue"). So auto-wait remembers every terminal it pressed Wait
//! on and, the moment that terminal transitions *out* of `LimitReached` to
//! a resting screen (`Done` — the reset happened and the turn settled),
//! injects the same continuation nudge the credit-recovery flow uses
//! (`ui.credit_recovery_prompt`, "Continue the work you were doing.")
//! through the settle-gated inject path, so it waits for the composer and
//! confirms submission rather than blind-firing a keystroke. A clear to
//! `Working` means the agent resumed on its own (re-auth / auto-continue) —
//! that needs no nudge, so the tracked terminal is simply dropped.

use lazybox_ipc::{AgentState, Event, TerminalId, TerminalInputIntent};
use std::collections::HashSet;
use tokio::sync::broadcast;

use crate::ServerConfig;

/// The byte the daemon writes to accept the limit prompt — a bare
/// carriage return, exactly what a user pressing Enter on the chooser
/// sends. This *assumes* the prompt highlights "Wait" (keep the session,
/// wait for reset) as its default option, which is why the policy is
/// opt-in and off by default: if a future Claude build ever defaulted the
/// highlight to "Exit", Enter would take that instead. A keystroke, not a
/// settle-gated paste — a chooser answer must not wait for a composer.
const WAIT_KEYSTROKE: &[u8] = b"\r";

/// Spawn the auto-wait watcher. Always runs (the task is cheap and
/// re-reads `ui.auto_wait_on_limit` live), so opting in later needs no
/// restart.
pub fn spawn(config: &ServerConfig) -> tokio::task::JoinHandle<()> {
    // Subscribe here, not inside the task: an event broadcast between this
    // call returning and the task's first poll must queue, not vanish.
    let rx = config.bus.subscribe();
    let config = config.clone();
    tokio::spawn(async move {
        run(rx, config, auto_wait_enabled, press_wait, resume_work).await;
    })
}

/// Current `ui.auto_wait_on_limit` from YAML; an unreadable config means
/// off.
fn auto_wait_enabled() -> bool {
    lazybox_config::Config::load()
        .map(|c| c.ui.auto_wait_on_limit)
        .unwrap_or(false)
}

/// Send the Wait keystroke to a limit-blocked agent terminal. Takes an
/// owned config (a cheap `Arc`-backed clone) so the returned future owns
/// what it borrows — the injectable-press shape `run` needs.
async fn press_wait(config: ServerConfig, terminal_id: TerminalId) {
    tracing::info!(?terminal_id, "auto-wait: accepting the usage-limit prompt");
    crate::spawn_handler::handle_write(
        &config,
        terminal_id,
        WAIT_KEYSTROKE,
        TerminalInputIntent::Submit,
    )
    .await;
    // Relabel the block to the calm `AwaitingReset`: we've handled it, so
    // the agent is now parked waiting on the reset, not on the user. This
    // swaps the alerting `⏳` pill for the quiet 💤 badge and drops it out
    // of the alert count / resume-all set. Daemon-asserted and held against
    // the lingering banner by the state machine.
    crate::spawn_handler::park_limit_reached_as_awaiting_reset(&config, terminal_id).await;
}

/// Inject the continuation prompt into an agent whose usage-limit wait has
/// just cleared, so it picks the interrupted work back up instead of
/// sitting idle at a ready composer. Reuses `ui.credit_recovery_prompt`
/// (the same "Continue the work you were doing." nudge the credit-recovery
/// flow submits) and the settle-gated inject path — it waits for the
/// composer and confirms submission rather than blind-firing a keystroke.
async fn resume_work(config: ServerConfig, terminal_id: TerminalId) {
    let prompt = resume_prompt();
    tracing::info!(
        ?terminal_id,
        "auto-wait: usage limit cleared — injecting the continuation prompt"
    );
    crate::spawn_handler::handle_inject_prompt(&config, terminal_id, &prompt, None, true).await;
}

/// The continuation prompt submitted once the wait clears. Reads
/// `ui.credit_recovery_prompt` (blank falls back to its default via
/// `resolved_ui`); an unreadable config falls back to the same default
/// text so the resume never silently pastes nothing.
fn resume_prompt() -> String {
    lazybox_config::Config::load()
        .map(|c| c.resolved_ui().credit_recovery_prompt)
        .unwrap_or_else(|_| "Continue the work you were doing.".to_string())
}

/// After the wait clears, only a settled screen needs the continuation
/// nudge. `Done` is "the reset happened and the turn came to rest at an
/// empty composer" — exactly the case that would otherwise idle forever. A
/// clear to `Working` means the agent auto-resumed (re-auth / auto-continue),
/// so nudging it would inject a stray prompt mid-turn; every other clear (a
/// fresh permission prompt, a new credit block, an exit) is left alone.
fn wants_resume_nudge(state: &AgentState) -> bool {
    matches!(state, AgentState::Done)
}

/// Watch the event bus and, for each agent that enters `LimitReached`,
/// press Wait when the policy is enabled; then, when that same terminal's
/// wait clears to a resting screen, inject the continuation nudge. Generic
/// over the enabled check, the press action, and the resume action so the
/// decision logic is testable without a real terminal. Returns when the bus
/// closes (production never closes it — the daemon exits by dropping the
/// runtime).
async fn run<F, P, PFut, R, RFut>(
    mut rx: broadcast::Receiver<Event>,
    config: ServerConfig,
    enabled: F,
    press: P,
    resume: R,
) where
    F: Fn() -> bool,
    P: Fn(ServerConfig, TerminalId) -> PFut,
    PFut: std::future::Future<Output = ()>,
    R: Fn(ServerConfig, TerminalId) -> RFut,
    RFut: std::future::Future<Output = ()>,
{
    // Terminals we pressed Wait on and are still holding for their reset.
    // The rising edge into `LimitReached` inserts; the falling edge out of
    // it removes and (on a resting clear) fires the resume.
    let mut pending_resume: HashSet<TerminalId> = HashSet::new();
    loop {
        match rx.recv().await {
            Ok(Event::AgentState {
                terminal_id,
                state: AgentState::LimitReached,
                ..
            }) => {
                if enabled() {
                    press(config.clone(), terminal_id).await;
                    // Only track a wait we actually pressed, so the resume
                    // can't fire for a block the user is handling manually.
                    pending_resume.insert(terminal_id);
                }
            }
            // Our own `LimitReached → AwaitingReset` relabel (and any later
            // re-assertion of it): the block is still live, just parked, so
            // keep tracking and wait for the real clear.
            Ok(Event::AgentState {
                state: AgentState::AwaitingReset,
                ..
            }) => continue,
            Ok(Event::AgentState {
                terminal_id, state, ..
            }) => {
                // A tracked terminal left the limit block. Drop it either
                // way; nudge only when it came to rest (see
                // `wants_resume_nudge`) and the policy is still enabled.
                if pending_resume.remove(&terminal_id) && enabled() && wants_resume_nudge(&state) {
                    resume(config.clone(), terminal_id).await;
                }
            }
            Ok(_) => continue,
            // Best-effort: a lagged receiver may have dropped a
            // `LimitReached` transition, so auto-Wait can miss it (and, if
            // the dropped event was a clear, a tracked terminal lingers in
            // the set until it hits the limit again). Unlike `keep_awake`
            // we can't safely recompute from the states map (no per-terminal
            // "already pressed" latch, so a rescan could double-press). The
            // user's manual `Shift-K` bulk resume is the backstop.
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn state_event(terminal: u64, state: AgentState) -> Event {
        Event::AgentState {
            session_key: "ws:1".into(),
            terminal_id: TerminalId(terminal),
            state,
        }
    }

    fn limit_event(terminal: u64) -> Event {
        state_event(terminal, AgentState::LimitReached)
    }

    /// A resume action that does nothing — used by the press-only
    /// assertions, which never expect a nudge.
    async fn noop_resume(_cfg: ServerConfig, _tid: TerminalId) {}

    /// Only a `LimitReached` transition presses Wait — a working / asking /
    /// exited transition must not — and only while the flag is enabled.
    #[tokio::test]
    async fn presses_wait_only_for_limit_reached_when_enabled() {
        let config = ServerConfig::in_memory();
        let (tx, rx) = broadcast::channel(16);
        tx.send(state_event(1, AgentState::Working)).unwrap();
        tx.send(limit_event(2)).unwrap();
        tx.send(state_event(3, AgentState::InputNeeded)).unwrap();
        tx.send(limit_event(4)).unwrap();
        drop(tx);

        let pressed = RefCell::new(Vec::new());
        run(
            rx,
            config,
            || true,
            |_cfg, tid| {
                pressed.borrow_mut().push(tid);
                async {}
            },
            noop_resume,
        )
        .await;
        assert_eq!(
            pressed.into_inner(),
            vec![TerminalId(2), TerminalId(4)],
            "only the two LimitReached transitions press Wait",
        );
    }

    /// With the flag off, a `LimitReached` transition presses nothing —
    /// detection still surfaces the block, but the keystroke is opt-in.
    #[tokio::test]
    async fn disabled_flag_never_presses() {
        let config = ServerConfig::in_memory();
        let (tx, rx) = broadcast::channel(16);
        tx.send(limit_event(1)).unwrap();
        drop(tx);

        let pressed = RefCell::new(0usize);
        run(
            rx,
            config,
            || false,
            |_cfg, _tid| {
                *pressed.borrow_mut() += 1;
                async {}
            },
            noop_resume,
        )
        .await;
        assert_eq!(pressed.into_inner(), 0);
    }

    /// The full arc: a terminal we pressed Wait on, once its limit clears to
    /// `Done` (the reset happened and the turn came to rest), gets the
    /// continuation nudge injected — exactly once, for that terminal.
    #[tokio::test]
    async fn resumes_when_a_pressed_terminals_wait_clears_to_done() {
        let config = ServerConfig::in_memory();
        let (tx, rx) = broadcast::channel(16);
        tx.send(limit_event(7)).unwrap();
        tx.send(state_event(7, AgentState::Done)).unwrap();
        drop(tx);

        let pressed = RefCell::new(Vec::new());
        let resumed = RefCell::new(Vec::new());
        run(
            rx,
            config,
            || true,
            |_cfg, tid| {
                pressed.borrow_mut().push(tid);
                async {}
            },
            |_cfg, tid| {
                resumed.borrow_mut().push(tid);
                async {}
            },
        )
        .await;
        assert_eq!(pressed.into_inner(), vec![TerminalId(7)]);
        assert_eq!(
            resumed.into_inner(),
            vec![TerminalId(7)],
            "the wait clearing to Done injects the continuation nudge",
        );
    }

    /// The production shape: after pressing Wait we relabel the block to
    /// `AwaitingReset` (an `AgentState` event auto-wait receives back).
    /// That event must NOT drop the terminal from tracking — the block is
    /// still live — so a later clear to `Done` still fires the nudge.
    #[tokio::test]
    async fn awaiting_reset_relabel_keeps_tracking_until_the_real_clear() {
        let config = ServerConfig::in_memory();
        let (tx, rx) = broadcast::channel(16);
        tx.send(limit_event(9)).unwrap();
        tx.send(state_event(9, AgentState::AwaitingReset)).unwrap();
        tx.send(state_event(9, AgentState::AwaitingReset)).unwrap();
        tx.send(state_event(9, AgentState::Done)).unwrap();
        drop(tx);

        let resumed = RefCell::new(Vec::new());
        run(
            rx,
            config,
            || true,
            |_cfg, _tid| async {},
            |_cfg, tid| {
                resumed.borrow_mut().push(tid);
                async {}
            },
        )
        .await;
        assert_eq!(
            resumed.into_inner(),
            vec![TerminalId(9)],
            "the AwaitingReset relabel is ignored; the later Done still resumes",
        );
    }

    /// A limit that clears to `Working` means the agent auto-resumed on its
    /// own (re-auth / auto-continue) — no nudge. And a `Done` for a terminal
    /// we never pressed Wait on (limit block the user handled manually, or a
    /// plain finished turn) is never resumed either.
    #[tokio::test]
    async fn no_nudge_on_working_clear_or_untracked_done() {
        let config = ServerConfig::in_memory();
        let (tx, rx) = broadcast::channel(16);
        // Pressed, then auto-resumed → Working: no nudge.
        tx.send(limit_event(1)).unwrap();
        tx.send(state_event(1, AgentState::Working)).unwrap();
        // Never a limit block, just a finished turn: no nudge.
        tx.send(state_event(2, AgentState::Done)).unwrap();
        drop(tx);

        let resumed = RefCell::new(Vec::new());
        run(
            rx,
            config,
            || true,
            |_cfg, _tid| async {},
            |_cfg, tid| {
                resumed.borrow_mut().push(tid);
                async {}
            },
        )
        .await;
        assert!(
            resumed.into_inner().is_empty(),
            "neither a Working clear nor an untracked Done triggers a resume",
        );
    }

    /// Disabling the policy between the press and the clear cancels the
    /// resume — the flag is re-read live at both edges.
    #[tokio::test]
    async fn disabling_between_press_and_clear_cancels_resume() {
        let config = ServerConfig::in_memory();
        let (tx, rx) = broadcast::channel(16);
        tx.send(limit_event(5)).unwrap();
        tx.send(state_event(5, AgentState::Done)).unwrap();
        drop(tx);

        // Enabled for the press (which tracks the terminal), disabled by the
        // time the clear arrives.
        let calls = std::cell::Cell::new(0u32);
        let resumed = RefCell::new(0usize);
        run(
            rx,
            config,
            || {
                let n = calls.get();
                calls.set(n + 1);
                n == 0
            },
            |_cfg, _tid| async {},
            |_cfg, _tid| {
                *resumed.borrow_mut() += 1;
                async {}
            },
        )
        .await;
        assert_eq!(
            resumed.into_inner(),
            0,
            "a policy disabled before the clear injects no continuation",
        );
    }

    #[test]
    fn only_done_wants_a_resume_nudge() {
        assert!(wants_resume_nudge(&AgentState::Done));
        for state in [
            AgentState::Working,
            AgentState::InputNeeded,
            AgentState::Idle,
            AgentState::LimitReached,
            AgentState::CreditExhausted,
            AgentState::Exited { code: Some(0) },
        ] {
            assert!(
                !wants_resume_nudge(&state),
                "{state:?} must not trigger a resume nudge",
            );
        }
    }
}
