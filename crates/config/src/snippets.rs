//! Snippet system: short keystroke shortcuts that expand into
//! pre-defined prompts and are auto-submitted to the active agent.
//!
//! Two files contribute, merged with the repo-local one winning on
//! key conflict:
//!
//! - **Global** — `<lazybox_home>/snippets.yaml` (defaults to
//!   `~/.lazybox/snippets.yaml`). Lives at the profile root so a
//!   schema bump in `v2/` doesn't orphan the user's library.
//! - **Repo-local** — `.lazybox/snippets.yaml` at the repository root.
//!   Checked into source control so a project can ship its own
//!   review prompts, deploy checks, etc.
//!
//! Both files have the same shape:
//!
//! ```yaml
//! snippets:
//!   rev:
//!     description: Review current diff
//!     body: |
//!       Please review the current diff for correctness bugs and
//!       obvious cleanups. Focus on the changes only, not the
//!       surrounding code.
//!   pr:
//!     description: Open a PR with summary + test plan
//!     body: |
//!       Please open a PR for the current branch. Use a concise
//!       title. Body should include a Summary section (1-3 bullets)
//!       and a Test plan section as a checklist.
//! ```
//!
//! At runtime the TUI loads both and feeds the merged set into the
//! snippet picker mounted by the terminal pane on `]<key>`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Single snippet definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snippet {
    /// Human-readable label shown in the picker. Short — one line.
    #[serde(default)]
    pub description: String,
    /// Optional grouping label ("Review", "Git & PR", …). Drives the
    /// category headers + colored tags in the picker. Defaults to
    /// empty; empty-category snippets fall under a trailing "Other"
    /// group. Free-form so user files can invent their own groups.
    #[serde(default)]
    pub category: String,
    /// Snippet body. Sent verbatim to the active agent's PTY,
    /// followed by a single carriage-return that triggers submit
    /// on every agent we ship (Claude Code, Codex, Cursor, shell).
    pub body: String,
    /// Provenance — which file the entry came from. Hand-set by the
    /// loader; serde ignores it on the way in / out. Used by the
    /// picker to show "global" vs "repo" hints alongside each row,
    /// and by the merge step to render conflict diagnostics.
    #[serde(skip)]
    pub origin: SnippetOrigin,
}

/// Where a snippet was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnippetOrigin {
    #[default]
    Unknown,
    /// Shipped with lazybox, merged beneath the user's files.
    BuiltIn,
    /// `<lazybox_home>/snippets.yaml`.
    Global,
    /// `<repo>/.lazybox/snippets.yaml`.
    Repo,
}

impl SnippetOrigin {
    /// Short label for the picker — `"built-in"` / `"global"` /
    /// `"repo"` / `""`.
    pub fn label(self) -> &'static str {
        match self {
            SnippetOrigin::BuiltIn => "built-in",
            SnippetOrigin::Global => "global",
            SnippetOrigin::Repo => "repo",
            SnippetOrigin::Unknown => "",
        }
    }
}

/// Top-level wire shape of a snippets file. The outer key in
/// `snippets:` is the shortcut key (`rev`, `pr`, …); the inner
/// fields populate [`Snippet`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct SnippetsFile {
    snippets: BTreeMap<String, Snippet>,
}

/// Loaded + merged snippet collection.
#[derive(Debug, Clone, Default)]
pub struct Snippets {
    /// Snippets keyed by shortcut. `BTreeMap` for deterministic
    /// iteration in the picker.
    by_key: BTreeMap<String, Snippet>,
}

