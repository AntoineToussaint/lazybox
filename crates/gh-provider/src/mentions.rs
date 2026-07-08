//! Detect `@lazybox` mentions in GitHub issue bodies and comments and
//! turn them into auto-spawn triggers.
//!
//! ## What "trigger" means
//!
//! When an allowed user writes `@lazybox` in an issue body or comment,
//! lazybox reacts 👀 on that body/comment and spawns the default agent
//! with the implement-issue prompt — same end-state as the user
//! manually pressing `w` on the issue row, just triggered by the
//! mention.
//!
//! ## Idempotency via 👀 reaction
//!
//! The `@lazybox` text stays in the comment forever; we'd re-spawn
//! every 60s poll cycle without a marker. We use GitHub's reaction
//! API: lazybox adds 👀 the first time it sees a mention, and on
//! subsequent polls we skip targets where `viewerHasReacted == true`.
//! The reaction is authoritative — no separate kv-store needed, and
//! humans reading the issue see the eyes emoji so they know lazybox
//! picked it up.
//!
//! ## Allowlist
//!
//! Anyone with comment permission can write `@lazybox`, but spending
//! tokens + creating branches needs gating. The allowlist defaults
//! to "just the authenticated lazybox user"; users can extend it via
//! `mention.allowed_logins` in `~/.lazybox/config.yaml`.

use crate::graphql::{GqlIssue, GqlReactionView};
use std::collections::BTreeSet;

/// One detected `@lazybox` mention that warrants an auto-spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LazyboxMention {
    /// Repo as `owner/name`. Extracted from the issue's `repository`
    /// field or parsed from the URL.
    pub repo: String,
    /// 1-based issue number (the `#N` part of `owner/repo#N`).
    pub issue_number: u64,
    /// GraphQL node id of the issue (set when the issue body itself
    /// is the source; populated even for comment-sourced mentions so
    /// the caller has a single field to log).
    pub issue_node_id: Option<String>,
    /// GraphQL node id of the thing to react 👀 on — the Issue when
    /// `source == Body`, the IssueComment when
    /// `source == Comment { .. }`.
    pub target_node_id: String,
    /// Where the mention came from.
    pub source: MentionSource,
    /// Login that wrote the `@lazybox` text. The caller already
    /// allow-listed this — included for logging / audit only.
    pub triggered_by_login: String,
}

/// Which surface the mention lived on. Today only Issue body and
/// IssueComment are scanned (PR scope is deliberately deferred per
/// the design doc — PRs have `w`-on-comments already).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MentionSource {
    /// `@lazybox` lives in the issue body itself.
    Body,
    /// `@lazybox` lives in an issue comment with this node id.
    Comment { comment_id: String },
}

/// True when `text` contains a `@lazybox` mention at a word boundary.
/// Case-insensitive on the `lazybox` portion (matches GitHub's
/// own mention rendering). Avoids false positives like `@lazyboxs`,
/// `@lazybox-bot`, `email@lazybox.io`.
///
/// We accept mentions inside `>` quote blocks — the design doc calls
/// this out as an acceptable false-positive ("user can kill via
/// Shift-X"). Detecting quotes line-by-line was deemed not worth the
/// complexity for an MVP.
pub fn contains_lazybox_mention(text: &str) -> bool {
    let bytes = text.as_bytes();
    let needle_lower = b"@lazybox";
    let n = needle_lower.len();
    if bytes.len() < n {
        return false;
    }
    for i in 0..=bytes.len() - n {
        let window = &bytes[i..i + n];
        if !window
            .iter()
            .zip(needle_lower.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            continue;
        }
        // Pre-boundary: `@` must NOT be preceded by an identifier
        // char — otherwise `email@lazybox.io` would match.
        if i > 0 {
            let prev = bytes[i - 1];
            if is_login_char(prev) || prev == b'@' {
                continue;
            }
        }
        // Post-boundary: the char after `@lazybox` must NOT be a
        // login-continuation char — otherwise `@lazyboxs`, `@lazybox1`,
        // `@lazybox-bot` would match. `.` is also rejected to skip
        // `@lazybox.io` style email-likes.
        if let Some(&next) = bytes.get(i + n)
            && (is_login_char(next) || next == b'.' || next == b'@')
        {
            continue;
        }
        return true;
    }
    false
}

