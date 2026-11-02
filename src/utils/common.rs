use anyhow::{Result, bail};
use regex::Regex;
use std::sync::LazyLock;
use url::Url;

pub static MANGA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/manga/[^/]+\.\d+$").unwrap());
pub static CHAPTER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(/manga/[^/]+\.\d+)/c([^/]+)$").unwrap());

/// Normalizes and extracts the canonical MangaKatana series URL from a user-provided input.
///
/// If input is an absolute URL pointing to MangaKatana (either the main series page or a
/// chapter URL), returns `Some(canonical_url)`. Otherwise returns `None`.
pub fn series_url(value: &str) -> Result<Option<String>> {
    if !value.starts_with("http://") && !value.starts_with("https://") {
        return Ok(None);
    }
    let parsed = Url::parse(value)?;
    let configured_base = Url::parse(&format!("{}/", *crate::config::BASE_URL))?;
    if parsed.host_str() != Some("mangakatana.com")
        && parsed.host_str() != Some("www.mangakatana.com")
        && parsed.host_str() != configured_base.host_str()
    {
        bail!("URL must belong to mangakatana.com");
    }

    let path = parsed.path().trim_end_matches('/');

    let path = if let Some(caps) = CHAPTER_RE.captures(path) {
        caps.get(1).map_or(path, |m| m.as_str())
    } else if let Some(stripped) = path.strip_suffix("/download") {
        stripped
    } else {
        path
    };

    if !MANGA_RE.is_match(path) {
        bail!("Expected MangaKatana manga or chapter URL");
    }

    Ok(Some(configured_base.join(path)?.to_string()))
}

/// Acquires a lock on a mutex, recovering gracefully if another thread panicked while holding it.
pub fn lock_mutex<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Escapes XML special characters and strips invalid XML 1.0 control characters for inclusion in `ComicInfo.xml`.
pub fn escape_xml(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            c if c == '\t' || c == '\n' || c == '\r' || !c.is_control() => escaped.push(c),
            _ => {} // Drop invalid XML 1.0 control characters
        }
    }
    escaped
}

/// Truncates a string to at most `max_chars` unicode characters, appending `"..."` if truncated.
pub fn truncate_str(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        None => s.to_string(),
        Some((idx, _)) => format!("{}...", &s[..idx]),
    }
}
