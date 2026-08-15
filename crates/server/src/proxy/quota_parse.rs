//! Plan-quota extraction from an LLM response — the "can I keep working?"
//! signal, distinct from the token counts [`super::usage_parse`] recovers.
//!
//! Anthropic reports it on **response headers** of every real `/v1/messages`
//! call (the `count_tokens` endpoint omits them, but agents don't hit that):
//!   - `anthropic-ratelimit-unified-5h-utilization`  — fraction 0..1
//!   - `anthropic-ratelimit-unified-5h-reset`        — unix seconds
//!   - `anthropic-ratelimit-unified-7d-utilization`  — fraction 0..1
//!   - `anthropic-ratelimit-unified-7d-reset`        — unix seconds
//! This mirrors what Claude Code's `/usage` surfaces; the metering proxy
//! already holds the upstream `HeaderMap`, so reading it is free.
//!
//! Codex delivers the same shape (`rate_limits.primary`/`.secondary`) in its
//! session log rather than proxy-visible headers, so it is sourced
//! separately (see the daemon's Codex quota reader), not here.

use hyper::header::HeaderMap;
use lazybox_ipc::{ProviderQuota, QuotaWindow};

/// Read Anthropic's unified rate-limit headers into a [`ProviderQuota`].
/// Absent headers yield an empty quota (`ProviderQuota::is_empty`), which the
/// caller drops rather than broadcasting.
pub fn parse_anthropic_headers(headers: &HeaderMap) -> ProviderQuota {
    ProviderQuota {
        five_hour: anthropic_window(
            headers,
            "anthropic-ratelimit-unified-5h-utilization",
            "anthropic-ratelimit-unified-5h-reset",
        ),
        weekly: anthropic_window(
            headers,
            "anthropic-ratelimit-unified-7d-utilization",
            "anthropic-ratelimit-unified-7d-reset",
        ),
    }
}

fn anthropic_window(headers: &HeaderMap, util_key: &str, reset_key: &str) -> Option<QuotaWindow> {
    let utilization_bp = utilization_bp(header_str(headers, util_key)?)?;
    let reset_at = header_str(headers, reset_key).and_then(|raw| raw.trim().parse::<i64>().ok());
    Some(QuotaWindow {
        utilization_bp,
        reset_at,
    })
}

fn header_str<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    headers.get(key)?.to_str().ok()
}

/// Convert Anthropic's utilization fraction (`0.45`) to basis points
/// (0..=10000). Over-budget values (`>1.0`) clamp to full rather than
/// wrapping, and a negative or non-finite value is rejected.
fn utilization_bp(raw: &str) -> Option<u32> {
    let value: f64 = raw.trim().parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some(((value * 10_000.0).round() as i64).clamp(0, 10_000) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (key, value) in pairs {
            map.insert(
                hyper::header::HeaderName::from_bytes(key.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn both_windows_parse_from_fractional_utilization() {
        let quota = parse_anthropic_headers(&headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.4512"),
            ("anthropic-ratelimit-unified-5h-reset", "1700000000"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.6"),
            ("anthropic-ratelimit-unified-7d-reset", "1700600000"),
        ]));
        let five = quota.five_hour.expect("5h window");
        assert_eq!(five.utilization_bp, 4512);
        assert_eq!(five.reset_at, Some(1_700_000_000));
        let weekly = quota.weekly.expect("weekly window");
        assert_eq!(weekly.utilization_bp, 6000);
    }

    #[test]
    fn absent_headers_yield_an_empty_quota() {
        assert!(parse_anthropic_headers(&HeaderMap::new()).is_empty());
    }

    #[test]
    fn a_window_without_a_reset_still_reports_utilization() {
        let quota = parse_anthropic_headers(&headers(&[(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.1",
        )]));
        let five = quota.five_hour.expect("5h window");
        assert_eq!(five.utilization_bp, 1000);
        assert_eq!(five.reset_at, None);
        assert!(quota.weekly.is_none());
    }

    #[test]
    fn over_budget_utilization_clamps_to_full() {
        // A fraction past 1.0 (over budget) caps at 100%, never wraps.
        let quota = parse_anthropic_headers(&headers(&[(
            "anthropic-ratelimit-unified-5h-utilization",
            "1.5",
        )]));
        assert_eq!(quota.five_hour.unwrap().utilization_bp, 10_000);
    }
}
