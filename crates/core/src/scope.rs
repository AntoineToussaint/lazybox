//! Scope discovery — the provider-agnostic answer to "which slices
//! of this provider's namespace am I allowed to see, and which do I
//! want to subscribe to?".
//!
//! GitHub's scopes are orgs (parent) and repos (children). Linear's
//! are workspaces and projects. Future providers add their own
//! shape; the trait stays the same.
//!
//! Setup flow calls [`ScopeSource::list_scopes`] once per provider
//! after auth completes. The user multi-selects scopes; the
//! selection lands in [`crate::PersistedSetup::selected_scopes`] and
//! gates polling.
//!
//! ## Why a trait, not a struct
//!
//! Discovery is provider-specific (different APIs, different
//! pagination, different auth). A trait keeps the setup-screen
//! component generic — it asks any provider for its scopes and
//! renders the same picker. Tests inject [`MockScopeSource`] so the
//! picker works without real network access; production code wires
//! `lazybox_gh::GhScopes` (or whatever provider) for the real call.

use crate::ProviderError;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

/// One selectable slice of a provider's namespace.
///
/// `id` is the persistence key — gets serialized into
/// `PersistedSetup.selected_scopes` so the daemon can filter
/// polling by it. Format is provider-prefixed (`"github:owner/repo"`)
/// so two providers can't collide.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Scope {
    pub id: String,
    pub label: String,
    /// `id` of the parent scope, if any. Repos point at their org;
    /// orgs are roots. Surfaces in the picker as a tree.
    #[serde(default)]
    pub parent: Option<String>,
    pub kind: ScopeKind,
    /// Whether the underlying repo is private. Only meaningful for
    /// `ScopeKind::Repo`; orgs and other scopes leave it `false`. The
    /// picker renders a marker for private repos so scopes from a
    /// mixed org are easy to tell apart.
    #[serde(default)]
    pub private: bool,
}

/// Coarse classification used by the picker for icons / sort order.
/// New providers add variants as needed; consumers must handle
/// unknown variants gracefully (the picker renders them as plain
/// rows, no special chrome).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ScopeKind {
    /// GitHub org, Linear workspace, Jira instance — the top
    /// container. Picker renders these as group headers.
    Org,
    /// GitHub repo, Linear project — leaf scopes the poller
    /// actually subscribes to.
    Repo,
    /// Any other shape. New providers can use this until the picker
    /// learns about them.
    Other,
}

/// Provider-side scope discovery. Setup flow calls `list_scopes`
/// once after auth; the result feeds the orgs picker. After the user
/// picks one or more parents, the flow drills down by calling
/// `list_children(parent_id)` per parent.
///
/// Boxed-future return rather than `async fn` so the trait is
/// `dyn`-compatible — the setup screen iterates over a
/// `Vec<Box<dyn ScopeSource>>` without monomorphization per provider.
pub trait ScopeSource: Send + Sync {
    /// Provider id matching `TaskProvider::name`. Used to key
    /// `PersistedSetup.selected_scopes`.
    fn provider_id(&self) -> &str;

    /// Hydrate the top-level scope list. For GitHub: every org the
    /// user belongs to, plus their personal account. One API call.
    /// The returned scopes are typically `ScopeKind::Org`; repos
    /// come from `list_children` after the user picks an org.
    fn list_scopes<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Scope>, ProviderError>> + Send + 'a>>;

    /// Hydrate the children of `parent_id` (e.g. repos under an org).
    /// Called lazily once the user picks a parent in the orgs picker —
    /// avoids fetching every repo upfront. Default impl returns empty,
    /// which the setup flow treats as "no further drill-down" and
    /// skips the per-parent repo picker phase.
    fn list_children<'a>(
        &'a self,
        _parent_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Scope>, ProviderError>> + Send + 'a>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// In-memory scope source for tests. Lets the setup flow run end-
/// to-end without a real GH token or network access.
///
/// ```
/// use lazybox_core::{MockScopeSource, ScopeSource};
/// let mock = MockScopeSource::new("github")
///     .with_org("acme")
///     .with_repo("acme", "web")
///     .with_repo("acme", "api");
/// assert_eq!(mock.provider_id(), "github");
/// ```
#[derive(Debug, Clone, Default)]
pub struct MockScopeSource {
    provider_id: String,
    scopes: Vec<Scope>,
}

impl MockScopeSource {
    pub fn new(provider_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            scopes: Vec::new(),
        }
    }

