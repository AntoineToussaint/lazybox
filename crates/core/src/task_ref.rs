//! Parse a human- or agent-written reference to a tracker record into a
//! [`TaskId`].
//!
//! "The tracker record is the workspace" (#1586) means every surface that
//! starts work — the `spawn_worker` MCP tool, `lazybox workspace create`, the
//! daemon's `CreateWorkspace` anchor — takes a *record* rather than a name.
//! Those references arrive in whatever shape the caller had at hand: the URL
//! `gh issue create` printed, the `owner/repo#N` an epic marker uses, a bare
//! `#N` beside a known repo, or a Linear identifier. This module is the one
//! place that maps all of them onto the ids the providers mint
//! (`github` → `owner/repo#N`, `linear` → `ENG-45`), so a reference resolves
//! to the same workspace no matter which surface it came through.
//!
//! Unrecognized input is `None`: a caller that cannot resolve a reference must
//! say so rather than mint a `TaskId` for a record that does not exist.

use crate::TaskId;

/// Parse `input` into a [`TaskId`].
///
/// `default_repo` (an `owner/repo` slug) resolves the repo-less forms — a bare
/// `#N` or `N`. Without it those return `None`, since a number alone names
/// nothing.
pub fn parse_task_ref(input: &str, default_repo: Option<&str>) -> Option<TaskId> {
    let raw = input
        .trim()
        .trim_matches(|c| c == '<' || c == '>' || c == '`');
    if raw.is_empty() {
        return None;
    }

    if let Some(path) = strip_host(raw, "github.com") {
        return github_from_url_path(path);
    }
    if let Some(path) = strip_host(raw, "linear.app") {
        return linear_from_url_path(path);
    }

    // `owner/repo#N`, or `#N` / `N` against `default_repo`.
    let (repo, number) = match raw.split_once('#') {
        Some(("", number)) => (default_repo?, number),
        Some((repo, number)) => (repo, number),
        None => match raw.parse::<u64>() {
            Ok(_) => (default_repo?, raw),
            Err(_) => return linear_identifier(raw),
        },
    };
    github_task_id(repo, number)
}

/// The `owner/repo` slug of a GitHub reference, for callers that need the repo
/// before (or without) resolving the record — the single-item sync needs it to
/// pick a client, and an error message reads better naming the repo.
pub fn github_repo_of(id: &TaskId) -> Option<&str> {
    (id.source == crate::GITHUB_SOURCE)
        .then(|| id.key.rsplit_once('#').map(|(repo, _)| repo))
        .flatten()
}

/// Strip `scheme://host/` off `raw`, returning the remaining path. `www.` is
/// accepted so a pasted browser URL resolves.
fn strip_host<'a>(raw: &'a str, host: &str) -> Option<&'a str> {
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or(raw);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    rest.strip_prefix(host)?.strip_prefix('/')
}

/// `owner/repo/issues/123`, `owner/repo/pull/123` — tolerating the trailing
/// segments and `#`/`?` fragments a real browser URL carries
/// (`…/pull/12/files`, `…/issues/12#issuecomment-9`).
///
/// Issues and PRs share one number sequence per repo, so either kind resolves
/// to the same `owner/repo#N`. **Discussions do not** — they are numbered
/// independently, so `…/discussions/5` and `…/issues/5` are different records
/// and accepting the former would silently attach work to the latter.
fn github_from_url_path(path: &str) -> Option<TaskId> {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let kind = parts.next()?;
    if !matches!(kind, "issues" | "pull") {
        return None;
    }
    github_task_id(&format!("{owner}/{repo}"), parts.next()?)
}

/// `<org>/issue/ENG-45/some-slug`.
fn linear_from_url_path(path: &str) -> Option<TaskId> {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let mut parts = path.split('/');
    let _org = parts.next()?;
    if parts.next()? != "issue" {
        return None;
    }
    linear_identifier(parts.next()?)
}

/// Build a GitHub [`TaskId`], rejecting a slug that isn't exactly
/// `owner/repo` or a number that isn't one.
fn github_task_id(repo: &str, number: &str) -> Option<TaskId> {
    let repo = repo.trim().trim_end_matches(".git");
    let (owner, name) = repo.split_once('/')?;
    let well_formed = !owner.is_empty()
        && !name.is_empty()
        && !name.contains('/')
        && !repo.chars().any(char::is_whitespace);
    if !well_formed {
        return None;
    }
    let number: u64 = number.trim().parse().ok()?;
    Some(TaskId {
        source: crate::GITHUB_SOURCE.to_string(),
        key: format!("{owner}/{name}#{number}"),
    })
}

