use super::parser::extract_image_urls;
use crate::rate_limit::{self, RateLimitError, is_rate_limited_response};
use anyhow::Result;
use std::time::Duration;
use ureq::Agent;

pub type Client = Agent;

pub fn build_client() -> Result<Client> {
    let config = ureq::config::Config::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124 Safari/537.36")
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .save_redirect_history(true)
        .max_redirects(10)
        .build();

    Ok(config.new_agent())
}

/// Mirror query suffixes probed sequentially on MangaKatana.
const MIRROR_SUFFIXES: [&str; 3] = ["", "?sv=mk", "?sv=3"];

/// Fetches image URLs for a chapter by probing MangaKatana server mirrors (`""`, `"?sv=mk"`, `"?sv=3"`).
pub fn chapter_images(client: &Client, chapter_url: &str) -> Result<Vec<String>> {
    let mut last_retry_after = None;
    let mut last_err_msg = String::new();

    for suffix in MIRROR_SUFFIXES {
        let url = format!("{}{}", chapter_url, suffix);
        let mut res = match client.get(&url).call() {
            Ok(r) => r,
            Err(e) => {
                log::warn!("Failed request to {}: {}", url, e);
                last_err_msg = format!("request failed: {}", e);
                continue;
            }
        };

        let status = res.status();
        if let Some(retry_after) = rate_limit::parse_retry_after(res.headers()) {
            last_retry_after = Some(retry_after);
        }

        let text = match res.body_mut().read_to_string() {
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