/// GitHub login alphabet for word-boundary checks: ASCII alnum +
/// hyphen + underscore. Conservative — GitHub logins are alnum +
/// hyphen, but allowing underscore matches mentions inside snake_case
/// identifiers (which we'd want to treat as a boundary, not part of
/// the word, but here we use "membership in the alphabet" as the
/// boundary signal so underscore IS a non-boundary char — same
/// treatment as a letter).
fn is_login_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Scan an issue (body + every comment) for `@lazybox` mentions worth
/// triggering an auto-spawn for. A mention triggers when ALL of:
///
/// 1. The text contains `@lazybox` at a word boundary.
/// 2. The author's login is in `allowed_logins`.
/// 3. The corresponding reaction record has `viewerHasReacted ==
///    false` — i.e. lazybox hasn't already acknowledged this surface.
///
/// Returns one [`LazyboxMention`] per qualifying surface. Multiple
/// `@lazybox` strings inside a single comment coalesce into one
/// trigger (the design doc's "coalesce to one spawn" requirement).
/// The body and individual comments produce SEPARATE triggers — the
/// caller can dedupe at workspace level if it wants only one spawn
/// per issue.
pub fn scan_issue(issue: &GqlIssue, allowed_logins: &BTreeSet<String>) -> Vec<LazyboxMention> {
    let mut out = Vec::new();
    let repo = match issue.repository.as_ref() {
        Some(r) => r.name_with_owner.clone(),
        None => extract_repo_from_url(&issue.url),
    };
    let issue_node_id = issue.id.clone();

    // Body. Author present + allowed + mention + no prior 👀.
    if let Some(author) = issue.author.as_ref()
        && allowed_logins.contains(&author.login)
        && contains_lazybox_mention(issue.body.as_deref().unwrap_or(""))
        && !viewer_has_eyes_reacted(issue.reactions.as_ref())
        && let Some(node_id) = issue.id.as_ref()
    {
        out.push(LazyboxMention {
            repo: repo.clone(),
            issue_number: issue.number,
            issue_node_id: issue_node_id.clone(),
            target_node_id: node_id.clone(),
            source: MentionSource::Body,
            triggered_by_login: author.login.clone(),
        });
    }

    for comment in &issue.comments.nodes {
        let Some(author) = comment.author.as_ref() else {
            continue;
        };
        if !allowed_logins.contains(&author.login) {
            continue;
        }
        if !contains_lazybox_mention(&comment.body) {
            continue;
        }
        if viewer_has_eyes_reacted(comment.reactions.as_ref()) {
            continue;
        }
        let Some(comment_id) = comment.id.clone() else {
            // No node id → can't post a reaction → can't be
            // idempotent. Skip and log — the only way this should
            // happen is if a GraphQL response is malformed.
            tracing::warn!(
                repo = %repo,
                issue = issue.number,
                author = %author.login,
                "comment with @lazybox mention has no node id; skipping (would re-spawn every poll)"
            );
            continue;
        };
        out.push(LazyboxMention {
            repo: repo.clone(),
            issue_number: issue.number,
            issue_node_id: issue_node_id.clone(),
            target_node_id: comment_id.clone(),
            source: MentionSource::Comment { comment_id },
            triggered_by_login: author.login.clone(),
        });
    }

    out
}

fn viewer_has_eyes_reacted(r: Option<&GqlReactionView>) -> bool {
    r.is_some_and(|v| v.viewer_has_reacted)
}

