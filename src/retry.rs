use std::future::Future;
use std::time::Duration;

use reqwest::header::RETRY_AFTER;
use reqwest::{Response, StatusCode};

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: usize,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl RetryPolicy {
    pub const fn provider_default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(10),
        }
    }
}

pub async fn retry_async<T, E, Fut, Op, RetryAfter, IsRetryable>(
    policy: &RetryPolicy,
    mut op: Op,
    retry_after: RetryAfter,
    is_retryable: IsRetryable,
    label: &str,
) -> Result<T, E>
where
    Op: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    RetryAfter: Fn(&E) -> Option<Duration>,
    IsRetryable: Fn(&E) -> bool,
    E: std::fmt::Display,
{
    let mut attempt = 1;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if attempt >= policy.max_attempts || !is_retryable(&error) {
                    return Err(error);
                }
                let delay = retry_after(&error)
                    .unwrap_or_else(|| backoff_delay(policy, attempt))
                    .min(policy.max_delay);
                log::warn!(
                    "{} attempt {} failed: {}; retrying in {:?}",
                    label,
                    attempt,
                    error,
                    delay
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

pub fn transient_reqwest_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

pub fn retry_after_from_response(response: &Response) -> Option<Duration> {
    response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
}

pub fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn backoff_delay(policy: &RetryPolicy, attempt: usize) -> Duration {
    let multiplier = 1u32 << attempt.saturating_sub(1).min(5);
    let base = policy.base_delay.saturating_mul(multiplier);
    let jitter = Duration::from_millis(jitter_millis());
    base.saturating_add(jitter)
}

fn jitter_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_nanos()) % 250)
        .unwrap_or(0)
}
