//! Startup check for a newer lazybox build — the fetching half.
//!
//! The pure result types (`AvailableUpdate`, `ReleaseInstall`,
//! `is_release_build`) live in the UI library (`lazybox_tui::build_guard`)
//! because the Model and sidebar render them. This module owns everything
//! that talks to the outside world — git, Homebrew, the GitHub releases
//! API through `octocrab`, and the state-DB release-check cache — and is
//! quarantined boot-side so the UI library never pulls in `octocrab` or a
//! second gh credential path (#548).

use lazybox_ipc::{BUILD_GIT_SHA, BUILD_SOURCE_DIR};
use lazybox_store::Store;
use lazybox_tui::build_guard::{AvailableUpdate, ReleaseInstall, is_release_build};
use semver::Version;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RELEASE_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const CREDENTIAL_TIMEOUT: Duration = Duration::from_secs(1);
const SOURCE_CHECK_TIMEOUT: Duration = Duration::from_secs(4);
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const BREW_CHECK_TIMEOUT: Duration = Duration::from_secs(1);
const CACHE_READ_TIMEOUT: Duration = Duration::from_millis(250);
const RELEASE_CACHE_MAX_AGE: Duration = Duration::from_secs(15 * 60);
const RELEASE_CACHE_KV_KEY: &str = "latest_release_cache";

/// Check the channel appropriate to this build's provenance.
pub async fn available_update(store: Option<Arc<dyn Store>>) -> Option<AvailableUpdate> {
    if is_release_build() {
        latest_release_update(store).await
    } else {
        match tokio::time::timeout(
            SOURCE_CHECK_TIMEOUT,
            source_update_in(BUILD_SOURCE_DIR, BUILD_GIT_SHA),
        )
        .await
        {
            Ok(update) => update,
            Err(_) => {
                tracing::debug!("source update check timed out");
                None
            }
        }
    }
}

async fn source_update_in(source_dir: &str, sha: &str) -> Option<AvailableUpdate> {
    if source_dir.is_empty() || sha.is_empty() || sha == "unknown" {
        return None;
    }

    let build_ref = format!("{sha}^{{commit}}");
    let built = git_stdout(source_dir, &["rev-parse", "--verify", &build_ref]).await?;
    let head = git_stdout(source_dir, &["rev-parse", "--verify", "HEAD^{commit}"]).await?;

    let upstream = git_stdout(
        source_dir,
        &["rev-parse", "--verify", "@{upstream}^{commit}"],
    )
    .await
    .filter(|commit| commit != &head);
    let upstream_is_ahead = match upstream.as_deref() {
        Some(commit) => git_is_ancestor(source_dir, &head, commit).await == Some(true),
        None => false,
    };
    let (available_commit, pull_before_build) = if upstream_is_ahead {
        (upstream.as_deref()?, true)
    } else {
        (head.as_str(), false)
    };

    if built == available_commit
        || git_is_ancestor(source_dir, &built, available_commit).await != Some(true)
    {
        return None;
    }

    let range = format!("{built}..{available_commit}");
    let commits_behind = git_stdout(source_dir, &["rev-list", "--count", &range])
        .await?
        .parse()
        .ok()?;
    if commits_behind == 0 {
        return None;
    }
    let available = git_stdout(source_dir, &["rev-parse", "--short=12", available_commit]).await?;

    Some(AvailableUpdate::Source {
        current: sha.to_string(),
        available,
        commits_behind,
        source_dir: source_dir.to_string(),
        pull_before_build,
    })
}

