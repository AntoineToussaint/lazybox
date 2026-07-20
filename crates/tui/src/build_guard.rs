//! Startup check for a newer lazybox build.
//!
//! Source builds compare their baked commit with the checkout's local
//! `origin/main` ref. Cargo-dist builds compare their package version with
//! GitHub's latest published release. Neither path changes the installation.

use lazybox_ipc::{BUILD_GIT_SHA, BUILD_SOURCE_DIR, IS_RELEASE_BUILD};
use semver::Version;
use std::process::Command;
use std::time::Duration;

const RELEASE_CHECK_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvailableUpdate {
    Source {
        current: String,
        available: String,
        commits_behind: u32,
    },
    Release {
        current: String,
        available: String,
    },
}

impl AvailableUpdate {
    pub(crate) fn target(&self) -> String {
        match self {
            Self::Source { available, .. } => format!("source:{available}"),
            Self::Release { available, .. } => format!("release:{available}"),
        }
    }

    pub(crate) fn modal_body(&self) -> String {
        match self {
            Self::Source {
                current,
                available,
                commits_behind,
            } => {
                let commits = if *commits_behind == 1 {
                    "commit"
                } else {
                    "commits"
                };
                format!(
                    "You're {commits_behind} {commits} behind — {current} → {available}.\n\n\
                     Update with:\n  git pull --ff-only && cargo build --release\n\n\
                     Lazybox will not run this command or update itself."
                )
            }
            Self::Release { current, available } => format!(
                "A newer release is available — {current} → {available}.\n\n\
                 Update with:\n  brew upgrade lazybox\n\n\
                 Lazybox will not run this command or update itself."
            ),
        }
    }
}

pub fn is_release_build() -> bool {
    IS_RELEASE_BUILD
}

/// Check the channel appropriate to this build's provenance.
pub async fn available_update() -> Option<AvailableUpdate> {
    if is_release_build() {
        latest_release_update().await
    } else {
        source_update_in(BUILD_SOURCE_DIR, BUILD_GIT_SHA)
    }
}

/// Legacy header-nudge input. The header remains suppressed for source builds;
/// their stale-binary warning is the startup modal returned by
/// [`available_update`].
pub fn commits_behind() -> Option<u32> {
    if !is_release_build() {
        return None;
    }
    commits_behind_in(BUILD_SOURCE_DIR, BUILD_GIT_SHA)
}

fn source_update_in(source_dir: &str, sha: &str) -> Option<AvailableUpdate> {
    let commits_behind = commits_behind_in(source_dir, sha)?;
    let available = git_stdout(source_dir, &["rev-parse", "--short=12", "origin/main"])?;
    Some(AvailableUpdate::Source {
        current: sha.to_string(),
        available,
        commits_behind,
    })
}

fn commits_behind_in(source_dir: &str, sha: &str) -> Option<u32> {
    if source_dir.is_empty() || sha.is_empty() || sha == "unknown" {
        return None;
    }
    let count = git_stdout(
        source_dir,
        &["rev-list", "--count", &format!("{sha}..origin/main")],
    )?
    .parse()
    .ok()?;
    (count > 0).then_some(count)
}

