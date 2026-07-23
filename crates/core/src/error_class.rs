//! Shared HTTP/transport error classification.
//!
//! Every provider (GitHub, Linear, …) hits the same three-way
//! question when a poll fails: retry next tick, tell the user to
//! rotate their token, or surface a permanent bug. Historically each
//! provider answered it on its own — lowercasing the error's `Display`
//! string and grepping for keywords — and the keyword lists drifted
//! (gh matched `"hyper"`/`"temporarily"`, linear didn't; linear
//! retried anything containing `"connection"` forever). The same
//! transport failure got different verdicts depending on which
//! provider produced it.
//!
//! This module is the single source of truth. Providers extract the
//! *typed* signal from their `reqwest`/`octocrab` error — the HTTP
//! status code and whether it was a transport-layer failure — and
//! feed it to [`classify`]. The stringly-typed substring probe
//! survives only as the last-resort fallback for errors that carry no
//! typed status at all (GraphQL wrapper errors, opaque future
//! variants), and it lives here too so it can never diverge again.
//!
//! Lives in `lazybox-core` (which depends on no internal crate and no
//! HTTP client) so every provider — each of which may depend on core —
//! routes through the one function.

use crate::ProviderError;

/// The three retry verdicts a provider failure resolves to. Maps 1:1
/// onto the non-`Unsupported` [`ProviderError`] variants via
/// [`ErrorClass::into_provider_error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Transient — network hiccup, 5xx, throttling. Retry next cycle.
    Retryable,
    /// Credentials wrong / expired. Surface loud; don't retry until
    /// the user rotates their token.
    Auth,
    /// Won't fix itself — bad query, protocol/schema mismatch, a 4xx
    /// that isn't auth. Surface; don't retry.
    Permanent,
}

impl ErrorClass {
    /// Attach a provider id + diagnostic detail, minting the matching
    /// [`ProviderError`]. Rate-limit errors that carry an exact reset
    /// window should bypass this and call
    /// [`ProviderError::retryable_after`] directly to preserve the
    /// hint.
    pub fn into_provider_error(
        self,
        source: impl Into<String>,
        detail: impl Into<String>,
    ) -> ProviderError {
        match self {
            ErrorClass::Retryable => ProviderError::retryable(source, detail),
            ErrorClass::Auth => ProviderError::auth(source, detail),
            ErrorClass::Permanent => ProviderError::permanent(source, detail),
        }
    }
}

/// Typed signals a provider extracts from its HTTP-client error, fed
/// to [`classify`]. Build with the typed status and transport flag
/// wherever the error type exposes them; the `message` is only
/// consulted when neither is present.
#[derive(Debug, Default, Clone, Copy)]
pub struct HttpErrorSignals<'a> {
    /// The HTTP status code, when the failure carried an actual HTTP
    /// response. `None` for transport-layer failures (no response was
    /// ever received) and for non-HTTP error variants.
    pub status: Option<u16>,
    /// `true` when the error is a transport-layer failure — connect,
    /// DNS, TLS, timeout, or a body/decode error — with no usable HTTP
    /// status. Such failures never reached the provider's app layer,
    /// so a fresh attempt next tick is safe and usually succeeds.
    pub transport: bool,
    /// Last-resort text for the substring fallback, used ONLY when
    /// `status` is `None` and `transport` is `false`.
    pub message: &'a str,
}

/// Classify an HTTP status code alone. The canonical status→verdict
/// mapping every provider shares:
///
/// - `401` / `403` → [`ErrorClass::Auth`]
/// - `429` and `5xx` → [`ErrorClass::Retryable`]
/// - everything else (other `4xx`, unexpected `2xx`/`3xx`) →
///   [`ErrorClass::Permanent`]
pub fn classify_status(status: u16) -> ErrorClass {
    match status {
        401 | 403 => ErrorClass::Auth,
        429 => ErrorClass::Retryable,
        s if (500..=599).contains(&s) => ErrorClass::Retryable,
        _ => ErrorClass::Permanent,
    }
}

