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
//! The nudge is gated on build provenance ([`is_release_build`], issue
//! #251): its "update & restart" fix only fits an installer-managed
//! release binary, so a dev/source build — normally *ahead* of the
//! latest release, and updated with `git pull && cargo build` — is
//! tagged `(dev)` in the header and never nudged. That leaves the nudge
//! dormant until a release build can compare itself against the latest
//! release tag (future work); a released binary built outside a checkout
//! has no source ref to count against regardless.

use lazybox_ipc::{BUILD_GIT_SHA, BUILD_SOURCE_DIR, IS_RELEASE_BUILD};
use std::process::Command;

/// Whether the running binary is an installer-managed release build
/// (cargo-dist) rather than a dev/source build. The outdated-build nudge
/// is gated on this: its "update & restart" fix only applies to a binary
/// an installer can swap in place, and a source checkout is normally
/// *ahead* of the latest release, so nagging it reads as a false alarm.
/// A dev build is tagged `(dev)` in the header instead (issue #251).
pub fn is_release_build() -> bool {
    IS_RELEASE_BUILD
}

/// How many commits the running build trails `origin/main`, when that
/// can be determined locally and is non-zero. `None` means "current,
/// or can't tell" — both resolve to no banner. Suppressed entirely on
/// dev/source builds: only an installer-managed release carries the
/// "update & restart" affordance the banner implies (issue #251).
pub fn commits_behind() -> Option<u32> {
    if !is_release_build() {
        return None;
    }
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

/// The banner text for a build `behind` commits back. Phrased as an
/// action ("update & restart") because that's the only fix — the
/// running binary can't update itself.
pub fn outdated_message(behind: u32) -> String {
    let commits = if behind == 1 { "commit" } else { "commits" };
    format!("⚠ outdated build — {behind} {commits} behind main; update & restart")
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
    fn dev_builds_never_nudge() {
        // The test binary is itself a dev/source build, so the guard is
        // gated off no matter how far the checkout trails main — a source
        // build is updated with `git pull && cargo build`, not the
        // installer swap the banner implies (issue #251). `commits_behind`
        // must short-circuit before shelling out to git.
        assert!(!is_release_build());
        assert_eq!(commits_behind(), None);
    }

    #[test]
    fn message_pluralizes_and_names_the_fix() {
        assert!(outdated_message(1).contains("1 commit behind"));
        assert!(outdated_message(89).contains("89 commits behind"));
        assert!(outdated_message(5).contains("update & restart"));
    }
}
