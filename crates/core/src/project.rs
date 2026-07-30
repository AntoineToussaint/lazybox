//! Project — a top-level container holding workspaces. Models the
//! same shape a GitHub repo or a Linear team has: a name, an optional
//! root directory on disk, and a stable key. Workspaces (PRs, issues,
//! ad-hoc work) live under a Project.
//!
//! Today's lazybox inferred "projects" from `task.repo` strings at
//! render time, which made it impossible to:
//!   1. Show an empty repo header (no workspaces yet).
//!   2. Have a local non-provider project (the old "sandbox" was the
//!      single hardcoded escape hatch).
//!   3. Move a workspace between projects.
//!
//! Promoting Project to a first-class entity removes those three
//! constraints. Provider polling registers a Project per polled
//! repo/team; the user creates local Projects via `x p`; the
//! sidebar headers read from the Project table instead of grouping
//! workspaces by their inferred repo string.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Stable identifier for a Project. Human-readable to survive
/// renames and show well in logs. The prefix encodes the source:
///
/// - `github-<owner>-<repo>` — Github repo project, auto-registered
///   by the polling loop when workspaces from that repo appear.
/// - `linear-<team-id>` — Linear team project.
/// - `local-<slug>` — Local-only project created via the TUI.
///
/// The prefix is purely advisory — the daemon routes by it in the
/// same way it routes workspace keys in `build_provider_for_workspace`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct ProjectKey(pub String);

impl ProjectKey {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Build a Project key for a GitHub repo. Slug-safe: `owner/repo`
    /// → `github-owner-repo`. Same shape provider polling uses to
    /// route mutations, so keys round-trip cleanly between the two
    /// surfaces.
    pub fn github(owner: &str, repo: &str) -> Self {
        Self(format!("github-{owner}-{repo}"))
    }

    /// Build a Project key for a Linear team.
    pub fn linear(team_id: &str) -> Self {
        Self(format!("linear-{team_id}"))
    }

    /// Build a Project key for a local-only project. `slug` should be
    /// pre-sluggified (e.g. via `lazybox_core::slug::slugify`).
    pub fn local(slug: &str) -> Self {
        Self(format!("local-{slug}"))
    }

    /// `"github"` / `"linear"` / `"local"` / `""` if no recognised
    /// prefix. Matches the routing convention used elsewhere.
    pub fn source_prefix(&self) -> &str {
        self.0.split_once('-').map(|(p, _)| p).unwrap_or("")
    }

    /// Recover `owner/repo` only when the flat GitHub key has exactly
    /// one possible boundary. Keys containing another `-` need an
    /// upstream task, project record, or subscribed scope to recover
    /// the slug without guessing.
    pub fn unambiguous_github_slug(&self) -> Option<String> {
        let rest = self.0.strip_prefix("github-")?;
        let mut parts = rest.split('-');
        let owner = parts.next()?;
        let repo = parts.next()?;
        (!owner.is_empty() && !repo.is_empty() && parts.next().is_none())
            .then(|| format!("{owner}/{repo}"))
    }

    /// Whether an exact `owner/repo` slug is a valid identity for this
    /// GitHub project key.
    pub fn matches_github_slug(&self, slug: &str) -> bool {
        let Some((owner, repo)) = slug.split_once('/') else {
            return false;
        };
        !owner.is_empty()
            && !repo.is_empty()
            && !repo.contains('/')
            && Self::github(owner, repo) == *self
    }

    /// Best-effort human-readable name derived from the key alone, for
    /// add paths and renders that lack an upstream display name.
    /// `github-<owner>-<repo>` → `<owner>/<repo>`; other prefixes
    /// return the suffix after the source prefix; an unprefixed key
    /// returns itself. The `owner`/`repo` split takes the first `-`,
    /// so a repo with hyphens (`pretty-hackernews`) round-trips; an
    /// owner with hyphens (rare) does not — acceptable for a fallback.
    pub fn display_name(&self) -> String {
        match self.0.split_once('-') {
            Some(("github", rest)) => match rest.split_once('-') {
                Some((owner, repo)) => format!("{owner}/{repo}"),
                None => rest.to_string(),
            },
            Some((_, rest)) => rest.to_string(),
            None => self.0.clone(),
        }
    }