/// The stringly-typed last resort. Only reached from [`classify`] when
/// no typed status and no transport flag were available. Kept
/// deliberately in one place so the two providers can't grow divergent
/// keyword lists again.
///
/// Auth wins over retryable (an expired token that also mentions
/// "connection" is still an auth failure); retryable wins over
/// permanent.
pub fn classify_message(message: &str) -> ErrorClass {
    let lower = message.to_lowercase();

    const AUTH: &[&str] = &["unauthorized", "forbidden", "authentication", "401", "403"];
    if AUTH.iter().any(|k| lower.contains(k)) {
        return ErrorClass::Auth;
    }

    const RETRYABLE: &[&str] = &[
        "timed out",
        "timeout",
        "connection",
        "network",
        "rate limit",
        "temporarily",
        "hyper",
        "502",
        "503",
        "504",
    ];
    if RETRYABLE.iter().any(|k| lower.contains(k)) {
        return ErrorClass::Retryable;
    }

    ErrorClass::Permanent
}

/// The one classifier. Prefers the typed HTTP status, then the
/// transport flag, and only then falls back to the substring probe on
/// the message.
pub fn classify(signals: &HttpErrorSignals) -> ErrorClass {
    if let Some(status) = signals.status {
        return classify_status(status);
    }
    if signals.transport {
        return ErrorClass::Retryable;
    }
    classify_message(signals.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_5xx_is_retryable() {
        assert_eq!(classify_status(500), ErrorClass::Retryable);
        assert_eq!(classify_status(502), ErrorClass::Retryable);
        assert_eq!(classify_status(503), ErrorClass::Retryable);
        assert_eq!(classify_status(504), ErrorClass::Retryable);
    }

    #[test]
    fn status_429_is_retryable() {
        assert_eq!(classify_status(429), ErrorClass::Retryable);
    }

    #[test]
    fn status_401_and_403_are_auth() {
        assert_eq!(classify_status(401), ErrorClass::Auth);
        assert_eq!(classify_status(403), ErrorClass::Auth);
    }

    #[test]
    fn status_404_and_422_are_permanent() {
        assert_eq!(classify_status(404), ErrorClass::Permanent);
        assert_eq!(classify_status(422), ErrorClass::Permanent);
        assert_eq!(classify_status(400), ErrorClass::Permanent);
    }

    #[test]
    fn transport_failure_is_retryable() {
        let signals = HttpErrorSignals {
            status: None,
            transport: true,
            message: "error sending request: connection reset",
        };
        assert_eq!(classify(&signals), ErrorClass::Retryable);
    }

    #[test]
    fn typed_status_wins_over_message() {
        // A 404 whose message happens to contain a retryable word must
        // still classify permanent — the typed status is authoritative.
        let signals = HttpErrorSignals {
            status: Some(404),
            transport: false,
            message: "connection to the resource: not found",
        };
        assert_eq!(classify(&signals), ErrorClass::Permanent);
    }

    #[test]
    fn message_fallback_detects_transient_words() {
        for msg in [
            "operation timed out",
            "request timeout",
            "connection refused",
            "network is unreachable",
            "secondary rate limit hit",
            "service temporarily unavailable",
            "hyper error: broken pipe",
            "server returned 502",
            "got a 503",
            "gateway 504",
        ] {
            assert_eq!(
                classify_message(msg),
                ErrorClass::Retryable,
                "expected retryable for {msg:?}"
            );
        }
    }

    #[test]
    fn message_fallback_detects_auth_words() {
        for msg in [
            "unauthorized",
            "forbidden resource",
            "authentication required",
            "server said 401",
            "http 403 returned",
        ] {
            assert_eq!(
                classify_message(msg),
                ErrorClass::Auth,
                "expected auth for {msg:?}"
            );
        }
    }

    #[test]
    fn message_fallback_defaults_to_permanent() {
        assert_eq!(
            classify_message("unexpected null field in response"),
            ErrorClass::Permanent
        );
    }

    #[test]
    fn auth_wins_over_retryable_in_message() {
        // Both an auth word and a retryable word present → auth.
        assert_eq!(
            classify_message("unauthorized: connection via proxy"),
            ErrorClass::Auth
        );
    }

    #[test]
    fn into_provider_error_maps_each_variant() {
        assert!(
            ErrorClass::Retryable
                .into_provider_error("github", "x")
                .is_retryable()
        );
        assert!(
            ErrorClass::Auth
                .into_provider_error("github", "x")
                .is_auth()
        );
        let perm = ErrorClass::Permanent.into_provider_error("github", "x");
        assert!(!perm.is_retryable() && !perm.is_auth());
    }
}
