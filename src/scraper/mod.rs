pub mod client;
pub mod parser;

pub use client::{Client, build_client, chapter_images};
pub use parser::{
    extract_image_urls, parse_book_list, parse_hot_manga, parse_latest_manga,
    parse_manga_items_from_container, parse_manga_page, parse_search_results,
};

use crate::{
    models::{Chapter, Manga},
    rate_limit::{RateLimitError, is_rate_limited_response},
};
use anyhow::{Result, bail};
use std::fmt;
use ureq::ResponseExt;
use url::Url;

/// Error indicating that a manga series was not found upstream (HTTP 404 or missing page).
#[derive(Debug)]
pub struct MangaNotFoundError {
    pub message: String,
}

impl fmt::Display for MangaNotFoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Manga not found: {}", self.message)
    }
}

impl std::error::Error for MangaNotFoundError {}

/// Returns true if the error represents an upstream 404 / manga not found condition.
pub fn is_not_found_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<MangaNotFoundError>().is_some()
}

/// Searches MangaKatana for manga matching the given query keyword.
///
/// Returns a list of matching `Manga` entries, or an error if rate-limited or unavailable.
pub fn search_manga(client: &Client, query: &str) -> Result<Vec<Manga>> {
    let base_url = format!("{}/", *crate::config::BASE_URL);
    let search_url = Url::parse_with_params(&base_url, &[("search", query)])?;
    let mut res = client.get(search_url.as_str()).call()?;
    let status = res.status();
    let effective_uri = res
        .get_redirect_history()
        .and_then(|h| h.last())
        .unwrap_or_else(|| res.get_uri());
    let url = Url::parse(&effective_uri.to_string())?;
    let path = url.path().trim_end_matches('/');

    let text = res.body_mut().read_to_string()?;
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
    let mut res = client.get(url).call()?;
    let status = res.status();
    let effective_uri = res
        .get_redirect_history()
        .and_then(|h| h.last())
        .unwrap_or_else(|| res.get_uri());
    let effective_url = Url::parse(&effective_uri.to_string())?;
    let text = res.body_mut().read_to_string()?;
    if is_rate_limited_response(status, &text) {
        return Err(RateLimitError::new("Manga page blocked or rate limited").into());
    }
    if status == http::StatusCode::NOT_FOUND {
        return Err(MangaNotFoundError {
            message: format!("HTTP 404 at {}", url),
        }
        .into());
    }
    if !status.is_success() {
        bail!("HTTP error {} fetching manga from {}", status, url);
    }

    let base_url = format!("{}/", *crate::config::BASE_URL);
    parse_manga_page(&text, &effective_url, url, &base_url)
}

/// Helper to fetch paginated listings backed by `#book_list` (e.g. `/latest`, `/new-manga`).
fn fetch_book_list(client: &Client, endpoint: &str, page: usize) -> Result<Vec<Manga>> {
    let base_url = format!("{}/", *crate::config::BASE_URL);
    let url = if page <= 1 {
        format!("{}/{}", *crate::config::BASE_URL, endpoint)
    } else {
        format!("{}/{}/page/{}", *crate::config::BASE_URL, endpoint, page)
    };
    let mut res = client.get(&url).call()?;
    let status = res.status();
    let text = res.body_mut().read_to_string()?;
    if is_rate_limited_response(status, &text) {
        return Err(RateLimitError::new(format!("Blocked or rate limited on {}", endpoint)).into());
    }
    if status == http::StatusCode::NOT_FOUND {
        return Ok(Vec::new());
    }
    if !status.is_success() {
        bail!("HTTP error {} fetching from {}", status, url);
    }
    parse_book_list(&text, &base_url)
}

/// Fetches latest updated manga from MangaKatana.
pub fn latest_manga(client: &Client, page: usize) -> Result<Vec<Manga>> {
    fetch_book_list(client, "latest", page)
}

/// Fetches newly added manga from MangaKatana.
pub fn new_manga(client: &Client, page: usize) -> Result<Vec<Manga>> {
    fetch_book_list(client, "new-manga", page)
}

/// Fetches hot/popular manga from MangaKatana.
pub fn popular_manga(client: &Client, page: usize) -> Result<Vec<Manga>> {
    if page > 1 {
        return Ok(Vec::new());
    }
    let base_url = format!("{}/", *crate::config::BASE_URL);
    let url = format!("{}/", *crate::config::BASE_URL);
    let mut res = client.get(&url).call()?;
    let status = res.status();
    let text = res.body_mut().read_to_string()?;
    if is_rate_limited_response(status, &text) {
        return Err(RateLimitError::new("Popular manga blocked or rate limited").into());
    }
    if !status.is_success() {
        bail!("HTTP error {} fetching popular manga from {}", status, url);
    }
    parse_hot_manga(&text, &base_url)
}
