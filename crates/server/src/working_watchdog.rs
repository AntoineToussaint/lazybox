//! Global working-watchdog sweep (#1383).
//!
//! A finished agent is normally promoted out of `Working` by its own PTY
//! pump: the quiet timer classifies the resting screen, and the
//! content-stability watchdog force-closes a wedged status line. Both run
//! inside the *per-terminal* pump task, so with a dozen-plus agents and a
//! saturated CPU (the hook+poll storm of #1366) those tasks are descheduled
//! and neither timer fires — the last `Working` reading sticks and the agent
//! spins forever.
//!
//! This one low-frequency task is the starvation backstop. It is independent
//! of any pump's scheduling: it reads the shared turn clock and, for every
//! terminal still cached `Working` whose meaningful content has been at rest
//! well past the per-terminal watchdog window, force-closes the turn through
//! the same out-of-pump state-commit path the hook ingest uses. The
//! content-stability age it keys on is reset by meaningful output and by
//! affirmative lifecycle hooks (both recorded outside the pump too), so a
//! genuinely busy agent never accrues it — only a truly stalled one does.

use std::time::Duration;

use crate::ServerConfig;

/// How often the sweep re-scans the registry. Low enough to be negligible
/// (one locked pass over the terminal map), frequent enough that a starved
/// terminal is caught within a tick of crossing the stall bound.
const SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// The stall bound is this multiple of the per-terminal watchdog window. A
/// healthy pump promotes a finished turn within one watchdog window (or one
/// quiet window, sooner), so by two windows any terminal still `Working` had
/// its pump starved past the point the pump itself would have acted. Keeping
/// the sweep strictly behind the pump means it never races a healthy pump's
/// own — properly re-classifying — promotion; it only ever cleans up after a
/// pump that never got the chance.
const SWEEP_STALL_MULTIPLIER: u32 = 2;

/// Spawn the sweep. Disabled exactly when the per-terminal watchdog is
/// (`agent.working_watchdog_secs = 0`): that override opts out of
/// content-stability forcing entirely, and the sweep is that same force made
/// starvation-proof, so it honors the opt-out. The task still exists (a
/// parked future) so the runtime owns and aborts it uniformly.
pub fn spawn(config: &ServerConfig) -> tokio::task::JoinHandle<()> {
    let cfg = lazybox_config::Config::load().unwrap_or_default();
    let stall_bound = crate::spawn_handler::working_watchdog_after(&cfg)
        .map(|window| window.saturating_mul(SWEEP_STALL_MULTIPLIER));
    let config = config.clone();
    tokio::spawn(async move {
        match stall_bound {
            Some(bound) => run(config, SWEEP_INTERVAL, bound).await,
            None => std::future::pending::<()>().await,
        }
    })
}

