use crate::{Credential, CredentialError, CredentialProvider};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a successfully resolved command credential is served from
/// cache before the command runs again. `gh auth token` rotates rarely
/// and used to be spawned fresh on *every* poll tick — dozens of
/// subprocesses per minute, each a 5s liability under system load.
const SUCCESS_TTL: Duration = Duration::from_secs(300);

/// Failure backoff: 30s doubling to a 5-minute cap. Without it, a
/// timing-out `gh auth token` was respawned every tick — a subprocess
/// storm exactly when the machine was already under pressure.
const FAILURE_BACKOFF_BASE: Duration = Duration::from_secs(30);
const FAILURE_BACKOFF_CAP: Duration = Duration::from_secs(300);

struct CacheEntry {
    outcome: Result<Credential, String>,
    at: Instant,
    consecutive_failures: u32,
}

fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn failure_backoff(consecutive: u32) -> Duration {
    FAILURE_BACKOFF_BASE
        .saturating_mul(1u32 << consecutive.saturating_sub(1).min(4))
        .min(FAILURE_BACKOFF_CAP)
}

/// Drop every cached command-credential outcome. Wired to explicit
/// user refresh: someone who just ran `gh auth login` and hits refresh
/// must not wait out a failure-backoff window.
pub fn invalidate_command_credential_cache() {
    cache().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// Resolves a credential by running a shell command and reading stdout.
///
/// ```rust,no_run
/// use lazybox_auth::CommandProvider;
/// // Use `gh auth token` to get a GitHub token
/// let provider = CommandProvider::new("gh", &["auth", "token"]);
/// // Use `vault read` to get a secret
/// // let provider = CommandProvider::new("vault", &["read", "-field=token", "secret/github"]);
/// ```
pub struct CommandProvider {
    program: String,
    args: Vec<String>,
    /// Label for logging/display — and the process-global cache key.
    label: String,
    /// Opt-out for callers that need every resolve to run the command
    /// (tests, one-shot CLI flows).
    cached: bool,
}

impl CommandProvider {
    pub fn new(program: impl Into<String>, args: &[&str]) -> Self {
        let program = program.into();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let label = format!("{} {}", program, args.join(" "));
        Self {
            program,
            args,
            label,
            cached: true,
        }
    }

    /// Override the display label.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Disable the process-global result cache for this provider.
    pub fn uncached(mut self) -> Self {
        self.cached = false;
        self
    }

    /// Serve from the process-global cache: a fresh success inside
    /// [`SUCCESS_TTL`], or the cached error while the failure backoff
    /// window is open. `None` means "run the command".
    fn cached_outcome(&self) -> Option<Result<Credential, CredentialError>> {
        if !self.cached {
            return None;
        }
        let cache = cache().lock().unwrap_or_else(|e| e.into_inner());
        let entry = cache.get(&self.label)?;
        match &entry.outcome {
            Ok(cred) if entry.at.elapsed() < SUCCESS_TTL => Some(Ok(cred.clone())),
            Ok(_) => None,
            Err(msg) => {
                if entry.at.elapsed() < failure_backoff(entry.consecutive_failures) {
                    Some(Err(CredentialError::Provider(format!(
                        "{msg} (cached failure; retrying after backoff)"
                    ))))
                } else {
                    None
                }
            }
        }
    }

    fn record_success(&self, cred: &Credential) {
        if !self.cached {
            return;
        }
        let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(
            self.label.clone(),
            CacheEntry {
                outcome: Ok(cred.clone()),
                at: Instant::now(),
                consecutive_failures: 0,
            },
        );
    }

    fn record_failure(&self, msg: &str) {
        if !self.cached {
            return;
        }
        let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
        let consecutive = cache
            .get(&self.label)
            .filter(|entry| entry.outcome.is_err())
            .map(|entry| entry.consecutive_failures.saturating_add(1))
            .unwrap_or(1);
        cache.insert(
            self.label.clone(),
            CacheEntry {
                outcome: Err(msg.to_string()),
                at: Instant::now(),
                consecutive_failures: consecutive,
            },
        );
    }
}

impl CredentialProvider for CommandProvider {
    fn name(&self) -> &str {
        "command"
    }

    async fn resolve(&self, _scope: &str) -> Result<Credential, CredentialError> {
        if let Some(outcome) = self.cached_outcome() {
            return outcome;
        }
        // 5s timeout. `gh auth token` is usually <100ms but can hang
        // if the network's down or `gh` is misconfigured. Without
        // this, the daemon's polling task blocks indefinitely on
        // first launch — lazybox looks frozen.
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        // kill_on_drop so a timeout actually kills the child. Without
        // this, every hung `gh auth token` leaks a process that lives
        // until it exits on its own — and on a flaky network they can
        // pile up.
        // stdin is nulled so a credential helper that decides to
        // prompt interactively (`gh` re-auth flow, a vault asking for
        // a passphrase) reads EOF and fails fast instead of waiting
        // on the inherited TTY — invisible under the TUI's alternate
        // screen.
        //
        // Own process group + group sweep on the non-success paths:
        // `gh` shells out to credential helpers, and `kill_on_drop`
        // alone signals only the direct child — a timed-out helper
        // survived it orphaned (the same class git-ops fixed for git
        // transport helpers).
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&self.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        struct KillGroupOnDrop(Option<i32>);
        impl Drop for KillGroupOnDrop {
            fn drop(&mut self) {
                #[cfg(unix)]
                if let Some(pgid) = self.0 {
                    unsafe { libc::killpg(pgid, libc::SIGKILL) };
                }
            }
        }
        let run = async {
            let child = cmd.spawn()?;
            let mut group = KillGroupOnDrop(child.id().map(|id| id as i32));
            let output = child.wait_with_output().await;
            // Disarm only on a clean exit; a non-zero or errored wait
            // may have left helpers behind, and sweeping a dead
            // leader's group costs one syscall that can only reach
            // processes this spawn created.
            if matches!(&output, Ok(o) if o.status.success()) {
                group.0 = None;
            }
            output
        };
        let output = match tokio::time::timeout(TIMEOUT, run).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                let msg = format!("failed to run `{}`: {e}", self.label);
                self.record_failure(&msg);
                return Err(CredentialError::Provider(msg));
            }
            Err(_) => {
                let msg = format!("`{}` timed out after {}s", self.label, TIMEOUT.as_secs());
                self.record_failure(&msg);
                return Err(CredentialError::Provider(msg));
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let msg = format!(
                "`{}` exited with {}: {}",
                self.label,
                output.status,
                stderr.trim()
            );
            self.record_failure(&msg);
            return Err(CredentialError::Provider(msg));
        }

        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            // Not negative-cached: the command ran fast and "empty" is
            // a stable configured-absence, not a failure to back off
            // from — and leaving it uncached keeps a fresh
            // `gh auth login` visible on the very next resolve.
            return Err(CredentialError::NotFound(format!(
                "`{}` returned empty output",
                self.label
            )));
        }

        let cred = Credential::new(token, format!("cmd:{}", self.label));
        self.record_success(&cred);
        Ok(cred)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command that runs forever (`sleep 60`) must surface as a
    /// timeout, not a hang. Without the timeout in `resolve`, this
    /// test would block indefinitely.
    #[tokio::test(flavor = "current_thread")]
    async fn resolve_times_out_on_hanging_command() {
        let provider = CommandProvider::new("sleep", &["60"]);
        let start = std::time::Instant::now();
        let result = provider.resolve("any").await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "should time out within ~5s, not block forever; took {elapsed:?}"
        );
        let err = result.expect_err("hanging command must produce an error, not a fake credential");
        match err {
            CredentialError::Provider(msg) => {
                assert!(
                    msg.contains("timed out"),
                    "error message should mention timeout; got: {msg}"
                );
            }
            other => panic!("expected Provider error, got {other:?}"),
        }
    }

    /// Sanity check: a fast successful command still returns the
    /// trimmed stdout as a credential.
    #[tokio::test(flavor = "current_thread")]
    async fn resolve_returns_stdout_on_success() {
        let provider = CommandProvider::new("printf", &["test-token-123"]);
        let cred = provider.resolve("any").await.expect("printf works");
        assert_eq!(cred.token(), "test-token-123");
    }

    /// A per-test scratch file the spawned shell appends to — counting
    /// its lines counts actual command runs, which is what the cache
    /// is supposed to prevent.
    fn run_counter_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "lazybox-auth-cache-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    fn runs(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path).map_or(0, |s| s.lines().count())
    }

    /// A successful resolve is served from cache: the second resolve
    /// must not spawn the command again. This is the fix for `gh auth
    /// token` being spawned fresh on every poll tick.
    #[tokio::test(flavor = "current_thread")]
    async fn success_is_cached_and_the_command_not_respawned() {
        let marker = run_counter_path("hit");
        let _ = std::fs::remove_file(&marker);
        let script = format!("echo run >> '{}'; echo tok", marker.display());
        let provider = CommandProvider::new("sh", &["-c", &script]);

        let first = provider.resolve("any").await.expect("first resolve");
        assert_eq!(first.token(), "tok");
        let second = provider.resolve("any").await.expect("cached resolve");
        assert_eq!(second.token(), "tok");
        assert_eq!(runs(&marker), 1, "second resolve must be a cache hit");
        let _ = std::fs::remove_file(&marker);
    }

    /// A failing resolve opens a backoff window: the second resolve
    /// returns the cached error without respawning. This is the fix
    /// for the credential subprocess storm under system load.
    #[tokio::test(flavor = "current_thread")]
    async fn failure_is_negative_cached_with_backoff() {
        let marker = run_counter_path("miss");
        let _ = std::fs::remove_file(&marker);
        let script = format!(
            "echo run >> '{}'; echo broken >&2; exit 1",
            marker.display()
        );
        let provider = CommandProvider::new("sh", &["-c", &script]);

        let first = provider.resolve("any").await;
        assert!(matches!(first, Err(CredentialError::Provider(_))));
        let second = provider.resolve("any").await;
        match second {
            Err(CredentialError::Provider(msg)) => {
                assert!(msg.contains("cached failure"), "served from cache: {msg}");
            }
            other => panic!("expected cached Provider error, got {other:?}"),
        }
        assert_eq!(runs(&marker), 1, "backoff window must prevent a respawn");

        // Explicit invalidation (the user-refresh path) reopens it.
        invalidate_command_credential_cache();
        let third = provider.resolve("any").await;
        assert!(matches!(third, Err(CredentialError::Provider(_))));
        assert_eq!(runs(&marker), 2, "invalidation must allow a fresh run");
        let _ = std::fs::remove_file(&marker);
    }

    /// `.uncached()` opts a provider out entirely.
    #[tokio::test(flavor = "current_thread")]
    async fn uncached_provider_always_runs() {
        let marker = run_counter_path("uncached");
        let _ = std::fs::remove_file(&marker);
        let script = format!("echo run >> '{}'; echo tok", marker.display());
        let provider = CommandProvider::new("sh", &["-c", &script]).uncached();

        provider.resolve("any").await.expect("first");
        provider.resolve("any").await.expect("second");
        assert_eq!(runs(&marker), 2);
        let _ = std::fs::remove_file(&marker);
    }
}
