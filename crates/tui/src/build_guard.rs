//! Outdated-build guard.
//!
//! A uniformly-stale install — daemon *and* client built from the same
//! old commit — passes the protocol handshake and the daemon/client
//! build-match check silently, then reproduces already-fixed bugs (the
//! issue→PR session loss of #78/#161/#167 being the worst). The build
//! version is exchanged on the wire, but nothing compares the *running*
//! build against "latest", so a 89-commits-behind binary runs with no
//! warning at all.
//!
//! The binary remembers the git checkout it was built from
//! ([`BUILD_SOURCE_DIR`]) and its commit ([`BUILD_GIT_SHA`]), and at
//! startup counts how many commits that build trails `origin/main`. A
//! non-zero count raises the persistent "outdated build" banner.
//!
//! The count is read from the existing `origin/main` remote-tracking
//! ref — no network, so startup pays nothing. That ref can itself lag
//! the true remote (a `git fetch` only updates `FETCH_HEAD`), which can
//! only *under*-report staleness; it never invents a warning.
//!
//! The guard fires for both build kinds, with the fix phrased per
//! provenance ([`update_action`]): an installer-managed release binary
//! gets "update & restart", a dev/source build gets "rebuild & restart"
//! — the exact incident of issue #391 was a dev binary running long
//! after its checkout was pulled forward, with zero signal because the
//! guard used to be gated off for dev builds entirely (#251 gated the
//! *installer* wording, and the gate took the whole check with it). A
//! released binary built outside a checkout still has no source ref to
//! count against, so it stays quiet until the release-tag comparison
//! lands (future work).

use lazybox_ipc::{BUILD_GIT_SHA, BUILD_SOURCE_DIR, IS_RELEASE_BUILD};
use std::process::Command;

/// Whether the running binary is an installer-managed release build
/// (cargo-dist) rather than a dev/source build. Decides the fix wording
/// of the outdated-build nudge ([`update_action`]) and the `(dev)`
/// header tag (issue #251) — a dev build is updated with
/// `git pull && cargo build`, not an installer swap.
pub fn is_release_build() -> bool {
    IS_RELEASE_BUILD
}

/// How many commits the running build trails `origin/main`, when that
/// can be determined locally and is non-zero. `None` means "current,
/// or can't tell" — both resolve to no banner. Checked for dev builds
/// too (issue #391): a stale dev binary running after `git pull` is the
/// staleness case that actually bites, and it used to get zero signal.
pub fn commits_behind() -> Option<u32> {
    commits_behind_in(BUILD_SOURCE_DIR, BUILD_GIT_SHA)
}

/// Testable core of [`commits_behind`]: count `sha..origin/main` in the
/// checkout at `source_dir`. Returns `None` when the build carries no
/// usable provenance (released tarball, `unknown` SHA) or when git can't
/// answer (no `origin/main`, detached source, missing checkout).
fn commits_behind_in(source_dir: &str, sha: &str) -> Option<u32> {
    if source_dir.is_empty() || sha.is_empty() || sha == "unknown" {
        return None;
    }
    let out = Command::new("git")
        .args(["-C", source_dir, "rev-list", "--count"])
        .arg(format!("{sha}..origin/main"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let count = parse_commit_count(&String::from_utf8_lossy(&out.stdout))?;
    (count > 0).then_some(count)
}

/// Parse the single integer `git rev-list --count` prints. Tolerant of
/// surrounding whitespace; rejects anything else.
fn parse_commit_count(stdout: &str) -> Option<u32> {
    stdout.trim().parse().ok()
}

/// The fix the banner tells the user to take, matched to how this
/// binary is actually updated: an installer swap for a release build,
/// `git pull && cargo build` for a dev/source build. The binary can't
/// update itself either way.
pub fn update_action() -> &'static str {
    update_action_for(is_release_build())
}

fn update_action_for(is_release: bool) -> &'static str {
    if is_release {
        "update & restart"
    } else {
        "rebuild & restart"
    }
}

/// The banner text for a build `behind` commits back. Phrased as an
/// action because that's the only fix — the running binary can't
/// update itself.
pub fn outdated_message(behind: u32) -> String {
    let commits = if behind == 1 { "commit" } else { "commits" };
    format!(
        "⚠ outdated build — {behind} {commits} behind main; {}",
        update_action()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_clean_count() {
        assert_eq!(parse_commit_count("89\n"), Some(89));
        assert_eq!(parse_commit_count("  0 "), Some(0));
    }

    #[test]
    fn rejects_non_numeric_output() {
        assert_eq!(parse_commit_count(""), None);
        assert_eq!(parse_commit_count("fatal: bad revision"), None);
    }

    #[test]
    fn no_provenance_means_no_check() {
        // Released tarball (empty source dir) or an `unknown` SHA must
        // never shell out or warn — that would be a false positive on
        // every launch of a legitimately-current release binary.
        assert_eq!(commits_behind_in("", "abc123"), None);
        assert_eq!(commits_behind_in("/some/repo", ""), None);
        assert_eq!(commits_behind_in("/some/repo", "unknown"), None);
    }

    #[test]
    fn message_pluralizes_and_names_the_fix() {
        assert!(outdated_message(1).contains("1 commit behind"));
        assert!(outdated_message(89).contains("89 commits behind"));
        // The test binary is a dev/source build, so the named fix is the
        // source-build one (issue #391 — dev builds are checked too).
        assert!(!is_release_build());
        assert!(outdated_message(5).contains("rebuild & restart"));
    }

    #[test]
    fn fix_wording_matches_build_provenance() {
        assert_eq!(update_action_for(true), "update & restart");
        assert_eq!(update_action_for(false), "rebuild & restart");
    }

    /// Runs the guard's actual git query against a real repository laid
    /// out like the stale-dev-build incident of #391: the binary was
    /// built at an old commit and `origin/main` has since moved on.
    /// Guards the machinery `commits_behind` runs at startup — the toy
    /// string tests above can't catch a broken `rev-list` invocation.
    #[test]
    fn counts_commits_behind_in_a_real_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf-8 tempdir");
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(["-C", path])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "one"]);
        let built_at = git(&["rev-parse", "--short=12", "HEAD"]);
        git(&["commit", "-q", "--allow-empty", "-m", "two"]);
        git(&["commit", "-q", "--allow-empty", "-m", "three"]);
        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);

        assert_eq!(commits_behind_in(path, &built_at), Some(2));

        // A binary built at the tip is current — no banner.
        let tip = git(&["rev-parse", "--short=12", "HEAD"]);
        assert_eq!(commits_behind_in(path, &tip), None);
    }
}
