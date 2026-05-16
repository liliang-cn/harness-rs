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
    pub message:   String,
    pub transient: bool,
}
impl Retryable {
    pub fn transient(msg: impl Into<String>) -> Self { Self { message: msg.into(), transient: true } }
    pub fn permanent(msg: impl Into<String>) -> Self { Self { message: msg.into(), transient: false } }
}

/// Run `f` up to 4 times (1 initial + 3 retries) on transient failures.
///
/// `label` shows up in tracing for grep-ability.
pub async fn with_retry<F, Fut, T>(label: &'static str, mut f: F) -> Result<T, String>
where
    F:   FnMut() -> Fut,
    Fut: Future<Output = Result<T, Retryable>>,
{
    let mut attempt = 0u32;
    let mut delay   = Duration::from_secs(1);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

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
        }).await;
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
                if n < 3 { Err(Retryable::transient(format!("flap {n}"))) }
                else { Ok(42) }
            }
        }).await;
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
        }).await;
        assert!(r.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 4); // 1 initial + 3 retries
    }
}