    pub fn with_org(mut self, org: &str) -> Self {
        let id = format!("{}:{org}", self.provider_id);
        self.scopes.push(Scope {
            id,
            label: org.to_string(),
            parent: None,
            kind: ScopeKind::Org,
            private: false,
        });
        self
    }

    /// Add a repo under `org`. Repos surface from `list_children`
    /// once the user picks the parent org in the picker; they are
    /// NOT returned by `list_scopes`, which mirrors how GitHub
    /// shapes the data (orgs cheap, repos lazy).
    pub fn with_repo(mut self, org: &str, repo: &str) -> Self {
        let parent_id = format!("{}:{org}", self.provider_id);
        let id = format!("{}:{org}/{repo}", self.provider_id);
        self.scopes.push(Scope {
            id,
            label: format!("{org}/{repo}"),
            parent: Some(parent_id),
            kind: ScopeKind::Repo,
            private: false,
        });
        self
    }

    /// Direct insert for callers that already have a `Scope` —
    /// useful when adapting fixtures from real GraphQL responses.
    pub fn push(mut self, scope: Scope) -> Self {
        self.scopes.push(scope);
        self
    }
}

impl ScopeSource for MockScopeSource {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn list_scopes<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Scope>, ProviderError>> + Send + 'a>> {
        // Top-level: only orgs.
        let scopes: Vec<Scope> = self
            .scopes
            .iter()
            .filter(|s| s.kind == ScopeKind::Org)
            .cloned()
            .collect();
        Box::pin(async move { Ok(scopes) })
    }

    fn list_children<'a>(
        &'a self,
        parent_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Scope>, ProviderError>> + Send + 'a>> {
        let scopes: Vec<Scope> = self
            .scopes
            .iter()
            .filter(|s| s.parent.as_deref() == Some(parent_id))
            .cloned()
            .collect();
        Box::pin(async move { Ok(scopes) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny runtime-free poll. The mock's future is non-blocking
    /// (it returns Ready on the first poll), so we don't pull in
    /// tokio just to test it.
    fn run<T>(fut: impl Future<Output = T>) -> T {
        use std::pin::pin;
        use std::task::{Context, Poll, Waker};
        let mut fut = pin!(fut);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("MockScopeSource futures must resolve immediately"),
        }
    }

    #[test]
    fn list_scopes_returns_orgs_only() {
        // Repos live under list_children, not list_scopes — keeps
        // the picker's first fetch cheap.
        let mock = MockScopeSource::new("github")
            .with_org("acme")
            .with_repo("acme", "web")
            .with_org("widgets")
            .with_repo("widgets", "core");
        let scopes = run(mock.list_scopes()).unwrap();
        let labels: Vec<&str> = scopes.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["acme", "widgets"]);
        assert!(scopes.iter().all(|s| s.kind == ScopeKind::Org));
    }

    #[test]
    fn list_children_returns_repos_under_parent() {
        let mock = MockScopeSource::new("github")
            .with_org("acme")
            .with_repo("acme", "web")
            .with_repo("acme", "api")
            .with_org("widgets")
            .with_repo("widgets", "core");
        let acme = run(mock.list_children("github:acme")).unwrap();
        let labels: Vec<&str> = acme.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["acme/web", "acme/api"]);
        assert!(acme.iter().all(|s| s.kind == ScopeKind::Repo));
    }

    #[test]
    fn list_children_unknown_parent_is_empty() {
        let mock = MockScopeSource::new("github").with_org("acme");
        let result = run(mock.list_children("github:nonsense")).unwrap();
        assert!(result.is_empty());
    }
}
