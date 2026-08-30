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

/// How many attempts a transient failure gets, and how far the backoff may
/// grow. Defaults to 6 attempts capped at 10s — 1+2+4+8+10, about 25 seconds
/// of tolerance.
///
/// The original policy was 4 attempts capped at 4s, which is seven seconds of
/// network trouble before the call fails. Seven seconds is a reasonable wait
/// for someone staring at a chat box, and a terrible one for an unattended run
/// that has been working for hours: a DNS blip ended a twelve-minute task and
/// took its work with it. Nobody is waiting on a background task, so it should
/// wait out an outage rather than lose everything to one.
///
/// `HARNESS_RETRY_ATTEMPTS` and `HARNESS_RETRY_MAX_DELAY_SECS` override it —
/// a host running long unattended work can afford minutes.
fn retry_budget() -> (u32, Duration) {
    fn env(key: &str, default: u64) -> u64 {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(default)
    }
    (
        env("HARNESS_RETRY_ATTEMPTS", 6) as u32,
        Duration::from_secs(env("HARNESS_RETRY_MAX_DELAY_SECS", 10)),
    )
}

/// Run `f` until it succeeds or the retry budget runs out; see [`retry_budget`].
///
/// `label` shows up in tracing for grep-ability.
pub async fn with_retry<F, Fut, T>(label: &'static str, mut f: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Retryable>>,
{
    let (max_attempts, max_delay) = retry_budget();
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
            Err(e) if e.transient && attempt < max_attempts => {
                tracing::warn!(label, attempt, delay_ms = delay.as_millis() as u64, reason = %e.message,
                    "transient failure, retrying");
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay * 2, max_delay);
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
/// Same policy as [`with_retry`]; see [`retry_budget`].
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
    let (max_attempts, max_delay) = retry_budget();
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
            Err(e) if is_transient(&e) && attempt < max_attempts => {
                tracing::warn!(label, attempt, delay_ms = delay.as_millis() as u64, reason = %e,
                    "transient failure, retrying");
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay * 2, max_delay);
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

    #[tokio::test(start_paused = true)]
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

    // Paused clock: the backoff schedule is the thing under test, and sitting
    // through 1s+2s+4s of it proves nothing that auto-advancing does not.
    #[tokio::test(start_paused = true)]
    async fn a_transient_failure_uses_the_whole_budget_then_gives_up() {
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
        // The budget is configurable, so the test asks for it rather than
        // freezing yesterday's number and failing when it is tuned.
        assert_eq!(count.load(Ordering::SeqCst), retry_budget().0);
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

    #[tokio::test(start_paused = true)]
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
        assert_eq!(count.load(Ordering::SeqCst), retry_budget().0);
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
