//! Snippet system: short keystroke shortcuts that expand into
//! pre-defined prompts and are auto-submitted to the active agent.
//!
//! Two files contribute, merged with the repo-local one winning on
//! key conflict:
//!
//! - **Global** — `<pilot_home>/snippets.yaml` (defaults to
//!   `~/.pilot/snippets.yaml`). Lives at the profile root so a
//!   schema bump in `v2/` doesn't orphan the user's library.
//! - **Repo-local** — `.pilot/snippets.yaml` at the repository root.
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
    /// Shipped with pilot, merged beneath the user's files.
    BuiltIn,
    /// `<pilot_home>/snippets.yaml`.
    Global,
    /// `<repo>/.pilot/snippets.yaml`.
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

    /// Snippets shipped with pilot. Merged *beneath* the user's
    /// global + repo files (see [`Snippets::load_merged`]), so any
    /// user entry with the same key transparently overrides one of
    /// these — they're a starting library, not a locked-in set.
    pub fn builtin() -> Self {
        let entry = |description: &str, body: &str| Snippet {
            description: description.to_string(),
            body: body.to_string(),
            origin: SnippetOrigin::BuiltIn,
        };
        let by_key = BTreeMap::from([
            (
                "rev".to_string(),
                entry(
                    "Review the current diff",
                    "Please review the current diff for correctness bugs and obvious \
                     cleanups. Focus on the changes only, not the surrounding code.",
                ),
            ),
            (
                "pr".to_string(),
                entry(
                    "Open a PR (summary + test plan)",
                    "Please open a PR for the current branch. Use a concise title. The \
                     body should include a Summary section (1-3 bullets) and a Test plan \
                     section as a checklist.",
                ),
            ),
            (
                "ready".to_string(),
                entry(
                    "Mark the PR ready for review",
                    "Mark the current pull request as ready for review by running \
                     `gh pr ready`.",
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
         # pilot ships built-in `rev`, `pr`, and `ready` snippets; anything\n\
         # you define here with the same key overrides the built-in one.\n\
         snippets:\n\
         \x20 rev:\n\
         \x20   description: Review current diff\n\
         \x20   body: |\n\
         \x20     Please review the current diff for correctness bugs and\n\
         \x20     obvious cleanups. Focus on the changes only.\n"
    }

    /// Default global path: `<pilot_home>/snippets.yaml`.
    pub fn default_global_path() -> PathBuf {
        pilot_core::paths::home().join("snippets.yaml")
    }

    /// Default repo-local path: `<repo_root>/.pilot/snippets.yaml`.
    pub fn default_repo_path(repo_root: &Path) -> PathBuf {
        repo_root.join(".pilot").join("snippets.yaml")
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

    /// Convenience: load repo-local file from `<repo_root>/.pilot/
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
            "pilot-snippets-test-{}-{}",
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