/// Force-close every stalled `Working` terminal once per `interval`. Never
/// returns in production; the daemon drops the runtime to exit, which drops
/// this future mid-tick.
async fn run(config: ServerConfig, interval: Duration, stall_bound: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        for agent in config.terminal.stalled_working_agents(stall_bound).await {
            if crate::spawn_handler::force_stalled_working_to_done(&config, &agent).await {
                tracing::warn!(
                    target: "lazybox::agent_status_telemetry",
                    terminal_id = ?agent.id,
                    reason = "global-sweep-force",
                    stall_bound_ms = stall_bound.as_millis(),
                    "global working-watchdog sweep force-closed a starved Working agent",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registries::AgentTurnEvent;
    use lazybox_core::SessionKey;
    use lazybox_ipc::{AgentState, Event, TerminalId, TerminalKind};

    const QUIET: Duration = Duration::from_secs(5);
    const WATCHDOG: Duration = Duration::from_secs(15);

    async fn make_working_agent(config: &ServerConfig, id: TerminalId, generation: u64) {
        config
            .terminal
            .register_terminal(
                id,
                format!("backend-{}", id.0),
                SessionKey::from(format!("github:o/r#{}", id.0)),
                TerminalKind::Agent("claude".into()),
            )
            .await;
        config
            .terminal
            .record_agent_state(id, AgentState::Working)
            .await;
        config
            .terminal
            .record_agent_state_generation(id, generation)
            .await;
        config
            .terminal
            .configure_agent_turn(id, QUIET, Some(WATCHDOG))
            .await;
    }

    /// The sweep's selection excludes a working agent whose meaningful
    /// content is fresh and includes one that has been at rest past the
    /// stall bound; a shell is never a candidate.
    #[tokio::test(start_paused = true)]
    async fn stalled_selection_tracks_content_stability() {
        let config = ServerConfig::in_memory();
        let bound = WATCHDOG * SWEEP_STALL_MULTIPLIER;

        make_working_agent(&config, TerminalId(1), 1).await;
        make_working_agent(&config, TerminalId(2), 1).await;
        config
            .terminal
            .register_terminal(
                TerminalId(3),
                "shell".into(),
                SessionKey::from("github:o/r#3"),
                TerminalKind::Shell,
            )
            .await;

        // Past the bound with no meaningful output: both agents are stalled.
        tokio::time::advance(bound + Duration::from_secs(1)).await;
        let stalled = config.terminal.stalled_working_agents(bound).await;
        assert_eq!(stalled.len(), 2, "both working agents are past the bound");

        // Fresh meaningful output on terminal 2 re-anchors its stability
        // clock, so it drops out of the next sweep; the shell never counts.
        config
            .terminal
            .record_turn_event(
                TerminalId(2),
                AgentTurnEvent::OutputChunk {
                    backend_seq: 1,
                    meaningful_progress: true,
                },
            )
            .await;
        let stalled = config.terminal.stalled_working_agents(bound).await;
        assert_eq!(
            stalled.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![TerminalId(1)],
            "a just-progressed agent is no longer stalled; the shell is excluded",
        );
    }

    /// Under simulated pump starvation — no pump ever runs the per-terminal
    /// timers — one sweep tick still force-closes the wedged `Working`
    /// terminal to `Done` and broadcasts the transition.
    #[tokio::test(start_paused = true)]
    async fn sweep_promotes_a_starved_working_agent() {
        let config = ServerConfig::in_memory();
        let mut rx = config.bus.subscribe();
        make_working_agent(&config, TerminalId(1), 1).await;

        let task = {
            let config = config.clone();
            tokio::spawn(async move {
                run(config, SWEEP_INTERVAL, WATCHDOG * SWEEP_STALL_MULTIPLIER).await;
            })
        };

        // Let the stall bound elapse and the sweep tick fire. Under
        // `start_paused` this auto-advances the clock and lets the spawned
        // sweep task run its interval ticks in between.
        tokio::time::sleep(WATCHDOG * SWEEP_STALL_MULTIPLIER + SWEEP_INTERVAL * 2).await;

        assert_eq!(
            config.terminal.agent_state_for(TerminalId(1)).await,
            Some(AgentState::Done),
            "the sweep force-closed the starved Working terminal",
        );
        let mut saw_done = false;
        while let Ok(event) = rx.try_recv() {
            if let Event::AgentState {
                terminal_id: TerminalId(1),
                state: AgentState::Done,
                ..
            } = event
            {
                saw_done = true;
            }
        }
        assert!(saw_done, "the forced Done was broadcast");

        task.abort();
    }

    /// A `Working` agent whose stability clock is younger than the bound is
    /// left alone — the sweep force-closes only genuinely stalled turns.
    #[tokio::test(start_paused = true)]
    async fn sweep_leaves_a_still_fresh_working_agent() {
        let config = ServerConfig::in_memory();
        make_working_agent(&config, TerminalId(1), 1).await;

        tokio::time::advance(WATCHDOG).await; // one window — under the 2× bound
        for agent in config
            .terminal
            .stalled_working_agents(WATCHDOG * SWEEP_STALL_MULTIPLIER)
            .await
        {
            crate::spawn_handler::force_stalled_working_to_done(&config, &agent).await;
        }
        assert_eq!(
            config.terminal.agent_state_for(TerminalId(1)).await,
            Some(AgentState::Working),
            "an agent under the stall bound must not be force-closed",
        );
    }
}
