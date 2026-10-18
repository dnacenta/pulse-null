use std::sync::Arc;
use std::time::Instant;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use tokio::sync::Mutex;

/// Simple token-bucket rate limiter. Shared across all requests.
///
/// Tokens and last-refill timestamp are stored in a single mutex to
/// eliminate any theoretical ordering / deadlock risk.
pub struct RateLimiter {
    state: Mutex<(f64, Instant)>,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
}

impl RateLimiter {
    /// Create a new rate limiter.
    /// `max_burst` — maximum requests allowed in a burst.
    /// `per_second` — sustained rate (requests per second).
    pub fn new(max_burst: u32, per_second: f64) -> Self {
        Self {
            state: Mutex::new((max_burst as f64, Instant::now())),
            max_tokens: max_burst as f64,
            refill_rate: per_second,
        }
    }

    /// Take one token if there is one; otherwise say how long until the
    /// next one (`None` when the bucket never refills).
    pub(crate) async fn try_acquire_or_wait(&self) -> Result<(), Option<std::time::Duration>> {
        let mut state = self.state.lock().await;
        let (ref mut tokens, ref mut last_refill) = *state;
        let now = Instant::now();
        let elapsed = now.duration_since(*last_refill);
        *tokens = (*tokens + elapsed.as_secs_f64() * self.refill_rate).min(self.max_tokens);
        *last_refill = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            return Ok(());
        }
        if self.refill_rate <= 0.0 {
            return Err(None);
        }
        Err(Some(std::time::Duration::from_secs_f64(
            (1.0 - *tokens) / self.refill_rate,
        )))
    }

    /// Take one token if there is one.
    async fn try_acquire(&self) -> bool {
        self.try_acquire_or_wait().await.is_ok()
    }
}

/// Rate limiting middleware. Returns 429 when limit exceeded.
/// Skips /health endpoint.
pub async fn rate_limit(
    limiter: axum::extract::State<Arc<RateLimiter>>,
    req: Request,
    next: Next,
) -> Result<Response, (StatusCode, &'static str)> {
    // Skip rate limiting for health checks
    if req.uri().path() == "/health" {
        return Ok(next.run(req).await);
    }

    if limiter.try_acquire().await {
        Ok(next.run(req).await)
    } else {
        Err((StatusCode::TOO_MANY_REQUESTS, "Rate limit exceeded"))
    }
}

/// Default rate limiter: 10 burst, 2 per second.
pub fn default_limiter() -> Arc<RateLimiter> {
    Arc::new(RateLimiter::new(10, 2.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_allows_within_burst() {
        let limiter = RateLimiter::new(3, 1.0);
        assert!(limiter.try_acquire().await);
        assert!(limiter.try_acquire().await);
        assert!(limiter.try_acquire().await);
    }

    #[tokio::test]
    async fn test_rejects_over_burst() {
        let limiter = RateLimiter::new(2, 0.0); // no refill
        assert!(limiter.try_acquire().await);
        assert!(limiter.try_acquire().await);
        assert!(!limiter.try_acquire().await);
    }

    #[tokio::test]
    async fn an_empty_bucket_says_how_long_to_wait() {
        let limiter = RateLimiter::new(1, 4.0);
        assert_eq!(limiter.try_acquire_or_wait().await, Ok(()));
        let wait = limiter.try_acquire_or_wait().await.unwrap_err().unwrap();
        assert!(
            wait <= Duration::from_millis(250) && wait > Duration::ZERO,
            "{wait:?}"
        );
        let never = RateLimiter::new(1, 0.0);
        assert_eq!(never.try_acquire_or_wait().await, Ok(()));
        assert_eq!(never.try_acquire_or_wait().await, Err(None));
    }

    #[tokio::test]
    async fn test_refills_over_time() {
        let limiter = RateLimiter::new(1, 100.0); // fast refill for testing
        assert!(limiter.try_acquire().await);
        assert!(!limiter.try_acquire().await);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(limiter.try_acquire().await);
    }
}
