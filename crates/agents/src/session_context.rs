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
human-visible window instead of you capturing it inline. When you run a long or \
noisy command — a dev server, a test watcher, a build, a tailable log — pipe it to \
`lazybox log` so its output streams in its own tile and never enters your context:\n\
\n\
    npm run dev 2>&1 | lazybox log --title dev\n\
\n\
Close every log window this workspace opened with `lazybox log --close-all`."
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
        // A SessionStart blurb rides in the model's context on every launch;
        // keep it a few lines, not a manual.
        assert!(
            text.lines().count() <= 8,
            "session context should stay tight: {} lines",
            text.lines().count()
        );
    }
}
