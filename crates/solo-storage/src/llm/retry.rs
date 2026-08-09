// SPDX-License-Identifier: Apache-2.0

//! Exponential backoff with jitter for transient LLM HTTP errors.
//!
//! Both [`super::anthropic::AnthropicClient`] and
//! [`super::openai::OpenAIClient`] wrap their `complete()` HTTP
//! call in a small retry loop that:
//!
//!   - Distinguishes **retryable** failures (HTTP 429, 5xx;
//!     connection errors, timeouts) from **terminal** failures
//!     (HTTP 4xx other than 429, parse failures, auth errors).
//!   - Honours `Retry-After` headers on 429s when present
//!     (capped at [`RetryConfig::max_delay`] so a malicious or
//!     malformed header can't force an unbounded sleep).
//!   - Falls back to exponential backoff with full jitter:
//!     `delay = random(0, base * 2^attempt)`, capped at `max_delay`.
//!     Full jitter (per AWS Architecture Blog) reduces thundering
//!     herd more than equal jitter.
//!   - Caps total attempts at [`RetryConfig::max_retries`] + 1
//!     (initial + N retries). Default is 3 retries, so 4 total
//!     attempts.
//!
//! ## Why hand-rolled instead of `tower-retry` or `backoff`
//!
//! The retry decision needs to read response headers
//! (`Retry-After`) and the response body cheaply enough to
//! match against API error messages. Generic retry middleware
//! tends to either consume the response (forcing re-parse on
//! the success path) or lose the `Retry-After` signal.
//! Hand-rolled is ~80 lines and pins exactly the semantics we
//! want; the dependency-free version also keeps `solo-storage`'s
//! transitive graph small.
//!
//! ## What this does NOT do
//!
//!   - **No request-level idempotency keys.** A retried call
//!     could in principle hit the API twice if the original
//!     request reached the server but the response was lost in
//!     transit — duplicating side effects (token spend, rate-
//!     limit consumption). For Solo's Steward use case this
//!     is benign (LLM "side effects" are billing only), but
//!     once the trait grows side-effecting tools we'll need
//!     real idempotency.
//!
//!   - **No streaming retry.** The retry wraps a complete
//!     non-streaming response. Streaming responses that fail
//!     mid-stream are out of scope (and Solo's Steward doesn't
//!     stream).
//!
//!   - **No circuit-breaker.** A persistently-failing endpoint
//!     produces N + 1 attempts per `complete()` call without
//!     adapting. Acceptable for batch consolidate; would want
//!     a breaker for high-RPS interactive use cases.

use std::time::Duration;

/// Tunable retry parameters. Both backends accept this via a
/// `with_retry_config(...)` builder. Defaults are conservative:
/// 3 retries (4 attempts total), 500ms base, 10s cap.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Number of retries after the initial attempt. Total
    /// attempts = `max_retries + 1`. Set to 0 to disable retries
    /// entirely (one attempt only).
    pub max_retries: u32,
    /// Base delay before the first retry. Subsequent retries
    /// scale this by 2^attempt (with full jitter).
    pub base_delay: Duration,
    /// Maximum sleep between attempts. Caps both the exponential
    /// backoff curve AND any `Retry-After` header value — a
    /// malicious server that returns `Retry-After: 999999` won't
    /// hang us.
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(10),
        }
    }
}

impl RetryConfig {
    /// No retries — single attempt. Useful in tests that want to
    /// observe the first failure without sleep noise.
    pub fn none() -> Self {
        Self {
            max_retries: 0,
            base_delay: Duration::from_millis(0),
            max_delay: Duration::from_millis(0),
        }
    }
}

/// HTTP status codes the retry loop treats as **retryable**.
/// Returns true for the standard transient set: 429 (rate limit)
/// and the 5xx family.
///
/// 408 (Request Timeout) is NOT included — both Anthropic and
/// OpenAI return 408 for client-side timeout indicators that
/// should not be auto-retried (the request was *too slow*; a
/// retry of the same shape will repeat).
pub fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