/// `ENG-45` → the Linear id the provider mints. The team prefix is
/// upper-cased: Linear's own `identifier` always is, so a lowercase `eng-45`
/// typed by an agent must still match the polled task.
fn linear_identifier(raw: &str) -> Option<TaskId> {
    let (team, number) = raw.split_once('-')?;
    let team_ok = !team.is_empty() && team.chars().all(|c| c.is_ascii_alphanumeric());
    if !team_ok || number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(TaskId {
        source: crate::LINEAR_SOURCE.to_string(),
        key: format!("{}-{number}", team.to_ascii_uppercase()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gh(key: &str) -> TaskId {
        TaskId {
            source: "github".into(),
            key: key.into(),
        }
    }

    #[test]
    fn parses_owner_repo_hash_number() {
        assert_eq!(
            parse_task_ref("acme/widget#42", None),
            Some(gh("acme/widget#42"))
        );
    }

    #[test]
    fn parses_a_github_issue_url() {
        assert_eq!(
            parse_task_ref("https://github.com/acme/widget/issues/42", None),
            Some(gh("acme/widget#42"))
        );
    }

    #[test]
    fn parses_a_github_pr_url_with_trailing_segments_and_fragment() {
        assert_eq!(
            parse_task_ref("https://github.com/acme/widget/pull/42/files", None),
            Some(gh("acme/widget#42"))
        );
        assert_eq!(
            parse_task_ref(
                "https://github.com/acme/widget/issues/42#issuecomment-9",
                None
            ),
            Some(gh("acme/widget#42"))
        );
    }

    #[test]
    fn a_bare_number_needs_a_default_repo() {
        assert_eq!(parse_task_ref("#42", None), None);
        assert_eq!(parse_task_ref("42", None), None);
        assert_eq!(
            parse_task_ref("#42", Some("acme/widget")),
            Some(gh("acme/widget#42"))
        );
        assert_eq!(
            parse_task_ref("42", Some("acme/widget")),
            Some(gh("acme/widget#42"))
        );
    }

    #[test]
    fn an_explicit_repo_beats_the_default() {
        assert_eq!(
            parse_task_ref("other/repo#7", Some("acme/widget")),
            Some(gh("other/repo#7"))
        );
    }

    #[test]
    fn parses_a_linear_identifier_and_url() {
        let eng = TaskId {
            source: "linear".into(),
            key: "ENG-45".into(),
        };
        assert_eq!(parse_task_ref("ENG-45", None), Some(eng.clone()));
        assert_eq!(parse_task_ref("eng-45", None), Some(eng.clone()));
        assert_eq!(
            parse_task_ref("https://linear.app/acme/issue/ENG-45/fix-the-thing", None),
            Some(eng)
        );
    }

    #[test]
    fn rejects_what_it_cannot_resolve() {
        for bad in [
            "",
            "   ",
            "just a workspace name",
            "acme/widget",
            "acme/widget#",
            "acme/widget#abc",
            "/widget#4",
            "acme/#4",
            "https://github.com/acme/widget/releases/4",
            // Discussions carry their own number sequence, so resolving one
            // to `acme/widget#4` would attach work to an unrelated issue.
            "https://github.com/acme/widget/discussions/4",
            "https://example.com/acme/widget/issues/4",
            "-45",
            "ENG-",
        ] {
            assert_eq!(parse_task_ref(bad, None), None, "{bad:?} must not resolve");
        }
    }

    #[test]
    fn tolerates_markdown_and_shell_decoration() {
        assert_eq!(
            parse_task_ref("`acme/widget#42`", None),
            Some(gh("acme/widget#42"))
        );
        assert_eq!(
            parse_task_ref("<https://github.com/acme/widget/issues/42>", None),
            Some(gh("acme/widget#42"))
        );
    }

    #[test]
    fn github_repo_of_reads_the_slug_back() {
        assert_eq!(github_repo_of(&gh("acme/widget#42")), Some("acme/widget"));
        assert_eq!(
            github_repo_of(&TaskId {
                source: "linear".into(),
                key: "ENG-45".into()
            }),
            None
        );
    }
}
