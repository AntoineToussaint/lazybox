//! Durable storage for review artifacts and their results (#1732).
//!
//! A review's findings outlive the session that produced them, so they live in
//! the daemon's store (`~/.lazybox/v2/state.db`) rather than in the worktree
//! the review ran in. That placement is the whole point: a worktree is deleted
//! when its workspace is cleaned up, and a session's scrollback is gone when
//! the PTY exits, while a fixer may start days later, in another checkout,
//! under another agent.
//!
//! Layout mirrors the blackboard's (`mcp::note_key_prefix` and friends): one kv
//! row per artifact under a sanitized, workspace-scoped prefix with a
//! zero-padded sequence, so `list_kv_prefix`'s lexical order *is* insertion
//! order. The workspace segment is escaped rather than collapsed, so two
//! distinct workspace keys can never share a prefix; reads additionally
//! filter on the `workspace` recorded inside each row, which is now
//! belt-and-braces against a hand-edited or foreign row rather than the only
//! thing standing between two workspaces.

use lazybox_core::{ArtifactStatus, ReviewArtifact, ReviewResult};
use lazybox_store::{Store, StoreError, StoreMutation};

/// kv prefix under which every persisted review report lives.
const REVIEW_KV_PREFIX: &str = "lazybox:review:";
/// kv prefix for fixer results. A separate space, because a result never
/// replaces the report it answers.
const RESULT_KV_PREFIX: &str = "lazybox:review-result:";
/// Reports retained per workspace; older ones are pruned as new ones land.
/// Generous — a report is the input to work that may not start for days — but
/// bounded, so a workspace reviewed on a loop can't grow the store forever.
const REPORTS_PER_WORKSPACE: usize = 20;
/// Results retained per workspace, for the same reason.
const RESULTS_PER_WORKSPACE: usize = 20;
/// Zero-pad width for the per-workspace sequence, matching the note keys'.
const SEQ_WIDTH: usize = 12;

/// Largest accepted submission, in bytes, summed across **every** free-text
/// field it carries.
///
/// Measuring `report` alone looked equivalent — it is the big field — but a
/// submission has ~1500 other unbounded strings: `MAX_FINDINGS` findings each
/// with a title, evidence, remediation, anchors and checks, plus the
/// top-level checks and open questions. Capping one of them bounds nothing,
/// and every one of those bytes lands in a single kv row that
/// `list_reviews` then deserializes in full on every call. A note has exactly
/// one free-text field, which is why `MAX_NOTE_BYTES` can cap a field and
/// still bound a row; this cannot, so it sums instead (#1831 review).
pub(crate) const MAX_SUBMISSION_BYTES: usize = 256 * 1024;
/// Largest accepted finding count in one submission.
pub(crate) const MAX_FINDINGS: usize = 200;