async fn git_is_ancestor(source_dir: &str, ancestor: &str, descendant: &str) -> Option<bool> {
    let output = git_output(
        source_dir,
        &["merge-base", "--is-ancestor", ancestor, descendant],
    )
    .await?;
    match output.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

async fn git_stdout(source_dir: &str, args: &[&str]) -> Option<String> {
    let output = git_output(source_dir, args).await?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

async fn git_output(source_dir: &str, args: &[&str]) -> Option<std::process::Output> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(source_dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(GIT_COMMAND_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => Some(output),
        Ok(Err(error)) => {
            tracing::debug!("git update check failed: {error}");
            None
        }
        Err(_) => {
            tracing::debug!("git update check command timed out");
            None
        }
    }
}

async fn latest_release_update(store: Option<Arc<dyn Store>>) -> Option<AvailableUpdate> {
    let now = unix_timestamp();
    let cached = load_release_cache(store.clone()).await;
    let latest_tag = if cached.as_ref().is_some_and(|cache| cache.is_fresh(now)) {
        cached.as_ref()?.tag.clone()
    } else {
        match fetch_latest_release_tag().await {
            Some(tag) => {
                if let Some(store) = store {
                    persist_release_cache(store, ReleaseCache::new(now, tag.clone()));
                }
                tag
            }
            None => cached?.tag,
        }
    };

    let install = release_install().await;
    release_update(env!("CARGO_PKG_VERSION"), &latest_tag, install)
}

async fn fetch_latest_release_tag() -> Option<String> {
    // Always the public AntoineToussaint/lazybox repo on github.com —
    // never the user's configured (possibly Enterprise) GitHub host.
    let credential = tokio::time::timeout(
        CREDENTIAL_TIMEOUT,
        lazybox_gh::credential_chain(None).resolve(lazybox_gh::SOURCE),
    )
    .await
    .ok()
    .and_then(Result::ok);

    let client = {
        let mut builder = octocrab::Octocrab::builder()
            .add_retry_config(octocrab::service::middleware::retry::RetryConfig::None)
            .set_connect_timeout(Some(RELEASE_CHECK_TIMEOUT))
            .set_read_timeout(Some(RELEASE_CHECK_TIMEOUT));
        if let Some(credential) = credential {
            builder = builder.personal_token(credential.into_token());
        }
        builder.build().ok()?
    };
    let repository = client.repos("AntoineToussaint", "lazybox");
    let releases = repository.releases();
    let request = releases.get_latest();
    match tokio::time::timeout(RELEASE_CHECK_TIMEOUT, request).await {
        Ok(Ok(release)) => Some(release.tag_name),
        Ok(Err(error)) => {
            tracing::debug!("latest release check failed: {error}");
            None
        }
        Err(_) => {
            tracing::debug!("latest release check timed out");
            None
        }
    }
}

fn release_update(
    current: &str,
    latest_tag: &str,
    install: ReleaseInstall,
) -> Option<AvailableUpdate> {
    let current_version = Version::parse(current).ok()?;
    let latest_version = Version::parse(latest_tag.strip_prefix('v').unwrap_or(latest_tag)).ok()?;
    (latest_version > current_version).then(|| AvailableUpdate::Release {
        current: format!("v{current_version}"),
        available: format!("v{latest_version}"),
        install,
    })
}

async fn release_install() -> ReleaseInstall {
    let Some(executable) = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
    else {
        return ReleaseInstall::Shell;
    };
    let mut command = tokio::process::Command::new("brew");
    command
        .args(["--prefix", "lazybox"])
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let prefix = match tokio::time::timeout(BREW_CHECK_TIMEOUT, command.output()).await {
        Ok(Ok(output)) if output.status.success() => String::from_utf8(output.stdout)
            .ok()
            .map(|value| PathBuf::from(value.trim()))
            .and_then(|path| path.canonicalize().ok()),
        _ => None,
    };
    release_install_for_paths(&executable, prefix.as_deref())
}

fn release_install_for_paths(executable: &Path, brew_prefix: Option<&Path>) -> ReleaseInstall {
    let components: Vec<_> = executable
        .components()
        .map(|component| component.as_os_str())
        .collect();
    let is_cellar_install = components.windows(2).any(|pair| {
        pair == [
            std::ffi::OsStr::new("Cellar"),
            std::ffi::OsStr::new("lazybox"),
        ]
    });
    if is_cellar_install || brew_prefix.is_some_and(|prefix| executable.starts_with(prefix)) {
        ReleaseInstall::Homebrew
    } else {
        ReleaseInstall::Shell
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseCache {
    checked_at: u64,
    tag: String,
}

impl ReleaseCache {
    fn new(checked_at: u64, tag: String) -> Self {
        Self { checked_at, tag }
    }

    fn parse(raw: &str) -> Option<Self> {
        let (checked_at, tag) = raw.split_once('\n')?;
        let checked_at = checked_at.parse().ok()?;
        Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()?;
        Some(Self::new(checked_at, tag.to_string()))
    }

    fn encode(&self) -> String {
        format!("{}\n{}", self.checked_at, self.tag)
    }

    fn is_fresh(&self, now: u64) -> bool {
        now.checked_sub(self.checked_at)
            .is_some_and(|age| age <= RELEASE_CACHE_MAX_AGE.as_secs())
    }
}

async fn load_release_cache(store: Option<Arc<dyn Store>>) -> Option<ReleaseCache> {
    let store = store?;
    let read = tokio::task::spawn_blocking(move || store.get_kv(RELEASE_CACHE_KV_KEY));
    match tokio::time::timeout(CACHE_READ_TIMEOUT, read).await {
        Ok(Ok(Ok(Some(raw)))) => ReleaseCache::parse(&raw),
        Ok(Ok(Err(error))) => {
            tracing::debug!("read latest release cache failed: {error}");
            None
        }
        Ok(Err(error)) => {
            tracing::debug!("latest release cache task failed: {error}");
            None
        }
        Err(_) => {
            tracing::debug!("latest release cache read timed out");
            None
        }
        Ok(Ok(Ok(None))) => None,
    }
}

fn persist_release_cache(store: Arc<dyn Store>, cache: ReleaseCache) {
    let raw = cache.encode();
    let _handle = tokio::task::spawn_blocking(move || {
        if let Err(error) = store.set_kv(RELEASE_CACHE_KV_KEY, &raw) {
            tracing::debug!("persist latest release cache failed: {error}");
        }
    });
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn init_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(
            repo.path(),
            &["config", "user.email", "lazybox@example.com"],
        );
        git(repo.path(), &["config", "user.name", "Lazybox Test"]);
        git(repo.path(), &["config", "commit.gpgsign", "false"]);
        git(repo.path(), &["branch", "-M", "main"]);
        repo
    }

    fn commit(repo: &Path, value: &str) -> String {
        std::fs::write(repo.join("version"), value).unwrap();
        git(repo, &["add", "version"]);
        git(repo, &["commit", "-qm", value]);
        git(repo, &["rev-parse", "--short=12", "HEAD"])
    }

    #[tokio::test]
    async fn source_build_detects_a_newer_checkout_head() {
        let repo = init_repo();
        let old = commit(repo.path(), "old");
        let latest = commit(repo.path(), "new");

        assert_eq!(
            source_update_in(repo.path().to_str().unwrap(), &old).await,
            Some(AvailableUpdate::Source {
                current: old,
                available: latest,
                commits_behind: 1,
                source_dir: repo.path().to_string_lossy().into_owned(),
                pull_before_build: false,
            })
        );
    }

    #[tokio::test]
    async fn source_build_uses_ahead_tracking_upstream() {
        let repo = init_repo();
        let old = commit(repo.path(), "old");
        let latest = commit(repo.path(), "new");
        git(repo.path(), &["remote", "add", "origin", "."]);
        git(
            repo.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git(repo.path(), &["reset", "--hard", "HEAD^"]);
        git(
            repo.path(),
            &["branch", "--set-upstream-to=origin/main", "main"],
        );

        assert_eq!(
            source_update_in(repo.path().to_str().unwrap(), &old).await,
            Some(AvailableUpdate::Source {
                current: old,
                available: latest,
                commits_behind: 1,
                source_dir: repo.path().to_string_lossy().into_owned(),
                pull_before_build: true,
            })
        );
    }

    #[tokio::test]
    async fn divergent_main_does_not_mark_a_feature_build_stale() {
        let repo = init_repo();
        commit(repo.path(), "base");
        git(repo.path(), &["switch", "-qc", "feature"]);
        let feature = commit(repo.path(), "feature");
        git(repo.path(), &["update-ref", "refs/heads/main", "HEAD^"]);
        git(repo.path(), &["switch", "-q", "main"]);
        commit(repo.path(), "main");
        git(
            repo.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git(repo.path(), &["switch", "-q", "feature"]);

        assert_eq!(
            source_update_in(repo.path().to_str().unwrap(), &feature).await,
            None
        );
    }

    #[tokio::test]
    async fn current_source_and_missing_provenance_stay_quiet() {
        let repo = init_repo();
        let current = commit(repo.path(), "current");
        assert_eq!(
            source_update_in(repo.path().to_str().unwrap(), &current).await,
            None
        );
        assert_eq!(source_update_in("", "abc123").await, None);
        assert_eq!(source_update_in("/some/repo", "").await, None);
        assert_eq!(source_update_in("/some/repo", "unknown").await, None);
    }

    #[test]
    fn release_comparison_only_reports_newer_semver() {
        assert_eq!(
            release_update("0.1.7", "v0.2.0", ReleaseInstall::Homebrew),
            Some(AvailableUpdate::Release {
                current: "v0.1.7".into(),
                available: "v0.2.0".into(),
                install: ReleaseInstall::Homebrew,
            })
        );
        assert_eq!(
            release_update("0.1.7", "v0.1.7", ReleaseInstall::Shell),
            None
        );
        assert_eq!(
            release_update("0.1.7", "v0.1.6", ReleaseInstall::Shell),
            None
        );
        assert_eq!(
            release_update("0.1.7", "not-a-version", ReleaseInstall::Shell),
            None
        );
    }

    #[test]
    fn release_install_tracks_the_executable_location() {
        assert_eq!(
            release_install_for_paths(
                Path::new("/opt/homebrew/Cellar/lazybox/0.2.0/bin/lazybox"),
                Some(Path::new("/opt/homebrew/Cellar/lazybox/0.2.0")),
            ),
            ReleaseInstall::Homebrew
        );
        assert_eq!(
            release_install_for_paths(
                Path::new("/Users/me/.cargo/bin/lazybox"),
                Some(Path::new("/opt/homebrew/Cellar/lazybox/0.2.0")),
            ),
            ReleaseInstall::Shell
        );
        assert_eq!(
            release_install_for_paths(Path::new("/custom/Cellar/lazybox/0.2.0/bin/lazybox"), None,),
            ReleaseInstall::Homebrew
        );
        assert_eq!(
            release_install_for_paths(Path::new("/Users/me/.cargo/bin/lazybox"), None),
            ReleaseInstall::Shell
        );
    }

    #[test]
    fn release_cache_is_validated_and_bounded() {
        let cache = ReleaseCache::new(1_000, "v0.2.0".into());
        assert_eq!(ReleaseCache::parse(&cache.encode()), Some(cache.clone()));
        assert!(cache.is_fresh(1_000 + RELEASE_CACHE_MAX_AGE.as_secs()));
        assert!(!cache.is_fresh(999));
        assert!(!cache.is_fresh(1_001 + RELEASE_CACHE_MAX_AGE.as_secs()));
        assert_eq!(ReleaseCache::parse("1000\nnot-a-version"), None);
    }

    #[tokio::test]
    async fn dev_check_uses_the_source_channel() {
        assert!(!is_release_build());
        assert_eq!(
            available_update(None).await,
            source_update_in(BUILD_SOURCE_DIR, BUILD_GIT_SHA).await
        );
    }
}
