//! Epic records — a named, cross-repo set of workspaces with a dependency
//! graph whose status the daemon derives (see `docs/orchestration-scoping.md`
//! §4). The record itself is deliberately thin: identity, name, an optional
//! tracker anchor, explicit members, and the opt-in for status labels.
//! Everything about *status* is derived by the daemon's `EpicResolver` and
//! never stored here — a stale record can only lose members, never freeze a
//! status.
//!
//! Lives in `core` (rather than `server`) so `ipc` can name `EpicRecord` in
//! the `Command::UpsertEpic` wire type without depending on the daemon crate.

use serde::{Deserialize, Serialize};

use crate::{TaskId, WorkspaceKey};

/// Stable identity of an epic. A `slugify`d form of the epic name or of the
/// anchor task id, so the same tracker parent always resolves to the same
/// key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct EpicKey(pub String);

impl EpicKey {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Derive a key from a human name (`"Auth refactor"` → `"auth-refactor"`),
    /// falling back to a stable placeholder when the name has no usable
    /// characters so a caller always gets a non-empty key.
    pub fn from_name(name: &str) -> Self {
        let slug = crate::slug::slugify(name);
        if slug.is_empty() {
            Self("epic".to_string())
        } else {
            Self(slug)
        }
    }
}

impl std::fmt::Display for EpicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A cross-repo epic: its identity, the tracker record it is anchored to
/// (when any), and its explicit members. Membership is the union of three
/// sources — the anchor's transitive sub-issue chain, these explicit
/// `members`, and (read-side only) an `epic:<key>` label — resolved fresh on
/// every poll so a fold from issue to PR keeps a row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct EpicRecord {
    pub key: EpicKey,
    pub name: String,
    /// The tracker record this epic is anchored to (a GitHub parent issue, a
    /// Linear project's identifier), when any. Membership is re-derived from
    /// it on every poll: every task whose `parent` chain reaches it.
    #[serde(default)]
    pub anchor: Option<TaskId>,
    /// Explicit members (kv-only epics, or extra rows beyond the anchor's
    /// sub-issues). Workspace keys, not task ids, so a row survives the
    /// issue→PR fold.
    #[serde(default)]
    pub members: Vec<WorkspaceKey>,
    /// When true, the resolver writes the derived-status labels
    /// (`lazybox:ready|blocked|done`) upstream on change. Off by default so a
    /// new epic does not start mutating GitHub labels until the operator opts
    /// in.
    #[serde(default)]
    pub publish_status_labels: bool,
    /// When true (the default), every dependency (`Blocks`) edge between two
    /// members also *implies* a merge-after edge: a member's PR must not land
    /// before the PRs it depends on. An epic opts out (setting this false) when
    /// its members can merge in any order despite the work ordering — the graph
    /// still gates *starting* work, but not the *merge* sequence. An explicit
    /// `Merge after:` marker always adds a merge-after edge regardless of this.
    #[serde(default = "default_true")]
    pub implied_merge_after: bool,
    #[serde(default)]
    pub archived: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// serde default for [`EpicRecord::implied_merge_after`] — a `Blocks` edge
/// implies a merge-after edge unless an epic explicitly opts out, and a record
/// written before the field existed must load with the implication *on*.
fn default_true() -> bool {
    true
}

impl EpicRecord {
    /// A kv-only epic with no tracker anchor and no members yet.
    pub fn new(key: EpicKey, name: impl Into<String>, now: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            key,
            name: name.into(),
            anchor: None,
            members: Vec::new(),
            publish_status_labels: false,
            implied_merge_after: true,
            archived: false,
            created_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_from_name_slugifies_and_falls_back() {
        assert_eq!(
            EpicKey::from_name("Auth Refactor").as_str(),
            "auth-refactor"
        );
        assert_eq!(EpicKey::from_name("🚀").as_str(), "epic");
        assert_eq!(EpicKey::from_name("").as_str(), "epic");
    }

    #[test]
    fn record_round_trips_through_json_with_defaults() {
        let now = chrono::Utc::now();
        let record = EpicRecord::new(EpicKey::new("auth-refactor"), "Auth refactor", now);
        let json = serde_json::to_string(&record).unwrap();
        let back: EpicRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
        assert!(!back.publish_status_labels);
        assert!(back.anchor.is_none());
        assert!(back.members.is_empty());
        assert!(back.implied_merge_after);
    }

    #[test]
    fn record_deserializes_without_optional_fields() {
        // A record written before the optional fields existed must still
        // load — the `serde(default)`s cover a forward migration. EpicKey is
        // a newtype struct, so serde encodes it as its inner string.
        let json = r#"{"key":"x","name":"X","created_at":"2026-09-07T00:00:00Z"}"#;
        let back: EpicRecord = serde_json::from_str(json).unwrap();
        assert_eq!(back.key.as_str(), "x");
        assert!(back.members.is_empty());
        assert!(!back.archived);
        // A record written before `implied_merge_after` existed must load with
        // the implication ON — the default-true migration, not bool's false.
        assert!(back.implied_merge_after);
    }

    #[test]
    fn implied_merge_after_opt_out_round_trips() {
        let now = chrono::Utc::now();
        let mut record = EpicRecord::new(EpicKey::new("x"), "X", now);
        record.implied_merge_after = false;
        let json = serde_json::to_string(&record).unwrap();
        let back: EpicRecord = serde_json::from_str(&json).unwrap();
        assert!(!back.implied_merge_after);
    }
}
