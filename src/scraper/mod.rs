pub mod client;
pub mod parser;

pub use client::{build_client, chapter_images};
pub use parser::{extract_image_urls, parse_manga_page, parse_search_results};

use crate::{
    models::{Chapter, Manga},
    rate_limit::{RateLimitError, is_rate_limited_response},
};
use anyhow::{Result, bail};
use reqwest::blocking::Client;
use url::Url;

/// Searches MangaKatana for manga matching the given query keyword.
///
/// Returns a list of matching `Manga` entries, or an error if rate-limited or unavailable.
pub fn search_manga(client: &Client, query: &str) -> Result<Vec<Manga>> {
    let base_url = format!("{}/", *crate::config::BASE_URL);
    let res = client
        .get(Url::parse_with_params(&base_url, &[("search", query)])?)
        .send()?;
    let status = res.status();
    let url = res.url().clone();
    let path = url.path().trim_end_matches('/');

    let text = res.text()?;
    if is_rate_limited_response(status, &text) {
        return Err(RateLimitError::new("Search blocked or rate limited").into());
    }
    if !status.is_success() {
        bail!(
            "HTTP error {} searching MangaKatana for '{}'",
            status,
            query
        );
    }

    parse_search_results(&text, path, &url, query, &base_url)
}

/// Fetches manga metadata and the complete sorted list of chapters from a series URL.
pub fn manga_chapters(client: &Client, url: &str) -> Result<(Manga, Vec<Chapter>)> {
    let res = client.get(url).send()?;
    let status = res.status();
    let effective_url = res.url().clone();
    let text = res.text()?;
    if is_rate_limited_response(status, &text) {
        return Err(RateLimitError::new("Manga page blocked or rate limited").into());
    }
    if !status.is_success() {
        bail!("HTTP error {} fetching manga from {}", status, url);
    }

    let base_url = format!("{}/", *crate::config::BASE_URL);
    parse_manga_page(&text, &effective_url, url, &base_url)
}
