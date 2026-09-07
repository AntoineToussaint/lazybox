//! Parse "Fixes #123" / "Closes ENG-45" / "Resolves owner/repo#7"
//! style links out of a PR body. Used to auto-link issues to a
//! workspace at attach time.
//!
//! ## What's recognized
//!
//! GitHub's documented closing-keyword set + a few common synonyms:
//! `close`, `closes`, `closed`, `fix`, `fixes`, `fixed`, `resolve`,
//! `resolves`, `resolved`. Case-insensitive. Must be followed by
//! whitespace and one of:
//!
//! - `#123` — same-repo GitHub issue
//! - `owner/repo#123` — cross-repo GitHub issue
//! - `ENG-45`, `ENG-456`, `ABC-1` — Linear-style ticket key
//!
//! Markdown links like `[ENG-45](https://linear.app/.../ENG-45)` are
//! parsed via the linear-key path: we only need the key to find the
//! task in the polled set.
//!
//! ## What's NOT recognized
//!
//! Plain bare `#123` without a closing keyword (too noisy — many
//! PRs reference issues without intending to close them). The user
//! can always attach manually if they want a non-closing link.

use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IssueLink {
    /// `#123` or `owner/repo#123`. `repo` is the optional explicit
    /// owner/repo; `None` means same-repo as the PR.
    GitHub { repo: Option<String>, number: u64 },
    /// `ENG-45`. Caller maps the prefix to a Linear team if needed.
    Linear { key: String },
}

const KEYWORDS: &[&str] = &[
    "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
];

/// Pull every `IssueLink` mention out of `body`. Order is preserved
/// in a sorted set for deterministic output (lets tests rely on the
/// shape and means re-parsing the same body returns the same value).
pub fn extract(body: &str) -> Vec<IssueLink> {
    let mut out = BTreeSet::new();
    for token in tokenize_with(body, KEYWORDS, 80) {
        if let Some(link) = parse_link_after_keyword(&token) {
            out.insert(link);
        }
    }
    out.into_iter().collect()
}

/// Keywords that introduce a *blocking* reference. Multi-word entries work
/// because the matcher is a plain substring compare on the lowercased body,
/// and the separator check already admits `Blocked by: owner/repo#7`.
const BLOCKED_KEYWORDS: &[&str] = &[
    "blocked by",
    "blocked-by",
    "blockedby",
    "depends on",
    "depends-on",
    "dependson",
];

/// Every `Blocked by:` / `Depends on:` reference in `body`, deduplicated,
/// deterministic order. Same link grammar as [`extract`].
pub fn extract_blocked_by(body: &str) -> Vec<IssueLink> {
    let mut out = BTreeSet::new();
    for token in tokenize_with(body, BLOCKED_KEYWORDS, 80) {
        if let Some(link) = parse_link_after_keyword(&token) {
            out.insert(link);
        }
    }
    out.into_iter().collect()
}

/// Keywords that introduce a *declared* (free-text) blocker.
const BLOCKED_ON_KEYWORDS: &[&str] = &["blocked on", "blocked-on", "blockedon"];

/// Maximum length of a declared `Blocked on:` reason before truncation.
const BLOCKED_ON_MAX: usize = 200;

/// `Blocked on: <reason>` — a declared blocker with a human-readable reason
/// (a decision, a credential, an outside party). Returns the rest of that
/// line, trimmed, capped at 200 chars; the LAST such line wins so an
/// updated reason replaces the old one. `None` when absent or empty.
/// Distinct from [`extract_blocked_by`], which yields task links: if the
/// text after the keyword parses as a link, it is a `blocked_by` edge and
/// this returns `None` for that occurrence.
pub fn extract_blocked_on(body: &str) -> Option<String> {
    let mut reason = None;
    for token in tokenize_with(body, BLOCKED_ON_KEYWORDS, BLOCKED_ON_MAX * 4 + 16) {
        // An ambiguous `Blocked on #12` is a task edge, not free text —
        // defer to `extract_blocked_by` and skip it here.
        if parse_link_after_keyword(&token).is_some() {
            continue;
        }
        let line = token
            .trim_start_matches(|c: char| c.is_whitespace() || c == ':')
            .lines()
            .next()
            .unwrap_or("")
            .trim();
        if line.is_empty() {
            continue;
        }
        let capped: String = line.chars().take(BLOCKED_ON_MAX).collect();
        reason = Some(capped);
    }
    reason
}