fn git_stdout(source_dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(source_dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

async fn latest_release_update() -> Option<AvailableUpdate> {
    let client = octocrab::Octocrab::builder()
        .set_connect_timeout(Some(RELEASE_CHECK_TIMEOUT))
        .set_read_timeout(Some(RELEASE_CHECK_TIMEOUT))
        .build()
        .ok()?;
    let repo = client.repos("AntoineToussaint", "lazybox");
    let releases = repo.releases();
    let request = releases.get_latest();
    let release = match tokio::time::timeout(RELEASE_CHECK_TIMEOUT, request).await {
        Ok(Ok(release)) => release,
        Ok(Err(error)) => {
            tracing::debug!("latest release check failed: {error}");
            return None;
        }
        Err(_) => {
            tracing::debug!("latest release check timed out");
            return None;
        }
    };
    release_update(env!("CARGO_PKG_VERSION"), &release.tag_name)
}

fn release_update(current: &str, latest_tag: &str) -> Option<AvailableUpdate> {
    let current_version = Version::parse(current).ok()?;
    let latest_version = Version::parse(latest_tag.strip_prefix('v').unwrap_or(latest_tag)).ok()?;
    (latest_version > current_version).then(|| AvailableUpdate::Release {
        current: format!("v{current_version}"),
        available: format!("v{latest_version}"),
    })
}

pub fn outdated_message(behind: u32) -> String {
    let commits = if behind == 1 { "commit" } else { "commits" };
    format!("⚠ outdated build — {behind} {commits} behind main; update & restart")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn git(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn source_build_detects_the_real_origin_main_boundary() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(
            repo.path(),
            &["config", "user.email", "lazybox@example.com"],
        );
        git(repo.path(), &["config", "user.name", "Lazybox Test"]);
        git(repo.path(), &["config", "commit.gpgsign", "false"]);

        std::fs::write(repo.path().join("version"), "old").unwrap();
        git(repo.path(), &["add", "version"]);
        git(repo.path(), &["commit", "-qm", "old"]);
        let old = git(repo.path(), &["rev-parse", "--short=12", "HEAD"]);

        std::fs::write(repo.path().join("version"), "new").unwrap();
        git(repo.path(), &["commit", "-qam", "new"]);
        let latest = git(repo.path(), &["rev-parse", "--short=12", "HEAD"]);
        git(
            repo.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );

        assert_eq!(
            source_update_in(repo.path().to_str().unwrap(), &old),
            Some(AvailableUpdate::Source {
                current: old,
                available: latest.clone(),
                commits_behind: 1,
            })
        );
        assert_eq!(
            source_update_in(repo.path().to_str().unwrap(), &latest),
            None
        );
    }

    #[test]
    fn current_source_and_missing_provenance_stay_quiet() {
        assert_eq!(source_update_in("", "abc123"), None);
        assert_eq!(source_update_in("/some/repo", ""), None);
        assert_eq!(source_update_in("/some/repo", "unknown"), None);
    }

    #[test]
    fn release_comparison_only_reports_newer_semver() {
        assert_eq!(
            release_update("0.1.7", "v0.2.0"),
            Some(AvailableUpdate::Release {
                current: "v0.1.7".into(),
                available: "v0.2.0".into(),
            })
        );
        assert_eq!(release_update("0.1.7", "v0.1.7"), None);
        assert_eq!(release_update("0.1.7", "v0.1.6"), None);
        assert_eq!(release_update("0.1.7", "not-a-version"), None);
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)]
    async fn dev_check_uses_the_source_channel() {
        assert!(!is_release_build());
        assert_eq!(
            available_update().await,
            source_update_in(BUILD_SOURCE_DIR, BUILD_GIT_SHA)
        );
        assert_eq!(commits_behind(), None);
    }

    #[test]
    fn modal_copy_names_versions_commands_and_no_auto_update() {
        let source = AvailableUpdate::Source {
            current: "abc123".into(),
            available: "def456".into(),
            commits_behind: 2,
        }
        .modal_body();
        assert!(source.contains("2 commits behind — abc123 → def456"));
        assert!(source.contains("git pull --ff-only && cargo build --release"));
        assert!(source.contains("will not run this command"));

        let release = AvailableUpdate::Release {
            current: "v0.1.7".into(),
            available: "v0.2.0".into(),
        }
        .modal_body();
        assert!(release.contains("v0.1.7 → v0.2.0"));
        assert!(release.contains("brew upgrade lazybox"));
        assert!(release.contains("will not run this command"));
    }

    #[test]
    fn legacy_message_pluralizes_and_names_the_fix() {
        assert!(outdated_message(1).contains("1 commit behind"));
        assert!(outdated_message(89).contains("89 commits behind"));
        assert!(outdated_message(5).contains("update & restart"));
    }
}