/// Key-safe, **injective** rendering of a workspace key (which carries `:`,
/// `/`, `#`).
///
/// Collapsing every non-alphanumeric byte to `_` — the obvious encoding, and
/// the one the blackboard still uses — is lossy, and the loss is not
/// cosmetic: `github:my-org/tools#42` and `github:my/org-tools#42` are two
/// real repos that collapse to the same string. Reads can defend against that
/// by filtering on the `workspace` stored inside each row, but *retention*
/// cannot — it deletes by key prefix, so one workspace's writes evict the
/// other's rows (#1831 review). Escaping instead of collapsing removes the
/// collision rather than guarding one of its two consequences.
///
/// Every byte outside `[A-Za-z0-9]` becomes `_XX` (uppercase hex), and `_`
/// itself is escaped as `_5F`, so a literal `_` never appears unescaped and
/// the mapping is reversible. The output stays within `[A-Za-z0-9_]`, which
/// keeps `:` out of the encoded segment — `next_seq` splits the trailing
/// sequence off on `:` and would otherwise mis-parse it.
fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for byte in key.bytes() {
        if byte.is_ascii_alphanumeric() {
            out.push(byte as char);
        } else {
            out.push('_');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

fn report_prefix(workspace: &str) -> String {
    format!("{REVIEW_KV_PREFIX}{}:", encode_key(workspace))
}

fn result_prefix(workspace: &str) -> String {
    format!("{RESULT_KV_PREFIX}{}:", encode_key(workspace))
}

fn row_key(prefix: &str, seq: u64) -> String {
    format!("{prefix}{seq:0width$}", width = SEQ_WIDTH)
}

/// The id a report or result carries, derived from its sequence. `prefix_char`
/// keeps the two spaces visually distinct in a tool payload (`r3` vs `x3`).
fn artifact_id(prefix_char: char, seq: u64) -> String {
    format!("{prefix_char}{seq}")
}

/// The sequence encoded in an id like `r3`, or `None` when it isn't one.
fn seq_of(id: &str, prefix_char: char) -> Option<u64> {
    id.strip_prefix(prefix_char)?.parse().ok()
}

/// Decode every row under `prefix` that really belongs to `workspace`, oldest
/// first. A row that fails to decode is skipped rather than sinking the read —
/// one artifact written by a future schema must not hide the rest.
fn load_rows<T: serde::de::DeserializeOwned>(
    store: &dyn Store,
    prefix: &str,
    belongs: impl Fn(&T) -> bool,
) -> Result<Vec<T>, StoreError> {
    Ok(store
        .list_kv_prefix(prefix)?
        .into_iter()
        .filter_map(|(_, value)| {
            let decoded = serde_json::from_str::<T>(&value).ok()?;
            belongs(&decoded).then_some(decoded)
        })
        .collect())
}

/// Every persisted report for `workspace`, oldest first.
pub fn list_reports(store: &dyn Store, workspace: &str) -> Result<Vec<ReviewArtifact>, StoreError> {
    load_rows::<ReviewArtifact>(store, &report_prefix(workspace), |a| {
        a.workspace == workspace
    })
}

/// One report by id, or `None` when this workspace has no such report.
pub fn get_report(
    store: &dyn Store,
    workspace: &str,
    id: &str,
) -> Result<Option<ReviewArtifact>, StoreError> {
    Ok(list_reports(store, workspace)?
        .into_iter()
        .find(|report| report.id == id))
}

/// Every persisted fixer result for `workspace`, oldest first.
pub fn list_results(store: &dyn Store, workspace: &str) -> Result<Vec<ReviewResult>, StoreError> {
    load_rows::<ReviewResult>(store, &result_prefix(workspace), |r| {
        r.workspace == workspace
    })
}

/// The sequence a new artifact under `prefix` should take: one past the highest
/// in use. Derived from the *keys*, not from the decoded rows, so a row this
/// build cannot parse still reserves its slot instead of being overwritten.
fn next_seq(store: &dyn Store, prefix: &str) -> Result<u64, StoreError> {
    Ok(store
        .list_kv_prefix(prefix)?
        .into_iter()
        .filter_map(|(key, _)| key.rsplit(':').next()?.parse::<u64>().ok())
        .max()
        .map_or(1, |max| max + 1))
}

/// The keys to delete so `prefix` holds at most `cap` rows once one more is
/// inserted. Oldest first, which the zero-padded sequence makes lexical order.
fn prune_keys(store: &dyn Store, prefix: &str, cap: usize) -> Result<Vec<String>, StoreError> {
    let mut keys: Vec<String> = store
        .list_kv_prefix(prefix)?
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    keys.sort();
    let over = (keys.len() + 1).saturating_sub(cap);
    keys.truncate(over);
    Ok(keys)
}

/// The id a report ingested now would take. Allocated before ingestion because
/// the id is part of the artifact, and returned to the caller so the whole
/// insert can ride one batch.
pub fn next_report_id(store: &dyn Store, workspace: &str) -> Result<String, StoreError> {
    Ok(artifact_id(
        'r',
        next_seq(store, &report_prefix(workspace))?,
    ))
}

/// The id a result ingested now would take.
pub fn next_result_id(store: &dyn Store, workspace: &str) -> Result<String, StoreError> {
    Ok(artifact_id(
        'x',
        next_seq(store, &result_prefix(workspace))?,
    ))
}

/// Persist `report`, pruning the workspace back to its retention cap.
///
/// Insert and prune ride one `apply_batch`, so a concurrent reader never sees
/// the workspace momentarily over its cap or missing the new report.
pub fn save_report(store: &dyn Store, report: &ReviewArtifact) -> Result<(), StoreError> {
    let prefix = report_prefix(&report.workspace);
    let seq = seq_of(&report.id, 'r').ok_or_else(|| {
        StoreError::Backend(format!("review id {:?} is not an `r<seq>` id", report.id))
    })?;
    let value = serde_json::to_string(report)
        .map_err(|error| StoreError::Backend(format!("encode review: {error}")))?;
    let mut mutations = vec![StoreMutation::SetKv {
        key: row_key(&prefix, seq),
        value,
    }];
    for key in prune_keys(store, &prefix, REPORTS_PER_WORKSPACE)? {
        mutations.push(StoreMutation::DeleteKv { key });
    }
    store.apply_batch(&mutations)
}

/// Persist `result`, pruning the workspace back to its retention cap.
pub fn save_result(store: &dyn Store, result: &ReviewResult) -> Result<(), StoreError> {
    let prefix = result_prefix(&result.workspace);
    let seq = seq_of(&result.id, 'x').ok_or_else(|| {
        StoreError::Backend(format!("result id {:?} is not an `x<seq>` id", result.id))
    })?;
    let value = serde_json::to_string(result)
        .map_err(|error| StoreError::Backend(format!("encode review result: {error}")))?;
    let mut mutations = vec![StoreMutation::SetKv {
        key: row_key(&prefix, seq),
        value,
    }];
    for key in prune_keys(store, &prefix, RESULTS_PER_WORKSPACE)? {
        mutations.push(StoreMutation::DeleteKv { key });
    }
    store.apply_batch(&mutations)
}

/// A one-line summary of `report` for a listing: enough to recognize which
/// review it is without shipping every finding's evidence into the caller's
/// context.
pub fn report_summary(report: &ReviewArtifact) -> serde_json::Value {
    serde_json::json!({
        "report_id": report.id,
        // Explicit, because the summaries list drafts too: a fixer reading
        // `reports[]` instead of `selection` must not be able to mistake an
        // unbindable draft for something it can work from.
        "bindable": report.is_bindable(),
        "status": match report.status {
            ArtifactStatus::Completed => "completed",
            ArtifactStatus::Draft => "draft",
        },
        "findings": report.findings.len(),
        "scope": report.scope,
        "origin": report.origin,
        "agent": report.agent,
        "run_id": report.run_id,
        "created_at_ms": report.created_at_ms,
        "defects": report.defects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_core::{
        FindingInput, OutcomeInput, ResultIngest, ReviewIngest, ReviewOrigin,
        ReviewResultSubmission, ReviewScope, ReviewSubmission,
    };
    use lazybox_store::SqliteStore;

    const WS: &str = "gh:acme/repo#7";

    fn store() -> (tempfile::TempDir, SqliteStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SqliteStore::open(dir.path().join("state.db")).expect("open store");
        (dir, store)
    }

    fn finding() -> FindingInput {
        FindingInput {
            title: "drops the error".to_string(),
            severity: "blocker".to_string(),
            anchors: vec!["src/poll.rs:88".to_string()],
            evidence: "a 500 returns Ok(vec![])".to_string(),
            ..Default::default()
        }
    }

    fn report(store: &dyn Store, workspace: &str, head: &str, at: i64) -> ReviewArtifact {
        let id = next_report_id(store, workspace).expect("next id");
        ReviewSubmission {
            report: "findings".to_string(),
            findings: vec![finding()],
            scope: ReviewScope {
                label: "diff vs main".to_string(),
                head_sha: Some(head.to_string()),
                ..Default::default()
            },
            ..Default::default()
        }
        .into_artifact(ReviewIngest {
            id,
            workspace: workspace.to_string(),
            repo: Some("acme/repo".to_string()),
            run_id: "run-1".to_string(),
            agent: Some("claude".to_string()),
            origin: ReviewOrigin::Submitted,
            created_at_ms: at,
        })
    }

    #[test]
    fn a_report_survives_a_reopen_of_the_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");
        let saved = {
            let store = SqliteStore::open(&path).expect("open");
            let artifact = report(&store, WS, "bbb", 10);
            save_report(&store, &artifact).expect("save");
            artifact
        };
        // A fresh handle on the same file is what a daemon restart sees.
        let store = SqliteStore::open(&path).expect("reopen");
        let read = list_reports(&store, WS).expect("list");
        assert_eq!(read, vec![saved]);
    }

    #[test]
    fn ids_advance_and_reports_read_back_oldest_first() {
        let (_dir, store) = store();
        let first = report(&store, WS, "aaa", 10);
        save_report(&store, &first).expect("save");
        let second = report(&store, WS, "bbb", 20);
        save_report(&store, &second).expect("save");
        assert_eq!(first.id, "r1");
        assert_eq!(second.id, "r2");
        let ids: Vec<String> = list_reports(&store, WS)
            .expect("list")
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec!["r1".to_string(), "r2".to_string()]);
    }

    #[test]
    fn a_workspaces_reports_are_invisible_to_another() {
        let (_dir, store) = store();
        save_report(&store, &report(&store, WS, "aaa", 10)).expect("save");
        let other = "gh:acme/repo#9";
        save_report(&store, &report(&store, other, "bbb", 20)).expect("save");
        assert_eq!(list_reports(&store, WS).expect("list").len(), 1);
        assert_eq!(list_reports(&store, other).expect("list").len(), 1);
        assert!(get_report(&store, other, "r1").expect("get").is_some());
    }

    /// The encoding is injective, so the pairs that collapse together under a
    /// collapse-to-`_` scheme stay distinct. `my-org/tools` vs `my/org-tools`
    /// are two REAL repo shapes — a hyphen in the owner against a hyphen in
    /// the repo — which is what makes this reachable rather than theoretical.
    #[test]
    fn distinct_workspaces_never_share_a_key_prefix() {
        for (a, b) in [
            ("github:my-org/tools#42", "github:my/org-tools#42"),
            ("github:a-b/c#7", "github:a/b-c#7"),
            ("gh:acme/repo#7", "gh_acme_repo_7"),
        ] {
            assert_ne!(
                encode_key(a),
                encode_key(b),
                "{a} and {b} share an encoded prefix"
            );
            assert_ne!(report_prefix(a), report_prefix(b));
            assert_ne!(result_prefix(a), result_prefix(b));
        }
        // And the encoding stays inside the alphabet the key structure needs:
        // a `:` in the workspace segment would break `next_seq`'s split.
        let encoded = encode_key("github:my-org/tools#42");
        assert!(
            encoded
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "{encoded}"
        );
    }

    /// Reads were always filtered by the stored `workspace`; RETENTION was
    /// not, and retention is what deletes. One workspace filling its
    /// allowance and a second submitting once used to evict the first's
    /// oldest report (observed: 20 -> 19). This asserts the write path, not
    /// just the read path — the distinction the previous test missed.
    #[test]
    fn one_workspaces_writes_never_evict_anothers_reports() {
        let (_dir, store) = store();
        let a = "github:my-org/tools#42";
        let b = "github:my/org-tools#42";
        for i in 0..REPORTS_PER_WORKSPACE {
            let artifact = report(&store, a, "aaa", i as i64);
            save_report(&store, &artifact).expect("save");
        }
        assert_eq!(
            list_reports(&store, a).expect("list").len(),
            REPORTS_PER_WORKSPACE
        );
        for i in 0..REPORTS_PER_WORKSPACE {
            let artifact = report(&store, b, "bbb", 100 + i as i64);
            save_report(&store, &artifact).expect("save");
        }
        assert_eq!(
            list_reports(&store, a).expect("list").len(),
            REPORTS_PER_WORKSPACE,
            "workspace B's writes evicted workspace A's reports"
        );
        assert_eq!(
            list_reports(&store, b).expect("list").len(),
            REPORTS_PER_WORKSPACE
        );
        // Sequences are per-workspace too, so ids don't skip.
        assert_eq!(list_reports(&store, b).expect("list")[0].id, "r1");
    }

    #[test]
    fn retention_prunes_the_oldest_reports() {
        let (_dir, store) = store();
        for i in 0..(REPORTS_PER_WORKSPACE + 3) {
            let artifact = report(&store, WS, "aaa", i as i64);
            save_report(&store, &artifact).expect("save");
        }
        let kept = list_reports(&store, WS).expect("list");
        assert_eq!(kept.len(), REPORTS_PER_WORKSPACE);
        // The three oldest are gone; the newest is the last id allocated.
        assert_eq!(kept[0].id, "r4");
        assert_eq!(
            kept[kept.len() - 1].id,
            format!("r{}", REPORTS_PER_WORKSPACE + 3)
        );
    }

    #[test]
    fn a_result_persists_beside_the_report_without_touching_it() {
        let (_dir, store) = store();
        let artifact = report(&store, WS, "aaa", 10);
        save_report(&store, &artifact).expect("save");
        let result = ReviewResultSubmission {
            report_id: artifact.id.clone(),
            outcomes: vec![OutcomeInput {
                finding_id: "f1".to_string(),
                disposition: "fixed".to_string(),
                evidence: "propagated the error".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }
        .into_artifact(
            &artifact,
            ResultIngest {
                id: next_result_id(&store, WS).expect("id"),
                run_id: "run-2".to_string(),
                agent: Some("codex".to_string()),
                created_at_ms: 40,
            },
        );
        save_result(&store, &result).expect("save");
        assert_eq!(result.id, "x1");
        assert_eq!(list_results(&store, WS).expect("list"), vec![result]);
        // The report is untouched by its own result.
        assert_eq!(list_reports(&store, WS).expect("list"), vec![artifact]);
    }

    #[test]
    fn an_undecodable_row_hides_neither_its_neighbours_nor_its_slot() {
        let (_dir, store) = store();
        save_report(&store, &report(&store, WS, "aaa", 10)).expect("save");
        store
            .set_kv(&row_key(&report_prefix(WS), 2), "{\"not\":\"a report\"}")
            .expect("poison a row");
        let readable = list_reports(&store, WS).expect("list");
        assert_eq!(readable.len(), 1, "the good report still reads back");
        // Seq allocation walks keys, not decoded rows, so the poisoned slot is
        // not handed out again and silently overwritten.
        assert_eq!(next_report_id(&store, WS).expect("next"), "r3");
    }
}