/// Split the body into "after-keyword" candidate strings. We look at
/// every position after one of `keywords` and grab the next `window` bytes
/// to scan, since "Fixes #1, #2, #3" should hit all three. The link
/// extractors use a small window; the free-text `Blocked on:` reason needs
/// a wider one so a long line survives to its own 200-char cap.
fn tokenize_with(body: &str, keywords: &[&str], window: usize) -> Vec<String> {
    let lower = body.to_lowercase();
    let mut tokens = Vec::new();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // `lower[i..]` would panic if `i` lands mid-UTF-8-codepoint.
        // Skip past byte positions that aren't char boundaries — the
        // keyword set is ASCII so a non-boundary byte can't possibly
        // start a match anyway.
        if !lower.is_char_boundary(i) {
            i += 1;
            continue;
        }
        for kw in keywords {
            if lower[i..].starts_with(kw) {
                let after = i + kw.len();
                if after >= bytes.len() {
                    break;
                }
                let next = bytes[after];
                // Need a separator after the keyword so we don't match
                // inside other words ("foreclosed" shouldn't match).
                if !next.is_ascii_whitespace() && next != b':' {
                    continue;
                }
                // Take `window` bytes past the keyword. Snap `end` back to
                // a char boundary so multi-byte chars (`…`, em-dash,
                // emoji, …) sitting on the window edge don't
                // panic when we slice. `floor_char_boundary` is
                // unstable, so walk back manually.
                let mut end = (after + window).min(body.len());
                while end > after && !body.is_char_boundary(end) {
                    end -= 1;
                }
                tokens.push(body[after..end].to_string());
            }
        }
        i += 1;
    }
    tokens
}

fn parse_link_after_keyword(s: &str) -> Option<IssueLink> {
    // Strip leading whitespace + colon.
    let s = s.trim_start_matches(|c: char| c.is_whitespace() || c == ':');

    // GitHub same-repo: `#123`
    if let Some(rest) = s.strip_prefix('#')
        && let Some(num) = take_digits(rest)
    {
        return Some(IssueLink::GitHub {
            repo: None,
            number: num,
        });
    }

    // GitHub cross-repo: `owner/repo#123`
    if let Some((repo, rest)) = split_repo_hash(s)
        && let Some(num) = take_digits(rest)
    {
        return Some(IssueLink::GitHub {
            repo: Some(repo),
            number: num,
        });
    }

    // Linear: `ABC-123`. ABC must be 2-10 alpha chars.
    if let Some(key) = take_linear_key(s) {
        return Some(IssueLink::Linear { key });
    }

    None
}

fn take_digits(s: &str) -> Option<u64> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

fn split_repo_hash(s: &str) -> Option<(String, &str)> {
    let hash_at = s.find('#')?;
    let repo = &s[..hash_at];
    if !repo
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '-' || c == '_' || c == '.')
        || !repo.contains('/')
    {
        return None;
    }
    Some((repo.to_string(), &s[hash_at + 1..]))
}

