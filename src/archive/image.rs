use crate::{
    rate_limit::{self, RateLimitError, is_rate_limit_error, is_rate_limited_response},
    scraper::Client,
};
use anyhow::{Context, Result, bail};
use http::StatusCode;
use std::{thread, time::Duration};

/// Attempts a single HTTP request to fetch an image, validating headers, status, and payload.
fn try_fetch_single_image(
    client: &Client,
    img_url: &str,
    referer: &str,
) -> Result<(Vec<u8>, String)> {
    let mut res = client.get(img_url).header("Referer", referer).call()?;

    let status = res.status();
    let retry_after = rate_limit::parse_retry_after(res.headers());

    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(RateLimitError::with_retry_after(
            format!(
                "Image CDN returned HTTP 429 Too Many Requests from {}",
                img_url
            ),
            retry_after,
        )
        .into());
    }

    if !status.is_success() {
        let text = res.body_mut().read_to_string().unwrap_or_default();
        if is_rate_limited_response(status, &text) {
            return Err(RateLimitError::with_retry_after(
                format!(
                    "Image CDN challenge or block (HTTP {}) from {}",
                    status, img_url
                ),
                retry_after,
            )
            .into());
        }
        bail!("HTTP error {} downloading image from {}", status, img_url);
    }

    let ct = res
        .headers()
        .get("Content-Type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();

    if ct.contains("text/html") {
        let text = res.body_mut().read_to_string().unwrap_or_default();
        if is_rate_limited_response(status, &text) {
            return Err(RateLimitError::with_retry_after(
                format!("Image CDN returned challenge page from {}", img_url),
                retry_after,
            )
            .into());
        }
        bail!("Expected image but received HTML from {}", img_url);
    }

    let mut data = match res.body().content_length() {
        Some(len) if len < 50 * 1024 * 1024 => Vec::with_capacity(len as usize),
        _ => Vec::new(),
    };
    std::io::copy(&mut res.body_mut().as_reader(), &mut data)?;
    if data.is_empty() {
        bail!("Received empty image data (0 bytes) from {}", img_url);
    }

    Ok((data, ct))
}

/// Fetches a single image with retries, exponential backoff, and Content-Type inspection.
pub fn fetch_single_image(
    client: &Client,
    img_url: &str,
    referer: &str,
    max_attempts: usize,
) -> Result<(Vec<u8>, String)> {
    for attempt in 1..=max_attempts {
        match try_fetch_single_image(client, img_url, referer) {
            Ok(val) => return Ok(val),
            Err(e) if is_rate_limit_error(&e) => return Err(e),
            Err(e) if attempt < max_attempts => {
                log::warn!(
                    "Retrying image ({}) after error: {} (attempt {}/{})",
                    img_url,
                    e,
                    attempt,
                    max_attempts
                );
                thread::sleep(Duration::from_millis(500 * attempt as u64));
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "Failed to download image ({}) after {} attempts",
                        img_url, max_attempts
                    )
                });
            }
        }
    }

    unreachable!()
}