/// Whether a [`reqwest::Error`] came from a network-level issue
/// that's worth retrying. Connection refused, DNS failure, TLS
/// handshake failure, request timeout — all transient. JSON
/// parse failures or builder errors are not retryable (the
/// request is structurally wrong).
pub fn is_retryable_reqwest_err(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

/// Compute the sleep before the **next** attempt (1-indexed:
/// `attempt = 1` is the wait between the first failure and the
/// first retry).
///
/// Formula: `delay = random(0, min(base * 2^(attempt-1), max))`.
/// "Full jitter" per AWS — reduces thundering herd more than
/// "equal jitter". Returns `Duration::ZERO` when `attempt == 0`.
///
/// Random source: 4 bytes from `getrandom` mapped to a u32, then
/// scaled. Cheap; cryptographic strength is overkill but it's
/// what's already in the dependency graph.
pub fn exp_backoff_with_jitter(attempt: u32, config: &RetryConfig) -> Duration {
    if attempt == 0 {
        return Duration::ZERO;
    }
    // 2^(attempt-1) but saturate so we don't overflow.
    let exp = (attempt - 1).min(20); // 2^20 ≈ 1M ms = 17 min — well past max_delay
    let scale = 1u64 << exp;
    let base_ms = config.base_delay.as_millis() as u64;
    let scaled_ms = base_ms.saturating_mul(scale);
    let max_ms = config.max_delay.as_millis() as u64;
    let cap_ms = scaled_ms.min(max_ms);
    if cap_ms == 0 {
        return Duration::ZERO;
    }
    // Full jitter: uniform random in [0, cap_ms].
    let mut buf = [0u8; 4];
    if getrandom::getrandom(&mut buf).is_err() {
        // Fallback: half the cap (deterministic fallback is OK
        // — getrandom failures are extraordinarily rare).
        return Duration::from_millis(cap_ms / 2);
    }
    let r = u32::from_le_bytes(buf);
    let jittered = (r as u64) % (cap_ms + 1);
    Duration::from_millis(jittered)
}

/// Parse a `Retry-After` header value, capped at `config.max_delay`.
/// Accepts either an integer-seconds form ("Retry-After: 30") or
/// an HTTP-date form ("Retry-After: Wed, 21 Oct 2015 07:28:00 GMT").
/// We only handle the seconds form — the date form is rare for
/// LLM APIs and parsing it would pull in a date dep; on a failed
/// parse we return None and the caller falls back to exponential
/// backoff.
pub fn parse_retry_after(header: Option<&str>, max_delay: Duration) -> Option<Duration> {
    let raw = header?.trim();
    let secs: u64 = raw.parse().ok()?;
    let d = Duration::from_secs(secs);
    Some(d.min(max_delay))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_status_classifies_correctly() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(502));
        assert!(is_retryable_status(503));
        assert!(is_retryable_status(504));
        assert!(is_retryable_status(599));
        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(403));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(408)); // documented exclusion
        assert!(!is_retryable_status(422));
    }

    #[test]
    fn backoff_zero_attempt_returns_zero() {
        let d = exp_backoff_with_jitter(0, &RetryConfig::default());
        assert_eq!(d, Duration::ZERO);
    }

    #[test]
    fn backoff_caps_at_max_delay() {
        // attempt=20 should be capped at max_delay even with the
        // full-jitter random factor.
        let cfg = RetryConfig {
            max_retries: 100,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
        };
        for _ in 0..50 {
            let d = exp_backoff_with_jitter(20, &cfg);
            assert!(d <= cfg.max_delay, "got {d:?}, cap {:?}", cfg.max_delay);
        }
    }

    #[test]
    fn backoff_grows_on_average() {
        // Statistical sanity: average over many samples of
        // attempt=4 should exceed average of attempt=1, given
        // full-jitter random in [0, cap].
        let cfg = RetryConfig {
            max_retries: 10,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(60),
        };
        let mean = |attempt: u32, n: u32| -> u128 {
            let total: u128 = (0..n)
                .map(|_| exp_backoff_with_jitter(attempt, &cfg).as_millis())
                .sum();
            total / n as u128
        };
        let m1 = mean(1, 200);
        let m4 = mean(4, 200);
        assert!(
            m4 > m1,
            "mean attempt=4 ({m4}ms) should exceed attempt=1 ({m1}ms)"
        );
    }

    #[test]
    fn backoff_zero_max_returns_zero() {
        // Pathological config (RetryConfig::none) must produce
        // zero sleep so callers can disable retries cleanly.
        let cfg = RetryConfig::none();
        let d = exp_backoff_with_jitter(1, &cfg);
        assert_eq!(d, Duration::ZERO);
    }

    #[test]
    fn parse_retry_after_seconds_form() {
        let cap = Duration::from_secs(30);
        assert_eq!(
            parse_retry_after(Some("5"), cap),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            parse_retry_after(Some("  10  "), cap),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn parse_retry_after_caps_at_max() {
        let cap = Duration::from_secs(10);
        assert_eq!(parse_retry_after(Some("999999"), cap), Some(cap));
    }

    #[test]
    fn parse_retry_after_returns_none_on_garbage() {
        let cap = Duration::from_secs(10);
        assert_eq!(parse_retry_after(None, cap), None);
        // Date form unsupported → None (caller falls back).
        assert_eq!(
            parse_retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT"), cap),
            None
        );
        assert_eq!(parse_retry_after(Some("not a number"), cap), None);
    }

    #[test]
    fn config_default_sane() {
        let c = RetryConfig::default();
        assert_eq!(c.max_retries, 3);
        assert_eq!(c.base_delay, Duration::from_millis(500));
        assert_eq!(c.max_delay, Duration::from_secs(10));
    }

    #[test]
    fn config_none_disables_retries() {
        let c = RetryConfig::none();
        assert_eq!(c.max_retries, 0);
    }
}
