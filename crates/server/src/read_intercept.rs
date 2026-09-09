//! The `PreToolUse` large-read intercept (#1610) — the sharper of the two
//! enforcement points in the context-hygiene epic (#1611).
//!
//! The proxy compactor rewrites a large tool result *after* the agent has
//! already paid to put it in the transcript. Claude's `PreToolUse` hook can
//! do better: it can refuse the read outright and hand the model the
//! condensed text as the refusal's reason, so the raw file never enters the
//! conversation at all. Both layers evaluate the same policy
//! ([`lazybox_core::context_hygiene`]) and render through the same
//! `render_condensed`, so whichever one fires the model sees identical
//! bytes.
//!
//! Everything here is written to fail *open*. A hook that denies wrongly is
//! far worse than one that never fires: the agent loses a file it needed and
//! has no way to know why. So every uncertainty — an unresolvable backend
//! key, an unreadable path, a summarizer that errored — resolves to
//! [`ToolUseDecision::Allow`], and the helper on the other end gives up after
//! its own deadline regardless.

use crate::ServerConfig;
use futures::future::BoxFuture;
use lazybox_core::WorkspaceKey;
use lazybox_core::context_hygiene::{CondenseKind, ToolResultFacts};
use lazybox_ipc::{Event, ToolUseDecision, ToolUseRequest};

/// Where the condensed text comes from: the cheap-model summarizer service
/// (#1608), registered on [`ServerConfig::condenser`] at daemon start.
///
/// `None` means "could not condense" — a rate limit, a timeout, an upstream
/// 5xx — and is not an error to report: the intercept simply allows the read
/// through, which is the same context bill the agent would have paid anyway.
/// The returned string is the fully rendered block
/// ([`lazybox_core::context_hygiene::render_condensed`]), not a bare summary,
/// so the hook and the proxy compactor emit the same bytes for the same file.
pub trait Condense: Send + Sync {
    fn condense<'a>(
        &'a self,
        kind: &'a CondenseKind,
        input: &'a str,
    ) -> BoxFuture<'a, Option<String>>;
}

/// What the model is told after a condensed redirect. `render_condensed`'s
/// own header points at re-reading the file, which is true of the proxy
/// compactor but would loop straight back into another deny here — this
/// layer only ever lets a *ranged* read through, so it has to say so.
pub const REREAD_AFFORDANCE: &str = "lazybox condensed this file instead of reading it. \
     To get the real bytes for a region, call Read again on the same path with an explicit \
     `offset` and `limit` — a ranged read is never intercepted.";

/// Largest file the intercept will pull into the daemon to condense. Past
/// this the read is allowed through: the summarizer would truncate it to a
/// fraction of itself anyway, and buffering an arbitrarily large file to
/// decide one hook is a memory cost the agent didn't ask for.
const MAX_CONDENSE_INPUT_BYTES: u64 = 8 * 1024 * 1024;

/// Handle [`lazybox_ipc::Command::DecideToolUse`]: rule on one about-to-run
/// tool call and answer on the same connection the helper is blocked on.
pub async fn handle_decide_tool_use(
    config: &ServerConfig,
    tx: &lazybox_ipc::EventSender,
    backend_key: Option<String>,
    request: ToolUseRequest,
    client_request_id: String,
) {
    let decision = decide(config, backend_key.as_deref(), &request).await;
    let _ = tx.send(Event::ToolUseDecided {
        client_request_id,
        decision,
    });
}

/// Whether the intercept may act at all. `mode.rewrites()`, not merely
/// `evaluates()`: shadow mode's contract is to decide everything `On` decides
/// and change no bytes, and a shadow deny would change everything. The hook
/// stays silent in shadow rather than round-tripping to log a verdict — the
/// proxy compactor already observes the same population off the request body,
/// without stopping the agent's turn to do it.
fn armed(policy: &lazybox_core::ContextHygiene) -> bool {
    policy.hook_intercept && policy.mode.rewrites()
}

/// Resolve everything the ruling depends on off the daemon's state, then
/// rule. The lookups are separated from [`rule`] because they are what makes
/// a decision untestable in isolation, and the rule itself is where every
/// interesting case lives.
async fn decide(
    config: &ServerConfig,
    backend_key: Option<&str>,
    request: &ToolUseRequest,
) -> ToolUseDecision {
    // Nothing to redirect the model *to* — never deny a read we can't
    // replace with condensed content.
    let Some(condenser) = config.condenser.clone() else {
        return ToolUseDecision::Allow;
    };
    let Some(key) = backend_key else {
        return ToolUseDecision::Allow;
    };
    let Some(terminal_id) = crate::spawn_handler::terminal_for_backend_key(config, key).await
    else {
        return ToolUseDecision::Allow;
    };
    let Some((session_key, _)) = config.terminal.terminal_meta_for(terminal_id).await else {
        return ToolUseDecision::Allow;
    };
    let cfg = lazybox_config::Config::load().unwrap_or_default();
    let metered =
        crate::spawn_handler::load_workspace(config, &WorkspaceKey::new(session_key.as_str()))
            .is_ok_and(|workspace| crate::spawn_handler::workspace_is_metered(&cfg, &workspace));

    rule(
        &cfg.agent.context_hygiene,
        metered,
        condenser.as_ref(),
        request,
    )
    .await
}

