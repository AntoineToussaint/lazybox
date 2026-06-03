//! Slug helpers — turn human strings into filesystem-safe identifiers.
//!
//! Lazybox uses these to derive on-disk worktree paths from PR titles
//! and user-supplied workspace names. The goal is something a person
//! reading a shell prompt can identify at a glance:
//!
//! ```text
//! ~/.lazybox/v2/worktrees/PR-7413-propagate-status-code/
//! ```
//!
//! Rules:
//! - ASCII-lowercase
//! - Alphanumeric + `-` only (everything else replaced or dropped)
//! - Word-bounded: split on whitespace + non-alnum, rejoin with `-`
//! - Capped at `MAX_WORDS` words so the path stays scannable
//! - Empty input falls back to a stable placeholder so the caller can
//!   still produce a usable path

const MAX_WORDS: usize = 8;
const MAX_TOTAL_LEN: usize = 60;

/// Slugify a free-form string. Up to 8 words, ASCII-lowercase, dashes
/// for separators. Returns an empty string when the input has no
/// usable characters — callers should fall back to a placeholder.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut current = String::new();
    let mut words: Vec<String> = Vec::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
            if words.len() >= MAX_WORDS {
                break;
            }
        }
    }
    if !current.is_empty() && words.len() < MAX_WORDS {
        words.push(current);
    }
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            out.push('-');
        }
        out.push_str(w);
    }
    if out.len() > MAX_TOTAL_LEN {
        // Trim to a word boundary near the cap so we don't slice
        // mid-word — looks weirder than a clean truncation.
        let cut = out[..MAX_TOTAL_LEN].rfind('-').unwrap_or(MAX_TOTAL_LEN);
        out.truncate(cut);
    }
    out
}

/// Compose `PR-{num}-{slugified_title}`. Falls back to `PR-{num}` if
/// the title slug is empty (e.g. emoji-only title).
pub fn pr_slug(num: u64, title: &str) -> String {
    let title_part = slugify(title);
    if title_part.is_empty() {
        format!("PR-{num}")
    } else {
        format!("PR-{num}-{title_part}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic_title() {
        assert_eq!(slugify("Add login flow"), "add-login-flow");
    }

    #[test]
    fn slugify_drops_non_alnum() {
        assert_eq!(slugify("fix: bug #123!"), "fix-bug-123");
    }

    #[test]
    fn slugify_caps_at_eight_words() {
        let s = "one two three four five six seven eight nine ten";
        let out = slugify(s);
        assert_eq!(out, "one-two-three-four-five-six-seven-eight");
    }

    #[test]
    fn slugify_truncates_long_results_at_word_boundary() {
        let long = "very-long-title-that-keeps-going-and-going-with-more-words";
        let out = slugify(long);
        assert!(out.len() <= 60);
        assert!(!out.ends_with('-'), "no dangling dash");
    }

    #[test]
    fn slugify_empty_input() {
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("   "), "");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn pr_slug_full() {
        assert_eq!(
            pr_slug(7413, "Propagate status code in FatalStreamError"),
            "PR-7413-propagate-status-code-in-fatalstreamerror"
        );
    }

    #[test]
    fn pr_slug_empty_title_falls_back() {
        assert_eq!(pr_slug(42, ""), "PR-42");
        assert_eq!(pr_slug(42, "🚀"), "PR-42");
    }

    #[test]
    fn pr_slug_caps_words() {
        let title = "this is a very long pull request title with way too many words to keep";
        let out = pr_slug(1, title);
        // PR-1- + 8 words from the title, joined with dashes.
        let title_words = out.strip_prefix("PR-1-").unwrap().split('-').count();
        assert_eq!(title_words, 8);
    }
}
