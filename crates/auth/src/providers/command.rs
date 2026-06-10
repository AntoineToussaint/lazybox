use crate::{Credential, CredentialError, CredentialProvider};

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
    /// Label for logging/display.
    label: String,
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
        }
    }

    /// Override the display label.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }
}

impl CredentialProvider for CommandProvider {
    fn name(&self) -> &str {
        "command"
    }

    async fn resolve(&self, _scope: &str) -> Result<Credential, CredentialError> {
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
        let run = tokio::process::Command::new(&self.program)
            .args(&self.args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let output = match tokio::time::timeout(TIMEOUT, run).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                return Err(CredentialError::Provider(format!(
                    "failed to run `{}`: {e}",
                    self.label
                )));
            }
            Err(_) => {
                return Err(CredentialError::Provider(format!(
                    "`{}` timed out after {}s",
                    self.label,
                    TIMEOUT.as_secs()
                )));
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CredentialError::Provider(format!(
                "`{}` exited with {}: {}",
                self.label,
                output.status,
                stderr.trim()
            )));
        }

        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            return Err(CredentialError::NotFound(format!(
                "`{}` returned empty output",
                self.label
            )));
        }

        Ok(Credential::new(token, format!("cmd:{}", self.label)))
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
}