fn take_linear_key(s: &str) -> Option<String> {
    let mut chars = s.chars();
    let prefix: String = chars
        .by_ref()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if !(2..=10).contains(&prefix.len()) {
        return None;
    }
    // Need a hyphen separator.
    let after_prefix = &s[prefix.len()..];
    let rest = after_prefix.strip_prefix('-')?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let prefix_upper = prefix.to_uppercase();
    Some(format!("{prefix_upper}-{digits}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_simple_same_repo_issue() {
        let body = "This PR fixes #42.";
        assert_eq!(
            extract(body),
            vec![IssueLink::GitHub {
                repo: None,
                number: 42
            }]
        );
    }

    #[test]
    fn extracts_cross_repo_issue() {
        let body = "Closes acme/widgets#7";
        assert_eq!(
            extract(body),
            vec![IssueLink::GitHub {
                repo: Some("acme/widgets".into()),
                number: 7
            }]
        );
    }

    #[test]
    fn extracts_linear_ticket() {
        let body = "Resolves ENG-456 with the new flow.";
        assert_eq!(
            extract(body),
            vec![IssueLink::Linear {
                key: "ENG-456".into()
            }]
        );
    }

    #[test]
    fn extracts_multiple_links_in_one_body() {
        let body = "Fixes #1\n\nAlso closes ENG-2 and resolves acme/foo#99.";
        let links = extract(body);
        assert!(links.contains(&IssueLink::GitHub {
            repo: None,
            number: 1
        }));
        assert!(links.contains(&IssueLink::Linear {
            key: "ENG-2".into()
        }));
        assert!(links.contains(&IssueLink::GitHub {
            repo: Some("acme/foo".into()),
            number: 99
        }));
    }

    #[test]
    fn ignores_bare_hash_without_keyword() {
        // "PR #5" alone shouldn't auto-link — only a closing keyword
        // is intentional enough to mean "this workspace is for that
        // issue."
        assert!(extract("Built on top of #5").is_empty());
    }

    #[test]
    fn ignores_keyword_inside_other_words() {
        // "foreclosed" contains "closed" but isn't a verb form.
        assert!(extract("This branch was foreclosed#42 incorrectly").is_empty());
    }

    #[test]
    fn handles_colon_separator() {
        let body = "Fixes: #99";
        assert_eq!(
            extract(body),
            vec![IssueLink::GitHub {
                repo: None,
                number: 99
            }]
        );
    }

    #[test]
    fn case_insensitive_keyword_matching() {
        let body = "FIXES #1\nFixed #2\nfix #3";
        let links = extract(body);
        for n in [1u64, 2, 3] {
            assert!(links.contains(&IssueLink::GitHub {
                repo: None,
                number: n
            }));
        }
    }

    #[test]
    fn deduplicates_repeated_mentions() {
        let body = "Fixes #1. Also fixes #1 because we mean it.";
        assert_eq!(
            extract(body),
            vec![IssueLink::GitHub {
                repo: None,
                number: 1
            }]
        );
    }

    #[test]
    fn empty_body_returns_empty() {
        assert!(extract("").is_empty());
    }

    #[test]
    fn linear_prefix_too_short_or_long_rejected() {
        assert!(extract("Fixes A-1").is_empty());
        assert!(extract("Fixes ABCDEFGHIJK-1").is_empty());
    }

    #[test]
    fn does_not_panic_on_multibyte_around_token_window() {
        // Regression: the tokenizer used to slice `body[after..end]`
        // with `end = after + 80`, which could land inside a UTF-8
        // multi-byte sequence. A PR body like `Closes #73.\n\nSummary: …`
        // happens to put the trailing `…` straddling that boundary
        // and would panic at the slice. We now snap `end` back to a
        // char boundary; the link is still extracted.
        let body = "Closes #73.\n\nSummary: …";
        let links = extract(body);
        assert_eq!(
            links,
            vec![IssueLink::GitHub {
                repo: None,
                number: 73,
            }],
        );
    }

    #[test]
    fn blocked_by_extracts_same_repo() {
        assert_eq!(
            extract_blocked_by("Blocked by: #4"),
            vec![IssueLink::GitHub {
                repo: None,
                number: 4
            }]
        );
    }

    #[test]
    fn blocked_by_extracts_cross_repo() {
        assert_eq!(
            extract_blocked_by("blocked by owner/repo#4"),
            vec![IssueLink::GitHub {
                repo: Some("owner/repo".into()),
                number: 4
            }]
        );
    }

    #[test]
    fn blocked_by_accepts_depends_on_and_hyphenated_forms() {
        for body in [
            "Depends on #7",
            "depends-on #7",
            "DependsOn #7",
            "blocked-by #7",
            "BlockedBy #7",
        ] {
            assert_eq!(
                extract_blocked_by(body),
                vec![IssueLink::GitHub {
                    repo: None,
                    number: 7
                }],
                "body: {body:?}"
            );
        }
    }

    #[test]
    fn blocked_by_ignores_closes_keywords() {
        // A closing keyword is not a blocking keyword.
        assert!(extract_blocked_by("Closes #3").is_empty());
    }

    #[test]
    fn closes_ignores_blocked_by_keywords() {
        // Regression: `extract` (closing links) must not pick up a
        // `Blocked by:` reference.
        assert!(extract("Blocked by: #9").is_empty());
    }

    #[test]
    fn blocked_by_dedupes_and_sorts() {
        let body = "Blocked by #2. Also depends on #1. Blocked by #2 again.";
        assert_eq!(
            extract_blocked_by(body),
            vec![
                IssueLink::GitHub {
                    repo: None,
                    number: 1
                },
                IssueLink::GitHub {
                    repo: None,
                    number: 2
                },
            ]
        );
    }

    #[test]
    fn blocked_by_does_not_panic_on_multibyte_window() {
        let body = "Blocked by #73.\n\nSummary: …";
        assert_eq!(
            extract_blocked_by(body),
            vec![IssueLink::GitHub {
                repo: None,
                number: 73,
            }]
        );
    }

    #[test]
    fn blocked_on_returns_trimmed_reason_last_line_wins() {
        let body = "Blocked on: waiting on a design decision\n\nBlocked on: legal review pending";
        assert_eq!(
            extract_blocked_on(body).as_deref(),
            Some("legal review pending")
        );
    }

    #[test]
    fn blocked_on_defers_to_blocked_by_when_the_text_is_a_link() {
        // `Blocked on #12` is a task edge, not a free-text reason: the
        // reason extractor yields nothing so it doesn't shadow the edge.
        assert_eq!(extract_blocked_on("Blocked on #12"), None);
        assert_eq!(extract_blocked_on("Blocked on owner/repo#12"), None);
    }

    #[test]
    fn blocked_on_caps_length() {
        let reason = "x".repeat(500);
        let body = format!("Blocked on: {reason}");
        let got = extract_blocked_on(&body).unwrap();
        assert_eq!(got.chars().count(), 200);
    }
}