#[derive(Debug, thiserror::Error)]
pub enum SnippetsError {
    #[error("failed to read snippets: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse snippets: {0}")]
    Parse(#[from] serde_yaml::Error),
}

impl Snippets {
    /// Load from a specific file. Missing file → empty collection
    /// (no error — snippet files are optional). Origin is stamped
    /// on every entry so the picker can show provenance.
    ///
    /// We match on `ErrorKind::NotFound` from the read itself
    /// rather than gating on a prior `path.exists()` check — that
    /// would race a file removal between the two calls and surface
    /// as a misleading `Io` error.
    pub fn load_from(path: &Path, origin: SnippetOrigin) -> Result<Self, SnippetsError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => return Err(e.into()),
        };
        let file: SnippetsFile = serde_yaml::from_str(&raw)?;
        let by_key = file
            .snippets
            .into_iter()
            .map(|(k, mut s)| {
                s.origin = origin;
                (k, s)
            })
            .collect();
        Ok(Self { by_key })
    }

    /// Snippets shipped with lazybox. Merged *beneath* the user's
    /// global + repo files (see [`Snippets::load_merged`]), so any
    /// user entry with the same key transparently overrides one of
    /// these — they're a starting library, not a locked-in set.
    pub fn builtin() -> Self {
        let entry = |category: &str, description: &str, body: &str| Snippet {
            description: description.to_string(),
            category: category.to_string(),
            body: body.to_string(),
            origin: SnippetOrigin::BuiltIn,
        };
        let by_key = BTreeMap::from([
            // ── Review ──────────────────────────────────────────────
            (
                "rev".to_string(),
                entry(
                    "Review",
                    "Review the current diff",
                    "Please review the current diff for correctness bugs and obvious \
                     cleanups. Focus on the changes only, not the surrounding code.",
                ),
            ),
            (
                "revdeep".to_string(),
                entry(
                    "Review",
                    "Deep review: design, edge cases, failure modes",
                    "Review the current diff deeply. Beyond surface bugs, evaluate the \
                     design: are the abstractions right, what edge cases and error paths \
                     are unhandled, and how could this break under concurrency, partial \
                     failure, or unexpected input? List concrete findings ranked by \
                     severity.",
                ),
            ),
            (
                "nit".to_string(),
                entry(
                    "Review",
                    "Nitpick pass: naming, comments, style",
                    "Do a nitpick pass over the current diff: naming, comment quality, \
                     dead code, inconsistent style, and anything that would slow a \
                     reviewer down. Keep suggestions small and mechanical.",
                ),
            ),
            (
                "selfrev".to_string(),
                entry(
                    "Review",
                    "Self-review before pushing",
                    "Before I push, self-review this branch as if you were a skeptical \
                     reviewer seeing it for the first time. Call out anything that isn't \
                     obviously correct, any missing tests, and anything I'd be asked to \
                     change in review.",
                ),
            ),
            // ── Git & PR ────────────────────────────────────────────
            (
                "pr".to_string(),
                entry(
                    "Git & PR",
                    "Open a PR (summary + test plan)",
                    "Please open a PR for the current branch. Use a concise title. The \
                     body should include a Summary section (1-3 bullets) and a Test plan \
                     section as a checklist.",
                ),
            ),
            (
                "ready".to_string(),
                entry(
                    "Git & PR",
                    "Mark the PR ready for review",
                    "Mark the current pull request as ready for review by running \
                     `gh pr ready`.",
                ),
            ),
            (
                "commit".to_string(),
                entry(
                    "Git & PR",
                    "Commit staged changes with a good message",
                    "Commit the staged changes. Write a concise, imperative commit \
                     subject (<=50 chars) and, if the change isn't trivial, a body \
                     explaining the why. Don't commit unrelated changes.",
                ),
            ),
            (
                "amend".to_string(),
                entry(
                    "Git & PR",
                    "Amend the last commit",
                    "Fold the current working changes into the previous commit with \
                     `git commit --amend`, keeping the existing message unless it no \
                     longer fits.",
                ),
            ),
            (
                "rebase".to_string(),
                entry(
                    "Git & PR",
                    "Rebase onto the base branch",
                    "Rebase this branch onto the latest base branch. Fetch first, replay \
                     cleanly, and resolve any conflicts conservatively — preserve intent \
                     on both sides and flag anything ambiguous.",
                ),
            ),
            (
                "squash".to_string(),
                entry(
                    "Git & PR",
                    "Squash the branch into clean commits",
                    "Squash this branch's commits into a small number of logically \
                     coherent commits with clear messages. Don't collapse genuinely \
                     independent changes into one.",
                ),
            ),
            (
                "conflicts".to_string(),
                entry(
                    "Git & PR",
                    "Resolve merge conflicts",
                    "Resolve the current merge conflicts. For each hunk, explain which \
                     side you kept and why, and make sure the merged result actually \
                     compiles and passes tests rather than just resolving markers.",
                ),
            ),
            // ── Testing ─────────────────────────────────────────────
            (
                "test".to_string(),
                entry(
                    "Testing",
                    "Run the test suite and fix failures",
                    "Run the project's test suite. If anything fails, fix the root cause \
                     (not the test) and re-run until green. Report what failed and why.",
                ),
            ),
            (
                "tdd".to_string(),
                entry(
                    "Testing",
                    "TDD: failing test first, then implement",
                    "Work test-first: write a failing test that captures the desired \
                     behavior, confirm it fails for the right reason, then implement the \
                     minimal change to make it pass. Refactor once green.",
                ),
            ),
            (
                "cover".to_string(),
                entry(
                    "Testing",
                    "Add tests for uncovered branches",
                    "Identify the important untested branches in the code I just touched \
                     and add focused tests for them — error paths and edge cases first, \
                     not just the happy path.",
                ),
            ),
            (
                "repro".to_string(),
                entry(
                    "Testing",
                    "Write a failing test that reproduces the bug",
                    "Write a minimal automated test that reproduces the bug I'm about to \
                     describe. It should fail on the current code for the same reason the \
                     bug occurs, so it becomes the regression guard once fixed.",
                ),
            ),
            // ── Debugging ───────────────────────────────────────────
            (
                "bug".to_string(),
                entry(
                    "Debugging",
                    "Root-cause the failure",
                    "Investigate the failure I'm about to describe. Find the root cause \
                     before proposing a fix — trace the actual code path, don't guess. \
                     Explain the mechanism, then fix it and add a regression test.",
                ),
            ),
            (
                "bisect".to_string(),
                entry(
                    "Debugging",
                    "git bisect to find the offending commit",
                    "Use `git bisect` to find the commit that introduced the regression. \
                     Identify a known-good and known-bad revision, script the check if \
                     possible, and report the first bad commit with an explanation.",
                ),
            ),
            (
                "trace".to_string(),
                entry(
                    "Debugging",
                    "Add logging to narrow it down",
                    "Add targeted logging/tracing around the suspect code path to narrow \
                     down where behavior diverges from expectation. Keep the \
                     instrumentation removable and report what it reveals.",
                ),
            ),
            (
                "explain".to_string(),
                entry(
                    "Debugging",
                    "Explain this error / stack trace",
                    "Explain the error or stack trace I'm about to paste: what it means, \
                     the most likely cause given this codebase, and the concrete next \
                     step to confirm and fix it.",
                ),
            ),
            // ── Refactor ────────────────────────────────────────────
            (
                "refac".to_string(),
                entry(
                    "Refactor",
                    "Refactor for clarity, no behavior change",
                    "Refactor the code I point you at for clarity and simplicity without \
                     changing behavior. Keep the diff reviewable, and make sure the tests \
                     still pass to prove behavior is unchanged.",
                ),
            ),
            (
                "rename".to_string(),
                entry(
                    "Refactor",
                    "Rename a symbol across the repo",
                    "Rename the symbol I specify consistently across the whole repo, \
                     including docs, tests, and comments. Verify nothing else \
                     accidentally matched and that it still builds.",
                ),
            ),
            (
                "extract".to_string(),
                entry(
                    "Refactor",
                    "Extract a function / module",
                    "Extract the highlighted logic into a well-named function (or module) \
                     with a clear signature. Update all call sites and keep behavior \
                     identical.",
                ),
            ),
            (
                "dedupe".to_string(),
                entry(
                    "Refactor",
                    "Remove duplication",
                    "Find the near-duplicate logic in the code I point you at and unify \
                     it behind a single implementation — but only where the duplication \
                     is genuinely the same concept, not coincidentally similar.",
                ),
            ),
            // ── Performance ─────────────────────────────────────────
            (
                "perf".to_string(),
                entry(
                    "Performance",
                    "Profile and optimize the hot path",
                    "Profile the hot path I describe, find where the time actually goes, \
                     and optimize the biggest wins first. Confirm the improvement with a \
                     measurement, and don't sacrifice correctness or clarity for micro-\
                     gains.",
                ),
            ),
            (
                "bench".to_string(),
                entry(
                    "Performance",
                    "Add a benchmark",
                    "Add a benchmark that captures the performance characteristic we care \
                     about here, so future changes can be measured against it. Report the \
                     current baseline numbers.",
                ),
            ),
            (
                "alloc".to_string(),
                entry(
                    "Performance",
                    "Reduce allocations in the hot path",
                    "Look for avoidable allocations and copies in the hot path and remove \
                     them (reuse buffers, borrow instead of clone, avoid intermediate \
                     collections) — only where it measurably helps and stays readable.",
                ),
            ),
            // ── Security ────────────────────────────────────────────
            (
                "sec".to_string(),
                entry(
                    "Security",
                    "Security review of the diff",
                    "Review the current diff for security issues: injection, missing \
                     authz checks, unsafe deserialization, path traversal, secret \
                     handling, and unchecked input at trust boundaries. Report concrete \
                     findings.",
                ),
            ),
            (
                "deps".to_string(),
                entry(
                    "Security",
                    "Audit and update dependencies",
                    "Audit the project's dependencies for known vulnerabilities and \
                     unmaintained packages. Propose safe upgrades, call out breaking \
                     changes, and don't bump anything without checking the changelog.",
                ),
            ),
            (
                "secrets".to_string(),
                entry(
                    "Security",
                    "Scan for leaked secrets",
                    "Scan the diff (and recent history if relevant) for accidentally \
                     committed secrets — API keys, tokens, private keys, passwords. If \
                     any are found, flag them clearly and advise on rotation.",
                ),
            ),
            // ── Docs ────────────────────────────────────────────────
            (
                "doc".to_string(),
                entry(
                    "Docs",
                    "Document public APIs",
                    "Add or improve documentation for the public APIs I touched: what \
                     each does, its parameters and return, invariants, and a short usage \
                     example where it helps. Match the surrounding doc style.",
                ),
            ),
            (
                "readme".to_string(),
                entry(
                    "Docs",
                    "Update the README",
                    "Update the README to reflect the change I just made — usage, flags, \
                     examples, and anything now stale. Keep it accurate and concise.",
                ),
            ),
            (
                "adr".to_string(),
                entry(
                    "Docs",
                    "Write an ADR for this decision",
                    "Write a short Architecture Decision Record for the decision we just \
                     made: context, the options considered, the decision, and its \
                     consequences. Follow any existing ADR format in the repo.",
                ),
            ),
            // ── Chores ──────────────────────────────────────────────
            (
                "lint".to_string(),
                entry(
                    "Chores",
                    "Fix lint and formatting",
                    "Run the project's linter and formatter, then fix every warning and \
                     formatting issue in the code I touched. Don't suppress warnings — \
                     address them.",
                ),
            ),
            (
                "ci".to_string(),
                entry(
                    "Chores",
                    "Diagnose and fix failing CI",
                    "The CI is failing. Pull the failing job's logs, find the real cause, \
                     and fix it locally. Re-run the equivalent checks here to confirm \
                     before pushing.",
                ),
            ),
            (
                "clean".to_string(),
                entry(
                    "Chores",
                    "Remove dead code and unused deps",
                    "Find and remove dead code, unused imports, and unused dependencies \
                     in the area I point you at. Verify nothing that looked unused is \
                     actually referenced (reflection, macros, feature gates) before \
                     deleting.",
                ),
            ),
        ]);
        Self { by_key }
    }

    /// Commented starter file written by the "Edit snippets" settings
    /// action when no global file exists yet. Mirrors the built-in
    /// `rev` entry so a user has a working example to copy.
    pub fn starter_template() -> &'static str {
        "# Snippets — keystroke shortcuts that expand into a prompt and\n\
         # auto-submit to the focused agent. Trigger with `]]<key>` in a\n\
         # session terminal. See docs/snippets.md for the full reference.\n\
         #\n\
         # lazybox ships a broad, categorized built-in library (rev, pr,\n\
         # test, refac, …); anything you define here with the same key\n\
         # overrides the built-in one. `category:` is optional.\n\
         snippets:\n\
         \x20 rev:\n\
         \x20   description: Review current diff\n\
         \x20   category: Review\n\
         \x20   body: |\n\
         \x20     Please review the current diff for correctness bugs and\n\
         \x20     obvious cleanups. Focus on the changes only.\n"
    }

    /// Default global path: `<lazybox_home>/snippets.yaml`.
    pub fn default_global_path() -> PathBuf {
        lazybox_core::paths::home().join("snippets.yaml")
    }

    /// Default repo-local path: `<repo_root>/.lazybox/snippets.yaml`.
    pub fn default_repo_path(repo_root: &Path) -> PathBuf {
        repo_root.join(".lazybox").join("snippets.yaml")
    }

    /// Convenience: load global file from the standard path. Missing
    /// → empty, never errors on absence.
    pub fn load_global() -> Self {
        match Self::load_from(&Self::default_global_path(), SnippetOrigin::Global) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to load global snippets: {e}");
                Self::default()
            }
        }
    }

    /// Convenience: load repo-local file from `<repo_root>/.lazybox/
    /// snippets.yaml`. Missing → empty.
    pub fn load_repo(repo_root: &Path) -> Self {
        match Self::load_from(&Self::default_repo_path(repo_root), SnippetOrigin::Repo) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "failed to load repo snippets at {}: {e}",
                    repo_root.display()
                );
                Self::default()
            }
        }
    }

    /// Merge two snippet sets. Entries in `overlay` win on key
    /// conflict. Used to stack repo-local on top of global so a
    /// project can override a shared shortcut without touching the
    /// user's library.
    pub fn merged(base: Self, overlay: Self) -> Self {
        let mut by_key = base.by_key;
        by_key.extend(overlay.by_key);
        Self { by_key }
    }

    /// Load both global + repo files and merge them over the
    /// built-in set. Precedence, lowest to highest: built-in →
    /// global → repo. This is the one-shot entry point most callers
    /// want.
    pub fn load_merged(repo_root: Option<&Path>) -> Self {
        let global = Self::load_global();
        let repo = repo_root.map(Self::load_repo).unwrap_or_default();
        Self::merged(Self::merged(Self::builtin(), global), repo)
    }

    /// Exact lookup by shortcut key.
    pub fn get(&self, key: &str) -> Option<&Snippet> {
        self.by_key.get(key)
    }

    /// All snippets, in key order. Lazy — no allocation. Caller
    /// decides whether to collect or stream.
    pub fn all(&self) -> impl Iterator<Item = (&str, &Snippet)> {
        self.by_key.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Helper: write a snippets YAML into a unique tmp dir and
    /// return the path. We don't pull in the `tempfile` crate just
    /// for tests — `std::env::temp_dir` + a unique name is enough.
    fn write_tmp(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lazybox-snippets-test-{}-{}",
            std::process::id(),
            name,
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snippets.yaml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn missing_file_yields_empty_collection() {
        let s = Snippets::load_from(
            &PathBuf::from("/nonexistent/snippets-does-not-exist.yaml"),
            SnippetOrigin::Global,
        )
        .expect("missing file is not an error");
        assert!(s.is_empty());
    }

    #[test]
    fn loads_basic_snippets() {
        let path = write_tmp(
            "basic",
            r#"
snippets:
  rev:
    description: Review current diff
    body: |
      Please review the current diff.
  pr:
    description: Open a PR
    body: Please open a PR for the current branch.
"#,
        );
        let s = Snippets::load_from(&path, SnippetOrigin::Global).unwrap();
        assert_eq!(s.len(), 2);
        let rev = s.get("rev").unwrap();
        assert_eq!(rev.description, "Review current diff");
        assert!(rev.body.contains("Please review"));
        assert_eq!(rev.origin, SnippetOrigin::Global);
    }

    /// Repo-local entries override globals on key conflict — this
    /// is the headline acceptance criterion.
    #[test]
    fn repo_overrides_global_on_key_conflict() {
        let global_path = write_tmp(
            "conflict-global",
            r#"
snippets:
  rev:
    description: Global review
    body: global body
  shared:
    description: Only in global
    body: global only
"#,
        );
        let repo_path = write_tmp(
            "conflict-repo",
            r#"
snippets:
  rev:
    description: Repo-specific review
    body: repo body
  deploy:
    description: Only in repo
    body: repo only
"#,
        );
        let global = Snippets::load_from(&global_path, SnippetOrigin::Global).unwrap();
        let repo = Snippets::load_from(&repo_path, SnippetOrigin::Repo).unwrap();
        let merged = Snippets::merged(global, repo);
        assert_eq!(merged.len(), 3);
        let rev = merged.get("rev").unwrap();
        assert_eq!(rev.description, "Repo-specific review");
        assert_eq!(rev.body, "repo body");
        assert_eq!(rev.origin, SnippetOrigin::Repo);
        assert_eq!(merged.get("deploy").unwrap().origin, SnippetOrigin::Repo);
        assert_eq!(merged.get("shared").unwrap().origin, SnippetOrigin::Global);
    }

    #[test]
    fn all_yields_entries_in_key_order() {
        let path = write_tmp(
            "all-order",
            r#"
snippets:
  zeta:
    description: last
    body: z
  alpha:
    description: first
    body: a
  mid:
    description: middle
    body: m
"#,
        );
        let s = Snippets::load_from(&path, SnippetOrigin::Global).unwrap();
        let keys: Vec<&str> = s.all().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["alpha", "mid", "zeta"]);
    }

    /// Lookup by exact key — the auto-submit path uses this.
    #[test]
    fn get_returns_exact_match() {
        let path = write_tmp(
            "get",
            r#"
snippets:
  rev:
    description: Review
    body: review body
"#,
        );
        let s = Snippets::load_from(&path, SnippetOrigin::Global).unwrap();
        assert!(s.get("rev").is_some());
        assert!(s.get("re").is_none());
        assert!(s.get("REV").is_none(), "keys are case-sensitive");
    }

    /// `category` parses from YAML and round-trips; a snippet that
    /// omits it defaults to the empty string (backwards compatible).
    #[test]
    fn category_parses_and_defaults_to_empty() {
        let path = write_tmp(
            "category",
            r#"
snippets:
  rev:
    description: Review
    category: Review
    body: review body
  bare:
    body: no category here
"#,
        );
        let s = Snippets::load_from(&path, SnippetOrigin::Global).unwrap();
        assert_eq!(s.get("rev").unwrap().category, "Review");
        assert_eq!(s.get("bare").unwrap().category, "");
    }

    /// The built-in library is large and every entry is categorized —
    /// the picker relies on that to group.
    #[test]
    fn builtin_is_large_and_categorized() {
        let b = Snippets::builtin();
        assert!(
            b.len() >= 30,
            "built-in library should be broad: {}",
            b.len()
        );
        assert!(
            b.all().all(|(_, s)| !s.category.is_empty()),
            "every built-in snippet carries a category",
        );
    }

    #[test]
    fn missing_description_defaults_to_empty_string() {
        let path = write_tmp(
            "no-desc",
            r#"
snippets:
  bare:
    body: just a body
"#,
        );
        let s = Snippets::load_from(&path, SnippetOrigin::Global).unwrap();
        let entry = s.get("bare").unwrap();
        assert_eq!(entry.description, "");
        assert_eq!(entry.body, "just a body");
    }

    /// Empty `snippets:` block parses to zero entries instead of
    /// erroring. Mirrors how every other section in `Config`
    /// behaves when omitted.
    #[test]
    fn empty_snippets_block_parses() {
        let path = write_tmp(
            "empty-block",
            r#"
snippets: {}
"#,
        );
        let s = Snippets::load_from(&path, SnippetOrigin::Global).unwrap();
        assert!(s.is_empty());
    }

    /// The shipped built-in set is non-empty and carries the
    /// `ready` shortcut (the headline default).
    #[test]
    fn builtin_includes_ready_snippet() {
        let b = Snippets::builtin();
        assert!(!b.is_empty());
        let ready = b.get("ready").expect("ready snippet ships built-in");
        assert_eq!(ready.origin, SnippetOrigin::BuiltIn);
        assert!(ready.body.contains("gh pr ready"));
        assert!(b.get("rev").is_some());
        assert!(b.get("pr").is_some());
    }

    /// A global entry with a built-in key wins; built-ins fill the
    /// gaps. Mirrors how `load_merged` layers built-in < global.
    #[test]
    fn user_entry_overrides_builtin_on_key_conflict() {
        let user = Snippets::load_from(
            &write_tmp(
                "override-builtin",
                r#"
snippets:
  rev:
    description: My review
    body: my custom review body
"#,
            ),
            SnippetOrigin::Global,
        )
        .unwrap();
        let merged = Snippets::merged(Snippets::builtin(), user);
        let rev = merged.get("rev").unwrap();
        assert_eq!(rev.body, "my custom review body");
        assert_eq!(rev.origin, SnippetOrigin::Global);
        // Untouched built-ins remain.
        assert_eq!(merged.get("ready").unwrap().origin, SnippetOrigin::BuiltIn);
    }

    /// The starter template parses as a valid snippets file.
    #[test]
    fn starter_template_is_valid_yaml() {
        let file: SnippetsFile =
            serde_yaml::from_str(Snippets::starter_template()).expect("template parses");
        assert!(file.snippets.contains_key("rev"));
    }

    /// Malformed YAML surfaces as `SnippetsError::Parse`, not a
    /// silent empty load — users will want to know they typo'd.
    #[test]
    fn malformed_yaml_surfaces_parse_error() {
        let path = write_tmp(
            "bad-yaml",
            r#"
snippets:
  rev:
    body: [this is not a valid mapping value
"#,
        );
        let err = Snippets::load_from(&path, SnippetOrigin::Global)
            .expect_err("malformed YAML should error");
        assert!(matches!(err, SnippetsError::Parse(_)));
    }
}