    /// Recover this GitHub project's exact `owner/repo` slug from the
    /// user's subscribed scope ids, which carry the unambiguous
    /// `github:owner/repo` form.
    ///
    /// The flat `github-{owner}-{repo}` key is lossy: once the owner
    /// itself contains a hyphen (`codefly-dev/warden-platform` →
    /// `github-codefly-dev-warden-platform`) there's no way to split it
    /// back — [`display_name`](Self::display_name) guesses the first `-`
    /// and yields `codefly/dev-warden-platform`, which then clones the
    /// wrong repo. Matching against the scope that reproduces this exact
    /// key sidesteps the guess: the scope slug still holds the
    /// owner/repo boundary. Returns `None` for non-GitHub keys, or when
    /// no subscribed scope reproduces this key (e.g. an org-level
    /// subscription with no per-repo entry).
    pub fn github_slug_from_scopes<'a>(
        &self,
        scopes: impl IntoIterator<Item = &'a str>,
    ) -> Option<String> {
        if self.source_prefix() != "github" {
            return None;
        }
        scopes.into_iter().find_map(|scope| {
            let slug = scope.strip_prefix("github:")?;
            self.matches_github_slug(slug).then(|| slug.to_string())
        })
    }
}

impl std::fmt::Display for ProjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Parse the `owner/repo` slug out of a GitHub `origin` remote URL —
/// the string an on-disk checkout carries in `remote.origin.url`.
/// Recognizes the SCP-style SSH form (`git@github.com:owner/repo.git`),
/// the `ssh://` form, and the `https://` form, with a trailing `.git`
/// and any trailing slash stripped. Returns `None` for a non-GitHub
/// host or a URL with no `owner/repo` tail, so a checkout whose origin
/// points elsewhere (GitLab, a fork mirror) is simply not mapped rather
/// than mis-mapped.
pub fn github_owner_repo_from_url(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    // Reduce every remote form to the `owner/repo…` path after the host.
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("git://github.com/"))?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    // `repo` may itself contain further path segments on a malformed
    // URL; keep only the first segment so `owner/repo/extra` still
    // yields `owner/repo` rather than a bogus name.
    let repo = repo.split('/').next().unwrap_or(repo);
    (!owner.is_empty() && !repo.is_empty()).then(|| (owner.to_string(), repo.to_string()))
}

/// A top-level Project — a container that holds Workspaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct Project {
    pub key: ProjectKey,
    /// Display name (e.g. `"owner/repo"` for github,
    /// `"Frontend"` for a Linear team, the user-chosen string for
    /// local projects).
    pub name: String,
    /// Exact GitHub `owner/repo` identity. Kept separately from `name`
    /// because the display name is mutable presentation data and the
    /// legacy flat project key cannot preserve a hyphenated boundary.
    #[serde(default)]
    github_repo: Option<String>,
    /// Where worktrees for workspaces under this project root. `None`
    /// means "use the default `~/.lazybox/v2/<workspace_key>/`". Local
    /// projects can override this to point at an existing repo on
    /// disk; provider projects leave it `None` and let
    /// `WorktreeManager` allocate.
    pub root_dir: Option<PathBuf>,
    pub created_at: DateTime<Utc>,
}

impl Project {
    /// Build a Project ready to be saved.
    pub fn new(key: ProjectKey, name: impl Into<String>, now: DateTime<Utc>) -> Self {
        Self {
            key,
            name: name.into(),
            github_repo: None,
            root_dir: None,
            created_at: now,
        }
    }

    /// Build a GitHub-backed project with an explicit, round-trippable
    /// repository identity.
    pub fn github(owner: &str, repo: &str, now: DateTime<Utc>) -> Self {
        let slug = format!("{owner}/{repo}");
        Self {
            key: ProjectKey::github(owner, repo),
            name: slug.clone(),
            github_repo: Some(slug),
            root_dir: None,
            created_at: now,
        }
    }

    /// Return the canonical GitHub repo only when it is well-formed and
    /// belongs to this project's key. Persisted JSON is a system boundary,
    /// so malformed or mismatched identity data is rejected here.
    pub fn github_repo(&self) -> Option<&str> {
        let slug = self.github_repo.as_deref()?;
        self.key.matches_github_slug(slug).then_some(slug)
    }

    /// Attach a trusted GitHub slug to a legacy project record. Returns
    /// `false` when the slug is malformed or does not map to this key.
    pub fn set_github_repo(&mut self, slug: &str) -> bool {
        if !self.key.matches_github_slug(slug) {
            return false;
        }
        self.name = slug.to_string();
        self.github_repo = Some(slug.to_string());
        true
    }

