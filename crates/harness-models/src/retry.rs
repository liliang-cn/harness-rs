//! Shared retry-with-backoff helper for the model adapters in this crate.
//!
//! Why: a single transient `reqwest` blip (connection reset, partial body,
//! 502/503 from the gateway, 429 rate-limit) was killing entire agent runs.
//! Now adapters classify each failure as **transient** (retry with exponential
//! backoff) or **permanent** (propagate immediately).
//!
//! Policy (intentionally not configurable yet — keep small until pressured):
//! - up to 3 retries on transient errors
//! - delays: 1s → 2s → 4s, capped at 4s
//! - simple, not jittered — fine for solo-agent workloads
//! - permanent errors never retry

use std::future::Future;
use std::time::Duration;

/// Carry the "is this worth retrying?" bit alongside the error message.
#[derive(Debug)]
pub struct Retryable {
    pub message: String,
    pub transient: bool,
}
impl Retryable {
    pub fn transient(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            transient: true,
        }
    }
    pub fn permanent(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            transient: false,
        }
    }
}

/// Run `f` up to 4 times (1 initial + 3 retries) on transient failures.
///
/// `label` shows up in tracing for grep-ability.
pub async fn with_retry<F, Fut, T>(label: &'static str, mut f: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Retryable>>,
{
    let mut attempt = 0u32;
    let mut delay = Duration::from_secs(1);
    loop {
        attempt += 1;
        match f().await {
            Ok(v) => {
                if attempt > 1 {
                    tracing::info!(label, attempt, "✓ recovered after retry");
                }
                return Ok(v);
            }
            Err(e) if e.transient && attempt < 4 => {
                tracing::warn!(label, attempt, delay_ms = delay.as_millis() as u64, reason = %e.message,
                    "transient failure, retrying");
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay * 2, Duration::from_secs(4));
            }
            Err(e) => {
                if e.transient {
                    tracing::error!(label, attempt, reason = %e.message, "transient failure, giving up");
                } else {
                    tracing::error!(label, attempt, reason = %e.message, "permanent failure");
                }
                return Err(e.message);
            }
        }
    }
}

/// Like [`with_retry`], but preserves the caller's error type.
///
/// [`with_retry`] collapses failures to `String`, which is fine when the only
/// question left is "what went wrong". The image and speech adapters need more:
/// their callers branch on `RateLimited` vs `Provider`, and re-deriving that
/// distinction by pattern-matching an error message afterwards is exactly the
/// kind of stringly-typed guessing that goes wrong silently. So the classifier
/// is passed in and the error type survives.
///
/// Same policy as [`with_retry`]: 1 initial attempt + 3 retries, 1s → 2s → 4s.
pub async fn with_retry_typed<F, Fut, T, E>(
    label: &'static str,
    is_transient: impl Fn(&E) -> bool,
    mut f: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut attempt = 0u32;
    let mut delay = Duration::from_secs(1);
    loop {
        attempt += 1;
        match f().await {
            Ok(v) => {
                if attempt > 1 {
                    tracing::info!(label, attempt, "✓ recovered after retry");
                }
                return Ok(v);
            }
            Err(e) if is_transient(&e) && attempt < 4 => {
                tracing::warn!(label, attempt, delay_ms = delay.as_millis() as u64, reason = %e,
                    "transient failure, retrying");
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay * 2, Duration::from_secs(4));
            }
            Err(e) => {
                tracing::error!(label, attempt, reason = %e, "giving up");
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn permanent_does_not_retry() {
        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let r = with_retry("test:perm", || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(Retryable::permanent("nope"))
            }
        })
        .await;
        assert!(r.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn transient_retries_then_succeeds() {
        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        // Use very short delays for the test — override is via the function
        // body's `tokio::time::pause()` would help but we just live with 1s+2s
        // since with_retry waits real time. Skip; just verify count.
        let r = with_retry("test:flap", || {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 3 {
                    Err(Retryable::transient(format!("flap {n}")))
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(r.unwrap(), 42);
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn transient_gives_up_after_3_retries() {
        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let r: Result<(), _> = with_retry("test:max", || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(Retryable::transient("always"))
            }
        })
        .await;
        assert!(r.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 4); // 1 initial + 3 retries
    }

    #[derive(Debug, PartialEq)]
    enum E {
        Limited,
        Fatal,
    }
    impl std::fmt::Display for E {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{self:?}")
        }
    }

    #[tokio::test]
    async fn typed_retry_preserves_the_error_variant() {
        // The whole point: after exhausting retries the caller still gets
        // `Limited`, not a string it has to re-parse.
        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let r: Result<(), E> = with_retry_typed(
            "test:typed",
            |e| matches!(e, E::Limited),
            || {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err(E::Limited)
                }
            },
        )
        .await;
        assert_eq!(r.unwrap_err(), E::Limited);
        assert_eq!(count.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn typed_retry_skips_non_transient() {
        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let r: Result<(), E> = with_retry_typed(
            "test:typed-perm",
            |e| matches!(e, E::Limited),
            || {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err(E::Fatal)
                }
            },
        )
        .await;
        assert_eq!(r.unwrap_err(), E::Fatal);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
