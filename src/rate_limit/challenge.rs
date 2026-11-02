use reqwest::StatusCode;
use std::time::Duration;

const CHALLENGE_MARKERS: &[&str] = &[
    "cf-chl",
    "challenge-platform",
    "turnstile",
    "Just a moment...",
    "cf-browser-verification",
    "cf_chl_opt",
];

const STATUS_MARKERS: &[&str] = &["cloudflare", "Cloudflare", "cf-ray", "Attention Required!"];

/// Typed error representing an HTTP rate limit or anti-bot block.
#[derive(Debug, Clone)]
pub struct RateLimitError {
    pub reason: String,
    pub retry_after: Option<Duration>,
}

impl RateLimitError {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            retry_after: None,
        }
    }

    pub fn with_retry_after(reason: impl Into<String>, retry_after: Option<Duration>) -> Self {
        Self {
            reason: reason.into(),
            retry_after,
        }
    }
}

impl std::fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl std::error::Error for RateLimitError {}

/// Parses the standard HTTP `Retry-After` header (in seconds) if present in response headers.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Extracts an explicit `Retry-After` duration if present in the error chain.
pub fn extract_retry_after(e: &anyhow::Error) -> Option<Duration> {
    e.chain()
        .find_map(|c| c.downcast_ref::<RateLimitError>())
        .and_then(|r| r.retry_after)
}

/// Checks whether an HTTP response indicates a rate limit or anti-bot challenge.
pub fn is_rate_limited_response(status: StatusCode, body: &str) -> bool {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return true;
    }

    CHALLENGE_MARKERS.iter().any(|&m| body.contains(m))
        || (matches!(
            status,
            StatusCode::FORBIDDEN | StatusCode::SERVICE_UNAVAILABLE
        ) && STATUS_MARKERS.iter().any(|&m| body.contains(m)))
}

#[rustfmt::skip]
/// Checks whether an error represents an HTTP rate limit or network block.
pub fn is_rate_limit_error(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause.downcast_ref::<RateLimitError>().is_some() ||
        cause.downcast_ref::<reqwest::Error>().is_some_and(
            |req_err| req_err.status() == Some(StatusCode::TOO_MANY_REQUESTS))
    })
}
