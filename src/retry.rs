//! Opt-in retry/backoff for transient failures: connection errors and a
//! configurable set of "try again" response statuses (429/500/502/503/504
//! by default). Disabled by default on both [`crate::Client`] and a bare
//! request -- silently retrying a non-idempotent request (POST/PATCH) can
//! duplicate whatever side effect it already caused, so retrying those
//! needs a second, explicit opt-in too (see
//! [`RetryPolicy::retry_non_idempotent`]).

use crate::error::Error;
use rusty_http::cookie::parse_http_date;
use rusty_http::{HeaderMap, Method};
use std::time::{Duration, SystemTime};

const DEFAULT_RETRY_STATUSES: [u16; 5] = [429, 500, 502, 503, 504];
const DEFAULT_MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// How long to wait before the next retry attempt.
#[derive(Debug, Clone, Copy)]
pub enum Backoff {
    /// The same delay before every retry.
    Fixed(Duration),
    /// `base * 2^attempt`, capped at `max`. When `jitter` is set, applies
    /// the "full jitter" strategy (a uniform random delay in
    /// `[0, capped_delay)`) so that many clients backing off from the same
    /// failure don't all retry in lockstep.
    Exponential {
        base: Duration,
        max: Duration,
        jitter: bool,
    },
}

impl Backoff {
    pub fn fixed(delay: Duration) -> Self {
        Backoff::Fixed(delay)
    }

    /// Exponential backoff with jitter enabled; see
    /// [`Backoff::exponential_no_jitter`] to disable it.
    pub fn exponential(base: Duration, max: Duration) -> Self {
        Backoff::Exponential {
            base,
            max,
            jitter: true,
        }
    }

    pub fn exponential_no_jitter(base: Duration, max: Duration) -> Self {
        Backoff::Exponential {
            base,
            max,
            jitter: false,
        }
    }

    fn delay_for(&self, attempt: usize) -> Duration {
        match *self {
            Backoff::Fixed(d) => d,
            Backoff::Exponential { base, max, jitter } => {
                // Exponent capped well below where `1u32 << exp` could
                // overflow -- the `.min(max)` below makes any exponent
                // past a handful of attempts equivalent anyway.
                let factor = 1u32 << attempt.min(20);
                let capped = base.checked_mul(factor).unwrap_or(max).min(max);
                if jitter {
                    jittered(capped)
                } else {
                    capped
                }
            }
        }
    }
}

/// A uniform random duration in `[0, max)`. Backoff jitter only needs to
/// avoid a thundering herd, not resist an adversary -- see
/// `crate::rand` for why this isn't a CSPRNG.
fn jittered(max: Duration) -> Duration {
    if max.is_zero() {
        return max;
    }
    let r = crate::rand::next_u64();
    let max_nanos = max.as_nanos().min(u64::MAX as u128) as u64;
    let scaled = ((r as u128 * max_nanos as u128) >> 64) as u64;
    Duration::from_nanos(scaled)
}

/// A configurable retry policy. Opt-in via
/// [`crate::ClientBuilder::retry`]/[`crate::RequestBuilder::retry`] -- no
/// request is retried unless one of those is called.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    max_retries: usize,
    backoff: Backoff,
    retry_statuses: Vec<u16>,
    retry_io_errors: bool,
    retry_non_idempotent: bool,
    respect_retry_after: bool,
    max_retry_after: Duration,
}

impl RetryPolicy {
    /// `max_retries` additional attempts after the first (so
    /// `max_retries = 3` allows up to 4 total requests).
    ///
    /// Defaults: exponential backoff (200ms base, 10s cap, jitter on),
    /// the status set above, connection errors retried, `Retry-After`
    /// respected (capped at 60s so a server can't stall a caller
    /// indefinitely), and only idempotent methods
    /// (GET/HEAD/PUT/DELETE/OPTIONS) retried.
    pub fn new(max_retries: usize) -> Self {
        RetryPolicy {
            max_retries,
            backoff: Backoff::exponential(Duration::from_millis(200), Duration::from_secs(10)),
            retry_statuses: DEFAULT_RETRY_STATUSES.to_vec(),
            retry_io_errors: true,
            retry_non_idempotent: false,
            respect_retry_after: true,
            max_retry_after: DEFAULT_MAX_RETRY_AFTER,
        }
    }

    pub fn backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// Replaces the default retryable status set (429/500/502/503/504)
    /// entirely.
    pub fn retry_statuses<I: IntoIterator<Item = u16>>(mut self, statuses: I) -> Self {
        self.retry_statuses = statuses.into_iter().collect();
        self
    }

    /// Also retries non-idempotent methods (POST/PATCH) on a retryable
    /// status or connection error. Off by default: a retried POST can
    /// duplicate whatever side effect it already caused before the
    /// client saw the failure, unless the caller knows the server
    /// treats it safely (e.g. via an idempotency key).
    pub fn retry_non_idempotent(mut self) -> Self {
        self.retry_non_idempotent = true;
        self
    }

    /// Stops retrying on connection/IO errors -- only retryable statuses
    /// trigger a retry.
    pub fn no_io_retry(mut self) -> Self {
        self.retry_io_errors = false;
        self
    }

    /// Ignores any `Retry-After` response header and always uses the
    /// configured [`Backoff`] instead.
    pub fn ignore_retry_after(mut self) -> Self {
        self.respect_retry_after = false;
        self
    }

    /// Caps how long a server-supplied `Retry-After` may delay a retry
    /// (default 60s).
    pub fn max_retry_after(mut self, max: Duration) -> Self {
        self.max_retry_after = max;
        self
    }

    pub(crate) fn max_retries(&self) -> usize {
        self.max_retries
    }

