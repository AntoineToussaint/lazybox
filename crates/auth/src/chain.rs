use crate::{Credential, CredentialError, CredentialProvider};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::{debug, trace};

/// TTL for cached credential resolution (both successes and failures).
/// Disabled/unconfigured providers deterministically fail; caching the result
/// prevents expensive chain re-runs on every poll tick.
const CHAIN_CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes

/// Cached chain resolution result: either a resolved credential or the error.
struct CacheEntry {
    outcome: Result<Credential, CredentialError>,
    at: Instant,
}

/// Process-global cache for credential chain resolutions per scope.
fn chain_cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CHAIN_CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CHAIN_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Tries multiple credential providers in order, returning the first success.
/// Modeled after AWS SDK's credential chain.
///
/// Chain resolutions (both successes and failures) are cached per scope for
/// `CHAIN_CACHE_TTL` to avoid expensive repeated provider attempts when
/// configured credentials are absent or disabled.
///
/// ```rust,no_run
/// use lazybox_auth::*;
///
/// let chain = CredentialChain::new()
///     .with(EnvProvider::new("GH_TOKEN"))
///     .with(EnvProvider::new("GITHUB_TOKEN"))
///     .with(CommandProvider::new("gh", &["auth", "token"]));
///
/// // In async context:
/// // let cred = chain.resolve("github").await?;
/// ```
pub struct CredentialChain {
    providers: Vec<Box<dyn CredentialProviderBoxed>>,
}

/// Object-safe version of CredentialProvider for dynamic dispatch.
trait CredentialProviderBoxed: Send + Sync {
    fn name(&self) -> &str;
    fn resolve_boxed<'a>(
        &'a self,
        scope: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Credential, CredentialError>> + Send + 'a>,
    >;
}

impl<T: CredentialProvider> CredentialProviderBoxed for T {
    fn name(&self) -> &str {
        CredentialProvider::name(self)
    }

    fn resolve_boxed<'a>(
        &'a self,
        scope: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Credential, CredentialError>> + Send + 'a>,
    > {
        Box::pin(CredentialProvider::resolve(self, scope))
    }
}

impl CredentialChain {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Add a provider to the end of the chain.
    pub fn with<P: CredentialProvider + 'static>(mut self, provider: P) -> Self {
        self.providers.push(Box::new(provider));
        self
    }

    /// Try each provider in order. Returns the first successful credential.
    ///
    /// Chain resolutions are cached per scope; if a cached result exists and
    /// is still fresh (within `CHAIN_CACHE_TTL`), it is returned immediately
    /// without running any providers. This avoids expensive re-runs when
    /// configured credentials are absent or disabled.
    ///
    /// When every provider declines, the chain reports *why*: a
    /// [`CredentialError::NotFound`] is mere absence (nothing configured) and
    /// must not mask a real failure, but any other error means a provider
    /// ran and failed — a locked keyring, a network blip, an expired
    /// token — so the most recent such failure is surfaced instead of a bare
    /// [`CredentialError::Exhausted`]. Collapsing every cause into `Exhausted`
    /// made a 2-second `gh auth token` hiccup indistinguishable from an
    /// unconfigured user, turning a transient error into a permanent-looking
    /// auth failure. Only a chain where *no* provider even ran (all absent)
    /// stays `Exhausted` — where "absent" covers both `NotFound` and a
    /// provider echoing this chain's own `Exhausted` sentinel.
    pub async fn resolve(&self, scope: &str) -> Result<Credential, CredentialError> {
        // Check cache first: if a fresh cached result exists, return it.
        {
            let cache = chain_cache().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = cache.get(scope)
                && entry.at.elapsed() < CHAIN_CACHE_TTL
            {
                trace!(scope, "credential chain cache hit");
                return entry.outcome.clone();
            }
        }

        // Cache miss or stale; run the chain.
        let mut last_failure: Option<CredentialError> = None;
        for provider in &self.providers {
            trace!(
                provider = provider.name(),
                scope, "trying credential provider"
            );
            match provider.resolve_boxed(scope).await {
                Ok(cred) => {
                    debug!(
                        provider = provider.name(),
                        source = %cred.source,
                        scope,
                        "credential resolved"
                    );
                    // Cache the success before returning.
                    {
                        let mut cache = chain_cache().lock().unwrap_or_else(|e| e.into_inner());
                        cache.insert(
                            scope.to_string(),
                            CacheEntry {
                                outcome: Ok(cred.clone()),
                                at: Instant::now(),
                            },
                        );
                    }
                    return Ok(cred);
                }
                Err(e) => {
                    trace!(provider = provider.name(), error = %e, "provider skipped");
                    // `NotFound` is mere absence; `Exhausted` is this chain's
                    // own sentinel (a provider returning it — e.g. a nested
                    // chain — is signalling absence too). Neither is a real
                    // failure worth surfacing, so neither may mask one.
                    if !matches!(e, CredentialError::NotFound(_) | CredentialError::Exhausted) {
                        last_failure = Some(e);
                    }
                    continue;
                }
            }
        }
        let result = Err(last_failure.unwrap_or(CredentialError::Exhausted));
        // Cache the failure too.
        {
            let mut cache = chain_cache().lock().unwrap_or_else(|e| e.into_inner());
            cache.insert(
                scope.to_string(),
                CacheEntry {
                    outcome: result.clone(),
                    at: Instant::now(),
                },
            );
        }
        result
    }

    /// Number of providers in the chain.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Provider names in resolution order. Lets callers assert the order a
    /// chain resolves in — e.g. that a last-resort provider stays behind the
    /// providers meant to take precedence — without running any resolution.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

