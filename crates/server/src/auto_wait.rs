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

use lazybox_ipc::{Event, TerminalId, TerminalInputIntent};
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
        run(rx, config, auto_wait_enabled, press_wait).await;
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
}

/// Watch the event bus and, for each agent that enters `LimitReached`,
/// press Wait when the policy is enabled. Generic over the enabled check
/// and the press action so the decision logic is testable without a real
/// terminal. Returns when the bus closes (production never closes it — the
/// daemon exits by dropping the runtime).
async fn run<F, P, Fut>(
    mut rx: broadcast::Receiver<Event>,
    config: ServerConfig,
    enabled: F,
    press: P,
) where
    F: Fn() -> bool,
    P: Fn(ServerConfig, TerminalId) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    loop {
        let terminal_id = match rx.recv().await {
            Ok(Event::AgentState {
                terminal_id,
                state: lazybox_ipc::AgentState::LimitReached,
                ..
            }) => terminal_id,
            Ok(_) => continue,
            // Best-effort: a lagged receiver may have dropped a
            // `LimitReached` transition, so auto-Wait can miss it. Unlike
            // `keep_awake` we can't safely recompute from the states map
            // (no per-terminal "already pressed" latch, so a rescan could
            // double-press). The user's manual `Shift-K` bulk resume is
            // the backstop.
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        };
        if enabled() {
            press(config.clone(), terminal_id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_ipc::AgentState;
    use std::cell::RefCell;

    fn limit_event(terminal: u64) -> Event {
        Event::AgentState {
            session_key: "ws:1".into(),
            terminal_id: TerminalId(terminal),
            state: AgentState::LimitReached,
        }
    }

    /// Only a `LimitReached` transition presses Wait — a working / asking /
    /// exited transition must not — and only while the flag is enabled.
    #[tokio::test]
    async fn presses_wait_only_for_limit_reached_when_enabled() {
        let config = ServerConfig::in_memory();
        let (tx, rx) = broadcast::channel(16);
        tx.send(Event::AgentState {
            session_key: "ws:1".into(),
            terminal_id: TerminalId(1),
            state: AgentState::Working,
        })
        .unwrap();
        tx.send(limit_event(2)).unwrap();
        tx.send(Event::AgentState {
            session_key: "ws:1".into(),
            terminal_id: TerminalId(3),
            state: AgentState::InputNeeded,
        })
        .unwrap();
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
        )
        .await;
        assert_eq!(pressed.into_inner(), 0);
    }
}
