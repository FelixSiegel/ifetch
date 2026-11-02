use super::parser::extract_image_urls;
use crate::rate_limit::{self, RateLimitError, is_rate_limited_response};
use anyhow::{Context, Result};
use reqwest::{
    blocking::Client,
    header::{HeaderMap, HeaderValue, USER_AGENT},
};
use std::time::Duration;

pub fn build_client() -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124 Safari/537.36",
        ),
    );

    Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(30))
        .build()
        .context("Failed to build HTTP client")
}

/// Fetches image URLs for a chapter by probing MangaKatana server mirrors (`""`, `"?sv=mk"`, `"?sv=3"`).
pub fn chapter_images(client: &Client, chapter_url: &str) -> Result<Vec<String>> {
    let mut last_retry_after = None;
    let mut last_err_msg = String::new();

    for suffix in ["", "?sv=mk", "?sv=3"] {
        let url = format!("{}{}", chapter_url, suffix);
        let res = match client.get(&url).send() {
            Ok(r) => r,
            Err(e) => {
                log::warn!("Failed request to {}: {}", url, e);
                last_err_msg = format!("request failed: {}", e);
                continue;
            }
        };

        let status = res.status();
        let retry_after = rate_limit::parse_retry_after(res.headers());
        if retry_after.is_some() {
            last_retry_after = retry_after;
        }

        let text = match res.text() {
            Ok(t) => t,
            Err(_) => {
                last_err_msg = format!("failed reading body from {}", url);
                continue;
            }
        };

        if !is_rate_limited_response(status, &text) {
            if let Some(urls) = extract_image_urls(&text) {
                return Ok(urls);
            }
            last_err_msg = format!("HTTP {} from {} contained no image array", status, url);
        } else {
            last_err_msg = format!(
                "HTTP {} ({} bytes) from {} (rate limit or down)",
                status,
                text.len(),
                url
            );
        }
    }

    Err(RateLimitError::with_retry_after(
        format!(
            "All mirrors failed to provide images (last: {})",
            last_err_msg
        ),
        last_retry_after,
    )
    .into())
}