impl Default for CredentialChain {
    fn default() -> Self {
        Self::new()
    }
}

/// Clear the process-global credential chain cache. Useful when credentials
/// have been updated (e.g., a user has run `gh auth login` and hits refresh).
pub fn invalidate_chain_cache() {
    chain_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// Drop every cached chain **failure**, keeping cached successes.
///
/// The failure half of `CHAIN_CACHE_TTL` exists to stop a polling loop
/// re-running a chain that deterministically fails — it is a throughput
/// guard, not a verdict. A *user-initiated* resolve is a fresh intent and
/// must not be answered from it: `gh auth token` failing once at launch
/// otherwise decides what the user sees for the next five minutes, so
/// someone who fixes their token and immediately retries gets the stale
/// error back with no provider run at all. That reads as "my fix did
/// nothing" and is indistinguishable from the original fault.
///
/// Only failures are forgotten, so this never costs a healthy session a
/// re-resolve: the worst case is one extra provider attempt for a scope
/// that was already broken. Callers that must also bypass
/// [`crate::invalidate_command_credential_cache`]'s sibling backoff should
/// use [`crate::invalidate_failed_credentials`], which clears both — the
/// command-provider cache is consulted *inside* the chain, so clearing one
/// without the other still serves a stale answer.
pub fn forget_failed_chain_resolutions() {
    chain_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, entry| entry.outcome.is_ok());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that touch the process-global chain cache.
    /// They each `invalidate_chain_cache()` and assert exact provider
    /// call counts, so running them concurrently lets one test's
    /// invalidate/resolve race another's and flip a count assertion. This
    /// lock makes them run one at a time; non-cache tests are unaffected.
    static CACHE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A provider that always fails with a preset error — lets the chain
    /// tests assert which cause survives to the exhausted result.
    struct Failing {
        name: &'static str,
        err: fn() -> CredentialError,
    }

    impl CredentialProvider for Failing {
        fn name(&self) -> &str {
            self.name
        }
        async fn resolve(&self, _scope: &str) -> Result<Credential, CredentialError> {
            Err((self.err)())
        }
    }

    fn not_found() -> CredentialError {
        CredentialError::NotFound("nothing here".into())
    }
    fn provider_err() -> CredentialError {
        CredentialError::Provider("gh auth token: keyring locked".into())
    }
    fn exhausted() -> CredentialError {
        CredentialError::Exhausted
    }

    /// A provider that fails its first `fail_first` resolves and then
    /// succeeds, counting every call. The counter is how these tests tell
    /// "the chain ran its providers again" apart from "the chain answered
    /// from cache" — the distinction the whole failure-cache question
    /// turns on.
    struct FlakyCounted {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        fail_first: usize,
    }

    impl CredentialProvider for FlakyCounted {
        fn name(&self) -> &str {
            "flaky"
        }
        async fn resolve(&self, _scope: &str) -> Result<Credential, CredentialError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.fail_first {
                Err(CredentialError::Provider("keyring locked".into()))
            } else {
                Ok(Credential::new("recovered-token", "flaky"))
            }
        }
    }

    /// A user who fixes their credential and immediately retries must
    /// actually get a retry. The failure half of `CHAIN_CACHE_TTL` is a
    /// throughput guard for the poll loop, so without
    /// `forget_failed_chain_resolutions` the second resolve here answers
    /// from cache — same error, zero providers run — for five minutes, and
    /// the user reads their own fix as having done nothing.
    #[tokio::test]
    async fn forgetting_failures_lets_the_next_resolve_actually_run_again() {
        let _cache_guard = CACHE_TEST_LOCK.lock().await;
        invalidate_chain_cache();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let chain = CredentialChain::new().with(FlakyCounted {
            calls: calls.clone(),
            fail_first: 1,
        });

        assert!(
            chain.resolve("github").await.is_err(),
            "first resolve fails"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // The sticky half: a second resolve inside the TTL is served from
        // cache and never reaches the provider that would now succeed.
        assert!(
            chain.resolve("github").await.is_err(),
            "a cached failure is served without running providers"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the cached failure must not have run the provider again"
        );

        forget_failed_chain_resolutions();
        assert!(
            chain.resolve("github").await.is_ok(),
            "after forgetting the failure the retry must reach the provider"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the retry must actually have run the provider"
        );
        invalidate_chain_cache();
    }

    /// Forgetting failures must not cost a healthy scope a re-resolve:
    /// a cached success stays cached, so an explicit refresh never turns
    /// into an extra `gh auth token` subprocess for a working token.
    #[tokio::test]
    async fn forgetting_failures_keeps_cached_successes() {
        let _cache_guard = CACHE_TEST_LOCK.lock().await;
        invalidate_chain_cache();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let chain = CredentialChain::new().with(FlakyCounted {
            calls: calls.clone(),
            fail_first: 0,
        });

        assert!(chain.resolve("github").await.is_ok());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        forget_failed_chain_resolutions();
        assert!(chain.resolve("github").await.is_ok());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a cached success must survive forgetting failures"
        );
        invalidate_chain_cache();
    }

    #[tokio::test]
    async fn all_absent_reports_exhausted() {
        let _cache_guard = CACHE_TEST_LOCK.lock().await;
        invalidate_chain_cache();
        let chain = CredentialChain::new()
            .with(Failing {
                name: "env",
                err: not_found,
            })
            .with(Failing {
                name: "cmd",
                err: not_found,
            });
        assert!(matches!(
            chain.resolve("github").await,
            Err(CredentialError::Exhausted)
        ));
        invalidate_chain_cache();
    }

    #[tokio::test]
    async fn a_real_provider_failure_is_surfaced_not_masked() {
        let _cache_guard = CACHE_TEST_LOCK.lock().await;
        invalidate_chain_cache();
        // A transient `gh auth token` failure must not read as "nothing
        // configured" (Exhausted) — the specific cause survives.
        let chain = CredentialChain::new()
            .with(Failing {
                name: "env",
                err: not_found,
            })
            .with(Failing {
                name: "cmd",
                err: provider_err,
            });
        match chain.resolve("github").await {
            Err(CredentialError::Provider(msg)) => assert!(msg.contains("keyring locked")),
            other => panic!("expected the provider failure, got {other:?}"),
        }
        invalidate_chain_cache();
    }

    #[tokio::test]
    async fn a_later_absence_does_not_overwrite_an_earlier_failure() {
        let _cache_guard = CACHE_TEST_LOCK.lock().await;
        invalidate_chain_cache();
        let chain = CredentialChain::new()
            .with(Failing {
                name: "cmd",
                err: provider_err,
            })
            .with(Failing {
                name: "env",
                err: not_found,
            });
        assert!(matches!(
            chain.resolve("github").await,
            Err(CredentialError::Provider(_))
        ));
        invalidate_chain_cache();
    }

    #[tokio::test]
    async fn an_exhausted_sentinel_does_not_overwrite_an_earlier_failure() {
        let _cache_guard = CACHE_TEST_LOCK.lock().await;
        invalidate_chain_cache();
        // A provider echoing this chain's own `Exhausted` sentinel (e.g. a
        // nested chain that found nothing) signals absence, not a failure —
        // it must not mask the real `Provider` error that preceded it.
        let chain = CredentialChain::new()
            .with(Failing {
                name: "cmd",
                err: provider_err,
            })
            .with(Failing {
                name: "nested",
                err: exhausted,
            });
        match chain.resolve("github").await {
            Err(CredentialError::Provider(msg)) => assert!(msg.contains("keyring locked")),
            other => panic!("expected the provider failure, got {other:?}"),
        }
        invalidate_chain_cache();
    }

    /// A counter to track how many times the provider's resolve is called.
    struct CountingProvider {
        name: &'static str,
        call_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        err: fn() -> CredentialError,
    }

    impl CredentialProvider for CountingProvider {
        fn name(&self) -> &str {
            self.name
        }
        async fn resolve(&self, _scope: &str) -> Result<Credential, CredentialError> {
            self.call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err((self.err)())
        }
    }

    #[tokio::test]
    async fn disabled_provider_result_is_cached_on_second_resolve() {
        let _cache_guard = CACHE_TEST_LOCK.lock().await;
        // A disabled provider (all return NotFound) must not re-run the full
        // chain on every resolve. The first resolve runs the chain; the second
        // must hit the cache and skip the providers.
        invalidate_chain_cache(); // Start clean.

        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = CountingProvider {
            name: "env",
            call_count: call_count.clone(),
            err: not_found,
        };

        let chain = CredentialChain::new().with(counter);

        // First resolve runs the provider.
        let _ = chain.resolve("github").await;
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "first resolve must call the provider"
        );

        // Second resolve must hit cache and not call the provider.
        let _ = chain.resolve("github").await;
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "second resolve must be a cache hit; provider not called again"
        );

        invalidate_chain_cache(); // Clean up for next test.
    }

    #[tokio::test]
    async fn different_scopes_have_independent_caches() {
        let _cache_guard = CACHE_TEST_LOCK.lock().await;
        // Two different scopes should maintain independent caches.
        invalidate_chain_cache();

        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = CountingProvider {
            name: "env",
            call_count: call_count.clone(),
            err: not_found,
        };

        let chain = CredentialChain::new().with(counter);

        // Resolve for "github" scope.
        let _ = chain.resolve("github").await;
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Resolve for "linear" scope — should call the provider again since
        // "linear" isn't cached yet.
        let _ = chain.resolve("linear").await;
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "different scope must not hit cache; provider called again"
        );

        // A second resolve of "github" must be a cache hit.
        let _ = chain.resolve("github").await;
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "github scope must be cached"
        );

        invalidate_chain_cache();
    }

    #[tokio::test]
    async fn explicit_invalidation_allows_fresh_run() {
        let _cache_guard = CACHE_TEST_LOCK.lock().await;
        // After invalidating the cache explicitly, the next resolve must
        // re-run the providers — simulating a user refresh.
        invalidate_chain_cache();

        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = CountingProvider {
            name: "env",
            call_count: call_count.clone(),
            err: not_found,
        };

        let chain = CredentialChain::new().with(counter);

        // First resolve caches the result.
        let _ = chain.resolve("github").await;
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Second resolve hits cache.
        let _ = chain.resolve("github").await;
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Invalidate the cache.
        invalidate_chain_cache();

        // Third resolve must re-run the provider.
        let _ = chain.resolve("github").await;
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "after invalidation, provider must be called again"
        );

        invalidate_chain_cache();
    }

    #[tokio::test]
    async fn successful_credential_is_cached_too() {
        let _cache_guard = CACHE_TEST_LOCK.lock().await;
        // Successes are also cached, not just failures.
        invalidate_chain_cache();

        struct SuccessProvider {
            call_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }

        impl CredentialProvider for SuccessProvider {
            fn name(&self) -> &str {
                "success"
            }
            async fn resolve(&self, _scope: &str) -> Result<Credential, CredentialError> {
                self.call_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Credential::new("token123", "test"))
            }
        }

        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = SuccessProvider {
            call_count: call_count.clone(),
        };

        let chain = CredentialChain::new().with(provider);

        // First resolve runs the provider and caches success.
        let first = chain.resolve("github").await.expect("first succeeds");
        assert_eq!(first.token(), "token123");
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Second resolve hits cache without calling the provider.
        let second = chain.resolve("github").await.expect("second succeeds");
        assert_eq!(second.token(), "token123");
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "success must be cached; provider not called again"
        );

        invalidate_chain_cache();
    }
}