/// Tiny URL parser for the fallback path when `repository.nameWithOwner`
/// is missing on a search result. Same shape `graphql::extract_repo_from_url`
/// uses; duplicated here to keep `mentions` independent of `graphql`'s
/// pub-vs-private surface. Returns the third + fourth path segments
/// joined by `/`, or an empty string on a malformed URL.
fn extract_repo_from_url(url: &str) -> String {
    url.splitn(4, '/')
        .nth(3)
        .unwrap_or("")
        .splitn(3, '/')
        .take(2)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphql::{
        GqlAssignees, GqlAuthor, GqlComment, GqlComments, GqlIssueRepo, GqlLabels,
    };
    use chrono::Utc;

    fn allow_only(login: &str) -> BTreeSet<String> {
        let mut s = BTreeSet::new();
        s.insert(login.to_string());
        s
    }

    fn comment(
        id: &str,
        author: Option<&str>,
        body: &str,
        eyes_reacted: bool,
    ) -> crate::graphql::GqlComment {
        GqlComment {
            id: Some(id.into()),
            author: author.map(|l| GqlAuthor { login: l.into() }),
            body: body.into(),
            created_at: Utc::now(),
            path: None,
            line: None,
            original_line: None,
            diff_hunk: None,
            reactions: Some(GqlReactionView {
                viewer_has_reacted: eyes_reacted,
            }),
        }
    }

    fn issue(
        number: u64,
        author: Option<&str>,
        body: Option<&str>,
        body_eyes: bool,
        comments: Vec<GqlComment>,
    ) -> GqlIssue {
        GqlIssue {
            id: Some(format!("I_{number}")),
            number,
            title: "title".into(),
            body: body.map(str::to_string),
            url: format!("https://github.com/o/r/issues/{number}"),
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            state: "OPEN".into(),
            author: author.map(|l| GqlAuthor { login: l.into() }),
            labels: GqlLabels { nodes: vec![] },
            assignees: GqlAssignees { nodes: vec![] },
            comments: GqlComments {
                nodes: comments,
                total_count: None,
            },
            repository: Some(GqlIssueRepo {
                name_with_owner: "o/r".into(),
            }),
            reactions: Some(GqlReactionView {
                viewer_has_reacted: body_eyes,
            }),
        }
    }

    // ── contains_lazybox_mention ──────────────────────────────────────

    #[test]
    fn detects_plain_at_lazybox() {
        assert!(contains_lazybox_mention("@lazybox please look"));
        assert!(contains_lazybox_mention("hey @lazybox"));
        assert!(contains_lazybox_mention("@lazybox\nmultiline"));
        assert!(contains_lazybox_mention("@LAZYBOX case insensitive"));
        assert!(contains_lazybox_mention("@Lazybox mixed"));
    }

    #[test]
    fn ignores_word_boundary_misses() {
        assert!(!contains_lazybox_mention("@lazyboxs love this"));
        assert!(!contains_lazybox_mention("@lazybox-bot"));
        assert!(!contains_lazybox_mention("@lazybox1"));
        assert!(!contains_lazybox_mention("@lazybox.io"));
        assert!(!contains_lazybox_mention("email@lazybox"));
        assert!(!contains_lazybox_mention("plain lazybox no at-sign"));
        assert!(!contains_lazybox_mention(""));
    }

    #[test]
    fn boundary_chars_count_as_boundaries() {
        assert!(contains_lazybox_mention("(@lazybox)"));
        assert!(contains_lazybox_mention("`@lazybox`"));
        assert!(contains_lazybox_mention("> @lazybox quoted"));
        assert!(contains_lazybox_mention("@lazybox!"));
    }

    // ── scan_issue ──────────────────────────────────────────────────

    #[test]
    fn scan_body_mention_by_allowed_user() {
        let i = issue(
            42,
            Some("alice"),
            Some("hey @lazybox please"),
            false,
            vec![],
        );
        let m = scan_issue(&i, &allow_only("alice"));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].source, MentionSource::Body);
        assert_eq!(m[0].target_node_id, "I_42");
        assert_eq!(m[0].issue_number, 42);
        assert_eq!(m[0].repo, "o/r");
        assert_eq!(m[0].triggered_by_login, "alice");
    }

    #[test]
    fn scan_skips_body_when_author_not_allowed() {
        let i = issue(1, Some("eve"), Some("@lazybox do something"), false, vec![]);
        let m = scan_issue(&i, &allow_only("alice"));
        assert!(m.is_empty());
    }

    #[test]
    fn scan_skips_body_when_already_reacted() {
        let i = issue(1, Some("alice"), Some("@lazybox do it"), true, vec![]);
        let m = scan_issue(&i, &allow_only("alice"));
        assert!(m.is_empty(), "viewerHasReacted=true should skip");
    }

    #[test]
    fn scan_skips_body_without_mention() {
        let i = issue(1, Some("alice"), Some("no mention here"), false, vec![]);
        let m = scan_issue(&i, &allow_only("alice"));
        assert!(m.is_empty());
    }

    #[test]
    fn scan_finds_comment_mention() {
        let c = comment("C_1", Some("alice"), "@lazybox fix it", false);
        let i = issue(7, Some("bob"), None, false, vec![c]);
        let m = scan_issue(&i, &allow_only("alice"));
        assert_eq!(m.len(), 1);
        assert!(matches!(
            &m[0].source,
            MentionSource::Comment { comment_id } if comment_id == "C_1"
        ));
        assert_eq!(m[0].target_node_id, "C_1");
        assert_eq!(m[0].triggered_by_login, "alice");
    }

    #[test]
    fn scan_skips_already_reacted_comment() {
        let c = comment("C_1", Some("alice"), "@lazybox", true);
        let i = issue(7, Some("bob"), None, false, vec![c]);
        let m = scan_issue(&i, &allow_only("alice"));
        assert!(m.is_empty());
    }

    #[test]
    fn scan_skips_comments_from_disallowed_users() {
        let c1 = comment("C_1", Some("eve"), "@lazybox", false);
        let c2 = comment("C_2", Some("alice"), "@lazybox", false);
        let i = issue(7, Some("bob"), None, false, vec![c1, c2]);
        let m = scan_issue(&i, &allow_only("alice"));
        assert_eq!(m.len(), 1, "only alice's comment qualifies");
        assert_eq!(m[0].target_node_id, "C_2");
    }

    #[test]
    fn scan_coalesces_repeated_mentions_in_one_comment() {
        let c = comment(
            "C_1",
            Some("alice"),
            "@lazybox @lazybox please @lazybox",
            false,
        );
        let i = issue(7, Some("bob"), None, false, vec![c]);
        let m = scan_issue(&i, &allow_only("alice"));
        assert_eq!(m.len(), 1, "one comment → one mention regardless of count");
    }

    #[test]
    fn scan_body_plus_comment_both_trigger() {
        // Author wrote @lazybox in the body AND a separate comment.
        // Both qualify (separate reactable targets); design doc
        // accepts the duplication — workspace-level dedupe can
        // happen in the caller if it cares.
        let c = comment("C_1", Some("alice"), "follow-up @lazybox", false);
        let i = issue(8, Some("alice"), Some("@lazybox please"), false, vec![c]);
        let m = scan_issue(&i, &allow_only("alice"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn scan_skips_comment_without_node_id() {
        // Defensive: GitHub returned a comment with no `id`. We
        // can't react idempotently, so we skip.
        let mut c = comment("dummy", Some("alice"), "@lazybox", false);
        c.id = None;
        let i = issue(8, Some("bob"), None, false, vec![c]);
        let m = scan_issue(&i, &allow_only("alice"));
        assert!(m.is_empty());
    }

    #[test]
    fn extract_repo_from_issue_url() {
        assert_eq!(
            super::extract_repo_from_url("https://github.com/o/r/issues/3"),
            "o/r"
        );
        assert_eq!(
            super::extract_repo_from_url("https://github.com/my-org/my-repo/issues/12"),
            "my-org/my-repo"
        );
    }
}
