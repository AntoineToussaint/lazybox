//! Capability text lazybox injects into a spawned agent at session start,
//! so every agent — in any repo, with no `CLAUDE.md`/`AGENTS.md` blurb —
//! learns what lazybox lets it do beyond plain `git`/`gh`.
//!
//! It is delivered through the agent's own `SessionStart` lifecycle hook
//! (see `lazybox_server::lifecycle::ingest_hook_from_stdio`), which is the
//! spawn-intrinsic, repo-free channel lazybox already owns. One function so
//! Claude and (later) Codex say the exact same thing.

/// The `SessionStart` context lazybox teaches a spawned agent. Kept to a
/// few tight lines: what the extra command does and when to reach for it.
///
/// Constant for now; a later revision can swap this for an
/// `agent.session_context` config key or daemon-provided dynamic text
/// without touching either agent's hook path.
pub fn lazybox_session_context() -> &'static str {
    "You are running inside lazybox, a PR inbox that hosts this session in its own \
terminal. lazybox can stream a command's output into a separate, live, \
human-visible window instead of you capturing it inline. Pipe a noisy command — a \
build, a test run, a tailable log — to `lazybox log`; its output streams in its own \
tile and never enters your context:\n\
\n\
    cargo test 2>&1 | lazybox log --title tests\n\
\n\
The pipe stays open until the command exits, so background a long-running process \
(a dev server, a watcher) with a trailing `&` — `npm run dev 2>&1 | lazybox log &` \
— or it blocks your turn. Close all log windows with `lazybox log --close-all`."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_names_the_command_and_stays_short() {
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
        // A SessionStart blurb rides in the model's context on every launch;
        // keep it a few lines, not a manual.
        assert!(
            text.lines().count() <= 8,
            "session context should stay tight: {} lines",
            text.lines().count()
        );
    }
}
