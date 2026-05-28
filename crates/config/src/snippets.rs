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
    /// `<pilot_home>/snippets.yaml`.
    Global,
    /// `<repo>/.pilot/snippets.yaml`.
    Repo,
}

impl SnippetOrigin {
    /// Short label for the picker — `"global"` / `"repo"` / `""`.
    pub fn label(self) -> &'static str {
        match self {
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
    /// Empty collection. Use this when neither file exists.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load from a specific file. Missing file → empty collection
    /// (no error — snippet files are optional). Origin is stamped
    /// on every entry so the picker can show provenance.
    pub fn load_from(path: &Path, origin: SnippetOrigin) -> Result<Self, SnippetsError> {
        if !path.exists() {
            return Ok(Self::empty());
        }
        let raw = std::fs::read_to_string(path)?;
        let file: SnippetsFile = serde_yaml::from_str(&raw)?;
        let mut by_key = BTreeMap::new();
        for (k, mut snippet) in file.snippets {
            snippet.origin = origin;
            by_key.insert(k, snippet);
        }
        Ok(Self { by_key })
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
                Self::empty()
            }
        }
    }

    /// Convenience: load repo-local file from `<repo_root>/.pilot/
    /// snippets.yaml`. Missing → empty.
    pub fn load_repo(repo_root: &Path) -> Self {
        match Self::load_from(&Self::default_repo_path(repo_root), SnippetOrigin::Repo) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to load repo snippets at {}: {e}", repo_root.display());
                Self::empty()
            }
        }
    }

    /// Merge two snippet sets. Entries in `overlay` win on key
    /// conflict. Used to stack repo-local on top of global so a
    /// project can override a shared shortcut without touching the
    /// user's library. Returns the merged set; neither input is
    /// preserved.
    pub fn merged(base: Self, overlay: Self) -> Self {
        let mut by_key = base.by_key;
        for (k, v) in overlay.by_key {
            by_key.insert(k, v);
        }
        Self { by_key }
    }

    /// Load both global + repo files and merge them (repo wins).
    /// This is the one-shot entry point most callers want.
    pub fn load_merged(repo_root: Option<&Path>) -> Self {
        let global = Self::load_global();
        let repo = repo_root.map(Self::load_repo).unwrap_or_default();
        Self::merged(global, repo)
    }

    /// Exact lookup by shortcut key.
    pub fn get(&self, key: &str) -> Option<&Snippet> {
        self.by_key.get(key)
    }

    /// All snippets, sorted by key for stable picker layout.
    pub fn all(&self) -> impl Iterator<Item = (&String, &Snippet)> {
        self.by_key.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// Filter for the picker. Empty query → every snippet (key
    /// order). Non-empty → entries whose KEY or DESCRIPTION contains
    /// `query`, case-insensitively. Stable sort: key-prefix matches
    /// first, then key-contains, then description-contains. Within
    /// each tier, alphabetical by key — the picker is for keyboard
    /// users and "type the prefix, hit Enter" should consistently
    /// land on the same item.
    pub fn filter(&self, query: &str) -> Vec<&Snippet> {
        let q = query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return self.by_key.values().collect();
        }
        let mut tiered: Vec<(u8, &String, &Snippet)> = self
            .by_key
            .iter()
            .filter_map(|(k, v)| {
                let kl = k.to_ascii_lowercase();
                let dl = v.description.to_ascii_lowercase();
                let tier = if kl.starts_with(&q) {
                    0
                } else if kl.contains(&q) {
                    1
                } else if dl.contains(&q) {
                    2
                } else {
                    return None;
                };
                Some((tier, k, v))
            })
            .collect();
        tiered.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
        tiered.into_iter().map(|(_, _, v)| v).collect()
    }

    /// Picker row layout — key + value pairs in the order the
    /// picker should render. Walks the merged map, so repo
    /// overrides are already in effect.
    pub fn entries(&self) -> Vec<(&String, &Snippet)> {
        self.by_key.iter().collect()
    }

    /// Snippet keys, sorted. Convenience for the model code that
    /// stashes "picker offered these snippets in this order" so
    /// `Msg::ChoicePicked(idx)` can resolve back to a key.
    pub fn keys(&self) -> Vec<String> {
        self.by_key.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Helper: write a snippets YAML into a `tempfile::NamedTempFile`-
    /// shaped tmp dir and return the path. We don't want the real
    /// crate `tempfile` here (avoid a new dep) — std::env::temp_dir
    /// plus a unique name is enough for these unit tests.
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
        // `rev` should be the repo version.
        let rev = merged.get("rev").unwrap();
        assert_eq!(rev.description, "Repo-specific review");
        assert_eq!(rev.body, "repo body");
        assert_eq!(rev.origin, SnippetOrigin::Repo);
        // The repo-only entry is present.
        assert_eq!(merged.get("deploy").unwrap().origin, SnippetOrigin::Repo);
        // The global-only entry is present.
        assert_eq!(merged.get("shared").unwrap().origin, SnippetOrigin::Global);
    }

    #[test]
    fn filter_empty_returns_all_in_key_order() {
        let path = write_tmp(
            "filter-all",
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
        let all = s.filter("");
        let keys: Vec<_> = all.iter().map(|v| v.description.as_str()).collect();
        // BTreeMap iteration is alphabetic by key: alpha, mid, zeta.
        assert_eq!(keys, vec!["first", "middle", "last"]);
    }

    /// Prefix matches on KEY come before contains-matches; contains
    /// on description comes last. Pins the tier order so the
    /// auto-submit "exact key match" path lands on the right row.
    #[test]
    fn filter_tiers_prefix_then_contains() {
        let path = write_tmp(
            "filter-tiers",
            r#"
snippets:
  rev:
    description: nothing
    body: ""
  preview:
    description: something with rev inside the description
    body: ""
  arrev:
    description: nothing
    body: ""
"#,
        );
        let s = Snippets::load_from(&path, SnippetOrigin::Global).unwrap();
        let out = s.filter("rev");
        // `rev` (key prefix) first, then `arrev` (key contains),
        // then `preview` (description contains). With three rows the
        // description of the third row pins the description-tier
        // landing in last place.
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[2].description,
            "something with rev inside the description"
        );
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
}
