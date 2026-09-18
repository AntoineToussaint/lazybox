//! Git ref namespace arithmetic: which branch names can coexist, and
//! what to call a branch whose name is already taken by the namespace.
//!
//! Refs are files under `.git/refs/heads`, so `deps` and `deps/grouping`
//! can never both exist — one wants a file where the other wants a
//! directory (git's "directory/file conflict"). The collision is
//! *directional*, and that direction decides what a usable alternative
//! looks like:
//!
//! - `release` blocked by `release/v1`: the requested name is the
//!   *ancestor*. A leaf suffix (`release-2`) leaves the `release/`
//!   directory alone and resolves it.
//! - `deps/grouping` blocked by `deps`: the requested name is the
//!   *descendant*, and it needs a `deps/` directory that the `deps` ref
//!   occupies. A leaf suffix is useless here — `deps/grouping-2` still
//!   asks for that same directory. The boundary itself has to go, which
//!   means flattening it (`deps-grouping`).
//!
//! Both the daemon's automatic retry and the client's suggested
//! alternative read from here so a name offered in the modal is arrived
//! at the same way the daemon would have arrived at it.

/// Whether two branch names can coexist as refs. `true` when they are
/// equal or one is a `/`-separated ancestor of the other — the cases git
/// refuses with a directory/file conflict.
///
/// Ancestry is checked on segment boundaries, so `deps` does not
/// conflict with `deps-grouping` (a sibling leaf), only with
/// `deps/grouping`.
pub fn conflicts(a: &str, b: &str) -> bool {
    a == b || is_ancestor(a, b) || is_ancestor(b, a)
}

/// Whether `ancestor` names a directory that `descendant` sits inside
/// (`deps` vs `deps/grouping`). Exact equality is not ancestry.
fn is_ancestor(ancestor: &str, descendant: &str) -> bool {
    !ancestor.is_empty()
        && descendant
            .strip_prefix(ancestor)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// A candidate branch name for the `attempt`th try at creating `branch`
/// when `conflicting` already occupies the namespace. `attempt` counts
/// from 1; every attempt yields a distinct name, so a caller looping on
/// it always makes progress even when a candidate collides in turn.
///
/// The candidate resolves the *reported* collision; it is not a promise
/// that nothing else in the repo conflicts. Callers validate against the
/// full ref namespace — the daemon by attempting the checkout, which is
/// the only check that can't go stale.
pub fn alternative(branch: &str, conflicting: &str, attempt: usize) -> String {
    // Normalized so the distinctness property holds for every input, not
    // just the counting the two callers happen to use: without it a 0th
    // attempt repeats the 1st in one arm and skips a name in the other.
    let attempt = attempt.max(1);
    match flatten_under(branch, conflicting) {
        // The blocker owns a directory this name needs. Flattening that
        // one boundary is the whole fix, so it is the first candidate;
        // later attempts add a leaf suffix to keep names distinct.
        Some(flat) if attempt <= 1 => flat,
        Some(flat) => format!("{flat}-{attempt}"),
        None => format!("{branch}-{}", attempt + 1),
    }
}

/// `branch` with the `/` that sits directly under `conflicting` turned
/// into a `-`, so the blocking ref's name is no longer a path segment
/// (`deps/grouping` under `deps` → `deps-grouping`). `None` when
/// `conflicting` isn't an ancestor of `branch`, i.e. when flattening
/// would not address the collision.
fn flatten_under(branch: &str, conflicting: &str) -> Option<String> {
    is_ancestor(conflicting, branch).then(|| {
        let mut flat = branch.to_string();
        flat.replace_range(conflicting.len()..conflicting.len() + 1, "-");
        flat
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Equality and ancestry in both directions are conflicts; a sibling
    /// leaf that merely shares a prefix is not. `deps-grouping` is the
    /// name the recovery suggests, so it must read as free.
    #[test]
    fn conflicts_covers_equality_and_both_ancestry_directions() {
        assert!(conflicts("deps", "deps"));
        assert!(conflicts("deps", "deps/grouping"));
        assert!(conflicts("deps/grouping", "deps"));
        assert!(conflicts("deps/a", "deps/a/b"));
        assert!(!conflicts("deps", "deps-grouping"));
        assert!(!conflicts("deps/grouping", "deps/other"));
        assert!(!conflicts("deps", "other"));
        assert!(!conflicts("", "deps"));
    }

    /// The historical direction: the requested name is the ancestor, so a
    /// leaf suffix counting from `-2` steps out of the `<branch>/*`
    /// namespace that blocked it.
    #[test]
    fn alternative_appends_leaf_suffix_when_requested_name_is_the_ancestor() {
        assert_eq!(alternative("release", "release/v0.2.102", 1), "release-2");
        assert_eq!(alternative("release", "release/v0.2.102", 2), "release-3");
    }

    /// The reported direction (#1742): `deps` blocks `deps/grouping`, so
    /// the boundary is flattened rather than suffixed — a suffix would
    /// still demand the `deps/` directory the blocker occupies.
    #[test]
    fn alternative_flattens_when_the_blocker_is_an_ancestor() {
        assert_eq!(alternative("deps/grouping", "deps", 1), "deps-grouping");
        assert_eq!(alternative("deps/grouping", "deps", 2), "deps-grouping-2");
        assert_eq!(alternative("deps/grouping", "deps", 3), "deps-grouping-3");
        // Only the blocking boundary flattens; deeper structure survives.
        assert_eq!(alternative("deps/a/b", "deps", 1), "deps-a/b");
        assert_eq!(alternative("deps/a/b", "deps/a", 1), "deps/a-b");
    }

    /// Whatever the direction, no candidate re-collides with the blocker
    /// that produced it, and every attempt is a fresh name — the two
    /// properties a bounded retry loop needs to terminate.
    #[test]
    fn alternatives_clear_the_blocker_and_never_repeat() {
        for (branch, conflicting) in [
            ("deps/grouping", "deps"),
            ("release", "release/v1"),
            ("deps/a/b", "deps"),
        ] {
            let mut seen = Vec::new();
            for attempt in 0..=5 {
                let candidate = alternative(branch, conflicting, attempt);
                assert!(
                    !conflicts(&candidate, conflicting),
                    "{candidate} still conflicts with {conflicting}"
                );
                // Attempt 0 is normalized to 1, so it is the one repeat
                // the contract allows.
                if attempt > 1 {
                    assert!(!seen.contains(&candidate), "{candidate} repeated");
                }
                seen.push(candidate);
            }
        }
    }

    /// A conflicting name that isn't an ancestor (git named some other
    /// blocker) still yields a distinct, suffixed candidate rather than
    /// replaying the requested name.
    #[test]
    fn alternative_falls_back_to_a_suffix_for_an_unrelated_blocker() {
        assert_eq!(alternative("deps/grouping", "other", 1), "deps/grouping-2");
    }
}
