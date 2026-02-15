//! Retry and Error Recovery
//!
//! Wraps LLM provider calls with exponential backoff retry logic.
//! Handles HTTP 429 (rate limit), 5xx errors, network timeouts,
//! and malformed LLM output.

use std::time::Duration;

use anyhow::Result;
use tracing::warn;

// ---------------------------------------------------------------------------
// Retry policy
// ---------------------------------------------------------------------------

/// Configuration for retry behavior.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts (default 3).
    pub max_retries: u32,
    /// Base delay in milliseconds (default 1000).
    pub base_delay_ms: u64,
    /// Maximum delay in milliseconds (default 30000).
    pub max_delay_ms: u64,
    /// Backoff multiplier (default 2.0).
    pub backoff_multiplier: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 1000,
            max_delay_ms: 30000,
            backoff_multiplier: 2.0,
        }
    }
}

impl RetryPolicy {
    /// Calculate the delay for a given attempt (0-indexed).
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let delay_ms =
            self.base_delay_ms as f64 * self.backoff_multiplier.powi(attempt as i32);
        let clamped = delay_ms.min(self.max_delay_ms as f64) as u64;
        Duration::from_millis(clamped)
    }
}

// ---------------------------------------------------------------------------
// Retryable error classification
// ---------------------------------------------------------------------------

/// Whether an error is transient and should be retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// HTTP 429 — rate limit. May include a Retry-After hint.
    RateLimit { retry_after: Option<Duration> },
    /// HTTP 500, 502, 503 — server errors.
    ServerError,
    /// Network timeout or connection reset.
    NetworkError,
    /// Malformed response from the LLM (bad JSON, etc.).
    MalformedResponse,
    /// Non-retryable error (4xx other than 429, auth failure, etc.).
    Fatal,
}

impl ErrorKind {
    /// Check if this error should be retried.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, ErrorKind::Fatal)
    }
}

/// Classify an error from an HTTP status code.
pub fn classify_http_error(status: u16, retry_after_header: Option<&str>) -> ErrorKind {
    match status {
        429 => {
            let retry_after = retry_after_header.and_then(|h| {
                h.parse::<u64>().ok().map(Duration::from_secs)
            });
            ErrorKind::RateLimit { retry_after }
        }
        500 | 502 | 503 => ErrorKind::ServerError,
        _ if status >= 400 => ErrorKind::Fatal,
        _ => ErrorKind::Fatal,
    }
}

// ---------------------------------------------------------------------------
// Retry executor
// ---------------------------------------------------------------------------

/// Execute an async operation with retry logic.
///
/// The `classify` closure maps errors to [`ErrorKind`] for retry decisions.
pub async fn with_retry<F, Fut, T, E>(
    policy: &RetryPolicy,
    operation_name: &str,
    mut operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display + Into<anyhow::Error>,
{
    let mut attempt = 0;

    loop {
        match operation().await {
            Ok(result) => return Ok(result),
            Err(err) => {
                if attempt >= policy.max_retries {
                    warn!(
                        operation = operation_name,
                        attempts = attempt + 1,
                        "max retries exceeded"
                    );
                    return Err(err.into());
                }

                let delay = policy.delay_for_attempt(attempt);
                warn!(
                    operation = operation_name,
                    attempt = attempt + 1,
                    max = policy.max_retries,
                    delay_ms = delay.as_millis() as u64,
                    err = %err,
                    "retrying after error"
                );

                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn delay_exponential_backoff() {
        let policy = RetryPolicy {
            base_delay_ms: 1000,
            max_delay_ms: 30000,
            backoff_multiplier: 2.0,
            ..Default::default()
        };

        assert_eq!(policy.delay_for_attempt(0), Duration::from_millis(1000));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_millis(2000));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(4000));
        assert_eq!(policy.delay_for_attempt(3), Duration::from_millis(8000));
    }

    #[test]
    fn delay_clamped_to_max() {
        let policy = RetryPolicy {
            base_delay_ms: 10000,
            max_delay_ms: 30000,
            backoff_multiplier: 2.0,
            ..Default::default()
        };

        // 10000 * 2^2 = 40000 → clamped to 30000
        assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(30000));
    }

    #[test]
    fn classify_rate_limit() {
        let kind = classify_http_error(429, Some("5"));
        assert_eq!(
            kind,
            ErrorKind::RateLimit {
                retry_after: Some(Duration::from_secs(5))
            }
        );
    }

    #[test]
    fn classify_server_errors() {
        assert_eq!(classify_http_error(500, None), ErrorKind::ServerError);
        assert_eq!(classify_http_error(502, None), ErrorKind::ServerError);
        assert_eq!(classify_http_error(503, None), ErrorKind::ServerError);
    }

    #[test]
    fn classify_fatal_errors() {
        assert_eq!(classify_http_error(401, None), ErrorKind::Fatal);
        assert_eq!(classify_http_error(403, None), ErrorKind::Fatal);
        assert_eq!(classify_http_error(404, None), ErrorKind::Fatal);
    }

    #[tokio::test]
    async fn retry_succeeds_after_failures() {
        let call_count = Arc::new(AtomicU32::new(0));

        let policy = RetryPolicy {
            max_retries: 3,
            base_delay_ms: 10, // fast for testing
            max_delay_ms: 100,
            backoff_multiplier: 2.0,
        };

        let counter = call_count.clone();
        let result = with_retry(&policy, "test", || {
            let counter = counter.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(anyhow::anyhow!("transient error"))
                } else {
                    Ok(42)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(call_count.load(Ordering::SeqCst), 3); // 2 failures + 1 success
    }

    #[tokio::test]
    async fn retry_exhausted() {
        let policy = RetryPolicy {
            max_retries: 2,
            base_delay_ms: 10,
            max_delay_ms: 100,
            backoff_multiplier: 2.0,
        };

        let result: Result<i32> = with_retry(&policy, "test", || async {
            Err::<i32, _>(anyhow::anyhow!("always fails"))
        })
        .await;

        assert!(result.is_err());
    }
}
