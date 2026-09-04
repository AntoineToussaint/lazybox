//! Capability text lazybox injects into a spawned agent at session start,
//! so every agent — in any repo, with no `CLAUDE.md`/`AGENTS.md` blurb —
//! learns what lazybox lets it do beyond plain `git`/`gh`.
//!
//! It is delivered through the agent's own `SessionStart` lifecycle hook
//! (see `lazybox_server::lifecycle::ingest_hook_from_stdio`), which is the
//! spawn-intrinsic, repo-free channel lazybox already owns. One function so
//! Claude and (later) Codex say the exact same thing.

/// The `SessionStart` context lazybox teaches every spawned agent — the
/// "always" half of what an agent is told, riding the spawn-intrinsic hook
/// so `a c`/`s` get it as surely as a `w w` work prompt does. It carries
/// lazybox's *mechanics* (the load-bearing labels an agent must not strip,
/// the policies that can act on a PR without it, the `@lazybox` trigger and
/// its hazard, and the extra handles beyond `git`/`gh`), not the per-task
/// brief — that stays in the work prompt.
///
/// Kept tight because it is paid on every launch: a mechanics reference, not
/// a manual. Constant for now; a later revision can swap this for an
/// `agent.session_context` config key or daemon-provided dynamic text (this
/// workspace's PR, branch, live policies, current labels) without touching
/// either agent's hook path.
pub fn lazybox_session_context() -> &'static str {
    "You are running inside lazybox, a reactive PR inbox that hosts this session in \
its own terminal. A few lazybox mechanics coordinate work across a fleet of agents — \
know them before you touch labels or post comments.\n\
\n\
Load-bearing GitHub labels — never strip these, they are live coordination state, \
not junk:\n\
  - `working` and `lazybox:w:…` mark a task as owned by a running agent \
(heartbeat-renewed, 1-hour TTL). Removing one lets the fleet double-spawn on a task \
it now thinks is free.\n\
  - `no-auto-fix` / `do-not-lazybox` opt a PR out of lazybox's auto-fix only (not \
auto-merge, not `@lazybox`). Add one to stop lazybox auto-fixing a PR; remove it to \
let it resume.\n\
\n\
Standing policies (set in lazybox, not GitHub labels; shown as `ARM` / `FIX` pills) \
can act on a PR without you: auto-merge-on-green merges it once CI passes, and \
auto-fix spawns an agent to repair failing CI. So a PR merging itself or an agent \
starting on its own is configured behavior, not a glitch.\n\
\n\
`@lazybox` in an issue or PR comment makes lazybox react 👀 and spawn an agent. You \
post as the authenticated lazybox user, which is an allowed login — do not write the \
literal `@lazybox` in a comment unless you intend to start one.\n\
\n\
Handles beyond `git`/`gh`:\n\
  - `lazybox log` streams a noisy command to its own window instead of your context \
— `cargo test 2>&1 | lazybox log --title tests`. Background long-running pipes with \
a trailing `&` or they block your turn; `lazybox log --close-all` clears them.\n\
  - `lazybox workspace create --name \"…\" [--agent claude]` starts a fresh line of \
work — reach for it instead of filing an issue.\n\
  - Snippets (`]]s`, `~/.lazybox/snippets.yaml`) and skills (`.claude/skills/`) drive \
you; a prompt you did not type yourself may have come from one."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_teaches_lazybox_log() {
        let text = lazybox_session_context();
        assert!(text.contains("lazybox log"), "must name the command");
        assert!(
            text.contains("lazybox log --close-all"),
            "must name the cleanup path"
        );
        // The pipe couples the helper's lifetime to the command: a foreground
        // pipe of a non-terminating process blocks the agent's turn until it is
        // killed. The blurb must warn about that and show backgrounding (`&`),
        // rather than leading with a bare `npm run dev` foreground pipe.
        assert!(
            text.contains('&') && text.to_ascii_lowercase().contains("block"),
            "must warn that a long-running pipe blocks the turn and show backgrounding: {text}"
        );
    }

    #[test]
    fn context_names_the_load_bearing_coordination_state() {
        let text = lazybox_session_context();
        // The whole point of the "always" half: an agent that can strip a claim
        // label or trip the mention trigger without knowing what it just did is
        // the bug this text exists to prevent. Each of these must be named.
        for needle in [
            "working",
            "lazybox:w:",
            "no-auto-fix",
            "do-not-lazybox",
            "@lazybox",
            "auto-merge",
            "auto-fix",
            "lazybox workspace create",
        ] {
            assert!(
                text.contains(needle),
                "session context must name `{needle}`: {text}"
            );
        }
    }

    #[test]
    fn context_stays_tight() {
        // A SessionStart blurb rides in the model's context on every launch of
        // every agent, so it must stay a mechanics reference, not a manual.
        // The cap is generous enough for the coordination vocabulary but tight
        // enough to fail if the blurb grows into prose.
        let text = lazybox_session_context();
        assert!(
            text.lines().count() <= 30,
            "session context should stay tight: {} lines",
            text.lines().count()
        );
        assert!(
            text.len() <= 2000,
            "session context should stay tight: {} bytes",
            text.len()
        );
    }
}