/// The ruling itself, over resolved inputs. `metered` is the workspace's
/// opt-in ([`crate::spawn_handler::workspace_is_metered`]).
async fn rule(
    policy: &lazybox_core::ContextHygiene,
    metered: bool,
    condenser: &dyn Condense,
    request: &ToolUseRequest,
) -> ToolUseDecision {
    if !armed(policy) || !metered {
        return ToolUseDecision::Allow;
    }
    // The helper filters these out locally; a build-skewed helper is still a
    // process boundary, and denying an `Edit` would be unrecoverable.
    if request.tool_name != "Read" {
        return ToolUseDecision::Allow;
    }
    let Some(contents) = read_for_condensing(&request.file_path).await else {
        return ToolUseDecision::Allow;
    };
    let kind = CondenseKind::FileRead {
        path: request.file_path.clone(),
    };
    let facts = ToolResultFacts::pending(&kind, contents.lines().count());
    if !policy.eligibility(&facts).is_condense() {
        return ToolUseDecision::Allow;
    }
    let Some(condensed) = condenser.condense(&kind, &contents).await else {
        return ToolUseDecision::Allow;
    };
    ToolUseDecision::Deny {
        reason: format!("{condensed}\n\n{REREAD_AFFORDANCE}"),
    }
}

/// Read the file the agent was about to read, off the runtime worker.
/// `None` for anything the intercept must not act on: a path that isn't a
/// readable regular file, or one too large to condense.
async fn read_for_condensing(path: &str) -> Option<String> {
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        let metadata = std::fs::metadata(&path).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_CONDENSE_INPUT_BYTES {
            return None;
        }
        std::fs::read_to_string(&path).ok()
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_core::context_hygiene::{CompactionMode, ContextHygiene};

    /// A condenser that always succeeds, so tests exercise the policy rather
    /// than the summarizer.
    struct FixedCondenser(&'static str);

    impl Condense for FixedCondenser {
        fn condense<'a>(
            &'a self,
            _kind: &'a CondenseKind,
            _input: &'a str,
        ) -> BoxFuture<'a, Option<String>> {
            Box::pin(async move { Some(self.0.to_string()) })
        }
    }

    /// A condenser that always fails — the summarizer's documented failure
    /// mode (rate limit, timeout, upstream 5xx).
    struct FailingCondenser;

    impl Condense for FailingCondenser {
        fn condense<'a>(
            &'a self,
            _kind: &'a CondenseKind,
            _input: &'a str,
        ) -> BoxFuture<'a, Option<String>> {
            Box::pin(async move { None })
        }
    }

    fn on_policy(min_lines: usize) -> ContextHygiene {
        ContextHygiene {
            mode: CompactionMode::On,
            min_lines,
            ..ContextHygiene::default()
        }
    }

    fn file_read() -> CondenseKind {
        CondenseKind::FileRead {
            path: "/w/big.rs".into(),
        }
    }

    /// A file of `lines` lines, plus a `Read` of the whole thing.
    fn read_of(dir: &tempfile::TempDir, lines: usize) -> ToolUseRequest {
        let path = dir.path().join("big.rs");
        std::fs::write(&path, "line\n".repeat(lines)).expect("write file");
        ToolUseRequest {
            tool_name: "Read".into(),
            file_path: path.to_string_lossy().into_owned(),
        }
    }

    #[tokio::test]
    async fn a_large_read_is_denied_with_the_condensed_text_and_the_affordance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let request = read_of(&dir, 400);
        let decision = rule(
            &on_policy(350),
            true,
            &FixedCondenser("[condensed by lazybox: big.rs, 400 lines → 1 lines]\nit repeats"),
            &request,
        )
        .await;
        let ToolUseDecision::Deny { reason } = decision else {
            panic!("a 400-line read past a 350-line floor must be denied: {decision:?}");
        };
        assert!(
            reason.starts_with("[condensed by lazybox: "),
            "the condenser's rendered bytes must lead, unmodified: {reason}"
        );
        assert!(
            reason.contains("`offset`") && reason.contains("`limit`"),
            "the model must be told the one read that gets through: {reason}"
        );
    }

    #[tokio::test]
    async fn a_read_below_the_line_floor_passes_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let request = read_of(&dir, 349);
        assert_eq!(
            rule(&on_policy(350), true, &FixedCondenser("summary"), &request).await,
            ToolUseDecision::Allow
        );
    }

    #[tokio::test]
    async fn an_unmetered_workspace_is_never_intercepted() {
        // Metering is the per-workspace opt-in: both enforcement points
        // condense through the agent's own upstream, which only a proxied
        // session has.
        let dir = tempfile::tempdir().expect("tempdir");
        let request = read_of(&dir, 4_000);
        assert_eq!(
            rule(&on_policy(350), false, &FixedCondenser("summary"), &request).await,
            ToolUseDecision::Allow
        );
    }

    #[tokio::test]
    async fn only_a_read_is_ever_denied() {
        // The helper never submits these, but the daemon sits behind a
        // process boundary and a denied `Edit` would be unrecoverable.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut request = read_of(&dir, 4_000);
        for tool in ["Edit", "Write", "Bash", "NotebookEdit"] {
            request.tool_name = tool.into();
            assert_eq!(
                rule(&on_policy(350), true, &FixedCondenser("summary"), &request).await,
                ToolUseDecision::Allow,
                "{tool} must pass through"
            );
        }
    }

    #[tokio::test]
    async fn a_failing_condenser_passes_the_read_through() {
        // The summarizer's failure mode is pass-through: the agent pays the
        // context it would have paid anyway, never a denied read.
        let dir = tempfile::tempdir().expect("tempdir");
        let request = read_of(&dir, 4_000);
        assert_eq!(
            rule(&on_policy(350), true, &FailingCondenser, &request).await,
            ToolUseDecision::Allow
        );
    }

    #[tokio::test]
    async fn an_unreadable_path_passes_through() {
        let request = ToolUseRequest {
            tool_name: "Read".into(),
            file_path: "/no/such/file".into(),
        };
        assert_eq!(
            rule(&on_policy(1), true, &FixedCondenser("summary"), &request).await,
            ToolUseDecision::Allow
        );
    }

    #[tokio::test]
    async fn shadow_mode_denies_nothing() {
        // Shadow is the epic's default and it *evaluates* — `eligibility()`
        // says Condense here. `armed()` is the only thing standing between
        // that verdict and a denied read, so this is what keeps the default
        // configuration byte-neutral.
        let kind = file_read();
        let shadow = ContextHygiene {
            mode: CompactionMode::Shadow,
            min_lines: 350,
            ..ContextHygiene::default()
        };
        assert!(
            shadow
                .eligibility(&ToolResultFacts::pending(&kind, 4_000))
                .is_condense(),
            "shadow still decides"
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let request = read_of(&dir, 4_000);
        assert_eq!(
            rule(&shadow, true, &FixedCondenser("summary"), &request).await,
            ToolUseDecision::Allow,
            "but it must never deny"
        );
    }

    #[test]
    fn the_intercept_is_disarmed_off_and_by_hook_intercept() {
        assert!(armed(&on_policy(350)));
        assert!(!armed(&ContextHygiene {
            mode: CompactionMode::Off,
            ..ContextHygiene::default()
        }));
        assert!(
            !armed(&ContextHygiene {
                hook_intercept: false,
                ..on_policy(350)
            }),
            "hook_intercept is the per-agent opt-out"
        );
    }

    #[test]
    fn a_pre_read_is_never_held_back_by_the_recency_window() {
        // The compactor's recency window protects results the model is
        // mid-task on; a read that has not happened has no position in the
        // conversation, so the size floor is the only gate at this layer.
        let policy = on_policy(400);
        let kind = file_read();
        assert!(
            policy
                .eligibility(&ToolResultFacts::pending(&kind, 401))
                .is_condense()
        );
        assert!(
            !policy
                .eligibility(&ToolResultFacts::pending(&kind, 399))
                .is_condense()
        );
    }

    #[tokio::test]
    async fn only_readable_regular_files_are_pulled_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("small.txt");
        std::fs::write(&path, "one\ntwo\n").expect("write");
        assert_eq!(
            read_for_condensing(&path.to_string_lossy())
                .await
                .as_deref(),
            Some("one\ntwo\n")
        );
        assert_eq!(
            read_for_condensing(&dir.path().to_string_lossy()).await,
            None,
            "a directory is not a file"
        );
        assert_eq!(read_for_condensing("/no/such/file").await, None);
    }
}