    pub(crate) fn allows_method(&self, method: &Method) -> bool {
        self.retry_non_idempotent
            || matches!(
                method,
                Method::Get | Method::Head | Method::Put | Method::Delete | Method::Options
            )
    }

    pub(crate) fn should_retry_status(&self, status: u16) -> bool {
        self.retry_statuses.contains(&status)
    }

    pub(crate) fn should_retry_error(&self, error: &Error) -> bool {
        self.retry_io_errors && matches!(error, Error::Io(_))
    }

    /// The server-requested delay from a `Retry-After` response header,
    /// if present, respected, and parseable -- capped at
    /// `max_retry_after`.
    pub(crate) fn retry_after(&self, headers: &HeaderMap) -> Option<Duration> {
        if !self.respect_retry_after {
            return None;
        }
        let raw = headers.get("retry-after")?;
        parse_retry_after(raw).map(|d| d.min(self.max_retry_after))
    }

    pub(crate) fn delay_for(&self, attempt: usize, retry_after: Option<Duration>) -> Duration {
        retry_after.unwrap_or_else(|| self.backoff.delay_for(attempt))
    }
}

/// Parses a `Retry-After` value: either delta-seconds (`Retry-After: 120`)
/// or an HTTP-date (`Retry-After: Fri, 31 Dec 1999 23:59:59 GMT`), per RFC
/// 9110 §10.2.3. Reuses the same IMF-fixdate parser `Set-Cookie`'s
/// `Expires` attribute needs rather than hand-rolling a second one.
fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let deadline = parse_http_date(value)?;
    Some(
        deadline
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_backoff_is_constant() {
        let b = Backoff::fixed(Duration::from_millis(50));
        assert_eq!(b.delay_for(0), Duration::from_millis(50));
        assert_eq!(b.delay_for(5), Duration::from_millis(50));
    }

    #[test]
    fn exponential_backoff_without_jitter_doubles_and_caps() {
        let b = Backoff::exponential_no_jitter(Duration::from_millis(100), Duration::from_secs(1));
        assert_eq!(b.delay_for(0), Duration::from_millis(100));
        assert_eq!(b.delay_for(1), Duration::from_millis(200));
        assert_eq!(b.delay_for(2), Duration::from_millis(400));
        assert_eq!(b.delay_for(10), Duration::from_secs(1));
    }

    #[test]
    fn exponential_backoff_with_jitter_never_exceeds_the_cap() {
        let b = Backoff::exponential(Duration::from_millis(100), Duration::from_secs(1));
        for attempt in 0..10 {
            assert!(b.delay_for(attempt) <= Duration::from_secs(1));
        }
    }

    #[test]
    fn default_policy_retries_io_errors_and_default_statuses() {
        let policy = RetryPolicy::new(3);
        assert!(policy.should_retry_error(&Error::Io(std::io::Error::other("boom"))));
        assert!(policy.should_retry_status(429));
        assert!(policy.should_retry_status(503));
        assert!(!policy.should_retry_status(404));
    }

    #[test]
    fn no_io_retry_disables_connection_error_retries() {
        let policy = RetryPolicy::new(3).no_io_retry();
        assert!(!policy.should_retry_error(&Error::Io(std::io::Error::other("boom"))));
    }

    #[test]
    fn default_policy_only_allows_idempotent_methods() {
        let policy = RetryPolicy::new(3);
        assert!(policy.allows_method(&Method::Get));
        assert!(policy.allows_method(&Method::Put));
        assert!(policy.allows_method(&Method::Delete));
        assert!(!policy.allows_method(&Method::Post));
        assert!(!policy.allows_method(&Method::Patch));
    }

    #[test]
    fn retry_non_idempotent_allows_post_and_patch() {
        let policy = RetryPolicy::new(3).retry_non_idempotent();
        assert!(policy.allows_method(&Method::Post));
        assert!(policy.allows_method(&Method::Patch));
    }

    #[test]
    fn custom_retry_statuses_replace_the_default_set() {
        let policy = RetryPolicy::new(3).retry_statuses([418]);
        assert!(policy.should_retry_status(418));
        assert!(!policy.should_retry_status(503));
    }

    #[test]
    fn parses_delta_seconds_retry_after() {
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("  5  "), Some(Duration::from_secs(5)));
    }

    #[test]
    fn parses_http_date_retry_after_in_the_future() {
        // Far enough out that "now" during the test run can't catch up.
        let d = parse_retry_after("Wed, 01 Jan 2099 00:00:00 GMT").unwrap();
        assert!(d > Duration::from_secs(3600 * 24 * 365));
    }

    #[test]
    fn past_http_date_retry_after_is_zero_not_negative() {
        assert_eq!(
            parse_retry_after("Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn garbage_retry_after_is_ignored() {
        assert_eq!(parse_retry_after("not a date or a number"), None);
    }

    #[test]
    fn max_retry_after_caps_a_large_delta_seconds_value() {
        let policy = RetryPolicy::new(3).max_retry_after(Duration::from_secs(30));
        let mut headers = HeaderMap::new();
        headers.insert("Retry-After", "3600").unwrap();
        assert_eq!(policy.retry_after(&headers), Some(Duration::from_secs(30)));
    }

    #[test]
    fn ignore_retry_after_falls_back_to_backoff() {
        let policy = RetryPolicy::new(3).ignore_retry_after();
        let mut headers = HeaderMap::new();
        headers.insert("Retry-After", "5").unwrap();
        assert_eq!(policy.retry_after(&headers), None);
    }

    #[test]
    fn missing_retry_after_header_returns_none() {
        let policy = RetryPolicy::new(3);
        let headers = HeaderMap::new();
        assert_eq!(policy.retry_after(&headers), None);
    }
}