    /// Display name for the sidebar, repairing the legacy raw-key
    /// fallback. Some records stored the raw key as their `name` (a
    /// self/local add that couldn't derive `owner/repo`); render the
    /// key-derived name instead so the sidebar never shows
    /// `github-owner-repo`.
    pub fn display_name(&self) -> String {
        if let Some(repo) = self.github_repo() {
            return repo.to_string();
        }
        if self.name == self.key.as_str() || self.name.is_empty() {
            self.key
                .unambiguous_github_slug()
                .unwrap_or_else(|| match self.key.source_prefix() {
                    "github" => self.key.as_str().to_string(),
                    _ => self.key.display_name(),
                })
        } else {
            self.name.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_key_round_trips_prefix() {
        let k = ProjectKey::github("acme", "widget");
        assert_eq!(k.as_str(), "github-acme-widget");
        assert_eq!(k.source_prefix(), "github");
    }

    #[test]
    fn local_key_carries_local_prefix() {
        let k = ProjectKey::local("my-experiment");
        assert_eq!(k.as_str(), "local-my-experiment");
        assert_eq!(k.source_prefix(), "local");
    }

    #[test]
    fn github_key_display_name_is_owner_slash_repo() {
        assert_eq!(
            ProjectKey::github("AntoineToussaint", "lazybox").display_name(),
            "AntoineToussaint/lazybox"
        );
        // Repo names with hyphens round-trip (split takes the first `-`).
        assert_eq!(
            ProjectKey::github("AntoineToussaint", "pretty-hackernews").display_name(),
            "AntoineToussaint/pretty-hackernews"
        );
    }

    #[test]
    fn github_slug_is_key_derived_only_when_the_boundary_is_unambiguous() {
        assert_eq!(
            ProjectKey::github("acme", "widget").unambiguous_github_slug(),
            Some("acme/widget".to_string())
        );
        assert_eq!(
            ProjectKey::github("codefly-dev", "warden-platform").unambiguous_github_slug(),
            None
        );
        assert_eq!(
            ProjectKey::github("acme", "pretty-widget").unambiguous_github_slug(),
            None
        );
        assert_eq!(
            ProjectKey::local("acme-widget").unambiguous_github_slug(),
            None
        );
    }

    #[test]
    fn github_key_matches_only_well_formed_slugs_that_reproduce_it() {
        let key = ProjectKey::github("codefly-dev", "warden-platform");
        assert!(key.matches_github_slug("codefly-dev/warden-platform"));
        assert!(key.matches_github_slug("codefly/dev-warden-platform"));
        assert!(!key.matches_github_slug("other/warden-platform"));
        assert!(!key.matches_github_slug("codefly-dev/warden/platform"));
        assert!(!key.matches_github_slug("codefly-dev"));
    }

    #[test]
    fn github_slug_from_scopes_recovers_hyphenated_owner() {
        // The clone target that the sidebar/git-ops path needs. A
        // hyphenated owner can't survive the flat key round-trip...
        let key = ProjectKey::github("codefly-dev", "warden-platform");
        assert_eq!(
            key.display_name(),
            "codefly/dev-warden-platform",
            "the lossy key split is exactly the bug",
        );
        // ...but the subscribed scope slug carries the boundary, so we
        // recover the real owner/repo and clone the right repo.
        let scopes = [
            "github:other-org/thing".to_string(),
            "github:codefly-dev/warden-platform".to_string(),
        ];
        let slug = key
            .github_slug_from_scopes(scopes.iter().map(String::as_str))
            .expect("subscribed scope reproduces the key");
        assert_eq!(slug, "codefly-dev/warden-platform");
        assert_eq!(
            format!("git@github.com:{slug}.git"),
            "git@github.com:codefly-dev/warden-platform.git",
        );
    }

    #[test]
    fn github_slug_from_scopes_ignores_unrelated_and_non_github() {
        let key = ProjectKey::github("acme", "widget");
        // Org-level subscription (no `/`) can't pin a repo.
        assert_eq!(
            key.github_slug_from_scopes(["github:acme".to_string()].iter().map(String::as_str)),
            None,
        );
        // A local project key never resolves against github scopes.
        assert_eq!(
            ProjectKey::local("notes").github_slug_from_scopes(["github:acme/widget"].into_iter()),
            None,
        );
    }

    #[test]
    fn non_github_key_display_name_drops_source_prefix() {
        assert_eq!(
            ProjectKey::local("my-experiment").display_name(),
            "my-experiment"
        );
        assert_eq!(ProjectKey::linear("TEAM").display_name(), "TEAM");
        assert_eq!(ProjectKey::new("sandbox").display_name(), "sandbox");
    }

    #[test]
    fn project_display_name_repairs_raw_key_fallback() {
        let key = ProjectKey::github("AntoineToussaint", "lazybox");
        // Legacy bug: `name` was defaulted to the raw key string.
        let broken = Project::new(key.clone(), key.as_str(), Utc::now());
        assert_eq!(broken.display_name(), "AntoineToussaint/lazybox");
        // A proper name is left untouched.
        let good = Project::new(key, "AntoineToussaint/lazybox", Utc::now());
        assert_eq!(good.display_name(), "AntoineToussaint/lazybox");
    }

    #[test]
    fn project_display_name_does_not_guess_an_ambiguous_github_slug() {
        let key = ProjectKey::github("codefly-dev", "warden-platform");
        let project = Project::new(key.clone(), key.as_str(), Utc::now());
        assert_eq!(project.display_name(), "github-codefly-dev-warden-platform");
    }

    #[test]
    fn github_project_keeps_repo_identity_separate_from_display_name() {
        let mut project = Project::github("codefly-dev", "warden-platform", Utc::now());
        project.name = "renamed label".to_string();

        assert_eq!(project.github_repo(), Some("codefly-dev/warden-platform"));
        assert_eq!(project.display_name(), "codefly-dev/warden-platform");
    }

    #[test]
    fn legacy_project_can_be_given_a_trusted_github_repo() {
        let key = ProjectKey::github("codefly-dev", "warden-platform");
        let mut project = Project::new(key, "codefly/dev-warden-platform", Utc::now());

        assert_eq!(project.github_repo(), None);
        assert!(project.set_github_repo("codefly-dev/warden-platform"));
        assert_eq!(project.github_repo(), Some("codefly-dev/warden-platform"));
        assert_eq!(project.display_name(), "codefly-dev/warden-platform");
    }

    #[test]
    fn legacy_project_json_defaults_to_no_github_repo() {
        let json = r#"{
            "key":"github-codefly-dev-warden-platform",
            "name":"codefly/dev-warden-platform",
            "root_dir":null,
            "created_at":"2025-01-01T00:00:00Z"
        }"#;

        let project: Project = serde_json::from_str(json).expect("legacy project decodes");

        assert_eq!(project.github_repo(), None);
    }

    #[test]
    fn github_owner_repo_from_url_parses_every_remote_form() {
        let want = Some(("AntoineToussaint".to_string(), "lazybox".to_string()));
        assert_eq!(
            github_owner_repo_from_url("git@github.com:AntoineToussaint/lazybox.git"),
            want
        );
        assert_eq!(
            github_owner_repo_from_url("git@github.com:AntoineToussaint/lazybox"),
            want
        );
        assert_eq!(
            github_owner_repo_from_url("https://github.com/AntoineToussaint/lazybox.git"),
            want
        );
        assert_eq!(
            github_owner_repo_from_url("https://github.com/AntoineToussaint/lazybox"),
            want
        );
        assert_eq!(
            github_owner_repo_from_url("ssh://git@github.com/AntoineToussaint/lazybox.git"),
            want
        );
        // Trailing slash + surrounding whitespace tolerated.
        assert_eq!(
            github_owner_repo_from_url("  https://github.com/AntoineToussaint/lazybox/  "),
            want
        );
        // Hyphenated owner and repo round-trip (the boundary is the
        // first `/`, not a guessed `-`).
        assert_eq!(
            github_owner_repo_from_url("git@github.com:codefly-dev/warden-platform.git"),
            Some(("codefly-dev".to_string(), "warden-platform".to_string()))
        );
    }

    #[test]
    fn github_owner_repo_from_url_rejects_non_github_and_malformed() {
        assert_eq!(
            github_owner_repo_from_url("git@gitlab.com:acme/widget.git"),
            None
        );
        assert_eq!(github_owner_repo_from_url("https://github.com/acme"), None);
        assert_eq!(github_owner_repo_from_url("not a url"), None);
        assert_eq!(github_owner_repo_from_url(""), None);
    }

    #[test]
    fn unprefixed_key_has_empty_source() {
        // Defensive: legacy keys without a `-` prefix (e.g. raw
        // `"sandbox"`) report empty source. Caller decides how to
        // treat that (back-compat → assume local).
        let k = ProjectKey::new("sandbox");
        assert_eq!(k.source_prefix(), "");
    }
}
