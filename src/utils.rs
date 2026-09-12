use anyhow::{Result, bail};
use regex::Regex;
use std::sync::LazyLock;
use url::Url;

static INVALID_CHARS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[<>:"/\\|?*\x00-\x1f]"#).unwrap());
static SPACES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static SERIES_CHAPTER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(/manga/[^/]+\.\d+)/c([^/]+)$").unwrap());
static SERIES_MANGA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/manga/[^/]+\.\d+$").unwrap());

/// Normalizes and extracts the canonical MangaKatana series URL from a user-provided input.
///
/// If input is an absolute URL pointing to MangaKatana (either the main series page or a
/// chapter URL), returns `Some(canonical_url)`. Otherwise returns `None`.
pub fn series_url(value: &str) -> Result<Option<String>> {
    if !value.starts_with("http://") && !value.starts_with("https://") {
        return Ok(None);
    }
    let parsed = Url::parse(value)?;
    if parsed.host_str() != Some("mangakatana.com")
        && parsed.host_str() != Some("www.mangakatana.com")
    {
        bail!("URL must belong to mangakatana.com");
    }

    let path = parsed.path().trim_end_matches('/');

    let path = if let Some(caps) = SERIES_CHAPTER_RE.captures(path) {
        caps.get(1).map_or(path, |m| m.as_str())
    } else if let Some(stripped) = path.strip_suffix("/download") {
        stripped
    } else {
        path
    };

    if !SERIES_MANGA_RE.is_match(path) {
        bail!("Expected MangaKatana manga or chapter URL");
    }

    let base = Url::parse("https://mangakatana.com/")?;
    Ok(Some(base.join(path)?.to_string()))
}

/// Generates a sanitized, zero-padded filename for a chapter CBZ archive.
///
/// For example: `"Initial D - Chapter 001.cbz"`
pub fn chapter_filename(title: &str, number_str: &str, width: usize) -> String {
    let parts: Vec<&str> = number_str.split('.').collect();
    let whole = parts[0];

    let mut padded = String::new();
    if whole.len() < width {
        padded.push_str(&"0".repeat(width - whole.len()));
    }
    padded.push_str(whole);
    if parts.len() > 1 {
        padded.push('.');
        padded.push_str(parts[1]);
    }

    let mut clean_title = INVALID_CHARS_RE.replace_all(title, "").to_string();
    clean_title = SPACES_RE.replace_all(&clean_title, " ").to_string();
    clean_title = clean_title.trim_matches([' ', '.']).to_string();

    if clean_title.is_empty() {
        clean_title = "manga".to_string();
    }

    let mut chars: Vec<char> = clean_title.chars().collect();
    if chars.len() > 100 {
        chars.truncate(100);
        clean_title = chars.into_iter().collect();
        clean_title = clean_title.trim_end_matches([' ', '.']).to_string();
    }

    format!("{} - Chapter {}.cbz", clean_title, padded)
}

/// Computes a filesystem-safe directory name for a given manga title.
pub fn get_folder_name(title: &str) -> String {
    let folder_name = INVALID_CHARS_RE.replace_all(title, "");
    let folder_name = SPACES_RE.replace_all(&folder_name, " ");
    let mut folder_name = folder_name.trim_matches([' ', '.']).to_string();
    if folder_name.is_empty() {
        return "manga".to_string();
    }

    let mut chars: Vec<char> = folder_name.chars().collect();
    if chars.len() > 120 {
        chars.truncate(120);
        folder_name = chars.into_iter().collect();
        folder_name = folder_name.trim_end_matches([' ', '.']).to_string();
    }

    if folder_name.is_empty() {
        "manga".to_string()
    } else {
        folder_name
    }
}

/// Searches for an existing CBZ file for a chapter across multiple possible padding widths.
pub fn find_chapter_cbz(
    manga_dir: &std::path::Path,
    title: &str,
    number_str: &str,
) -> Option<std::path::PathBuf> {
    if !manga_dir.exists() {
        return None;
    }
    for width in [3, 4, 5, 6, 2, 1] {
        let path = manga_dir.join(chapter_filename(title, number_str, width));
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Infers the MIME type string from a filename's extension.
pub fn get_mime_type(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".avif") {
        "image/avif"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else {
        "image/jpeg"
    }
}

/// Determines the file extension (`.jpg`, `.png`, etc.) using HTTP Content-Type headers,
/// magic bytes inspection, or URL path fallback.
pub fn image_extension(content_type: &str, data: &[u8], url_path: &str) -> &'static str {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    match ct.as_str() {
        "image/jpeg" => return ".jpg",
        "image/png" => return ".png",
        "image/webp" => return ".webp",
        "image/avif" => return ".avif",
        "image/gif" => return ".gif",
        _ => {}
    }

    match data {
        d if d.starts_with(b"\xff\xd8\xff") => return ".jpg",
        d if d.starts_with(b"\x89PNG\r\n\x1a\n") => return ".png",
        d if d.starts_with(b"GIF87a") || d.starts_with(b"GIF89a") => return ".gif",
        d if d.starts_with(b"RIFF") => return ".webp",
        d if d.len() >= 12 && (&d[4..12] == b"ftypavif" || &d[4..12] == b"ftypavis") => {
            return ".avif";
        }
        _ => {}
    }

    match url_path {
        p if p.ends_with(".jpg") || p.ends_with(".jpeg") => ".jpg",
        p if p.ends_with(".png") => ".png",
        p if p.ends_with(".webp") => ".webp",
        p if p.ends_with(".gif") => ".gif",
        _ => ".jpg", // Default fallback
    }
}

/// Escapes XML special characters for inclusion in `ComicInfo.xml`.
pub fn escape_xml(text: &str) -> String {
    text.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
}

/// Truncates a string to at most `max_chars` unicode characters, appending `"..."` if truncated.
pub fn truncate_str(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        None => s.to_string(),
        Some((idx, _)) => format!("{}...", &s[..idx]),
    }
}

/// Calculates the zero-padding digit width required for the given list of chapters.
/// Guarantees a minimum width of 3 digits.
pub fn determine_width(chapters: &[crate::models::Chapter]) -> usize {
    // We can not be sure if chapters are sorted, as we use in-official API,
    // so we need to iter instead of calling last or first :C
    let max_num = chapters.iter().map(|c| c.number).max().unwrap_or_default();
    max_num.trunc().to_string().len().max(3)
}

/// Upgrades filenames of previously downloaded chapters to the target padding width if necessary.
pub fn upgrade_padding(
    title: &str,
    chapters: &[crate::models::Chapter],
    dir: &std::path::Path,
    target_width: usize,
) {
    if target_width <= 3 || chapters.is_empty() || !dir.exists() {
        return;
    }

    for chapter in chapters {
        let num_str = chapter.number.to_string();

        let target_name = chapter_filename(title, &num_str, target_width);
        let target_path = dir.join(&target_name);

        if target_path.exists() {
            continue;
        }

        for current_width in 3..target_width {
            let old_name = chapter_filename(title, &num_str, current_width);
            let old_path = dir.join(&old_name);

            if old_path.exists() {
                if let Err(e) = std::fs::rename(&old_path, &target_path) {
                    log::error!(
                        "Failed to rename {} to {} for {}: {}",
                        old_name,
                        target_name,
                        title,
                        e
                    );
                }
                break;
            }
        }
    }
}

/// Scans the manga directory for existing chapters, upgrades filename padding if needed,
/// and partitions the chapters into already downloaded chapters and missing chapters.
pub fn scan_manga_chapters(
    manga_dir: &std::path::Path,
    title: &str,
    chapters: &[crate::models::Chapter],
) -> (
    usize,
    Vec<crate::models::Chapter>,
    Vec<crate::models::Chapter>,
) {
    let width = determine_width(chapters);
    upgrade_padding(title, chapters, manga_dir, width);

    let mut existing = Vec::new();
    let mut missing = Vec::new();
    let dir_exists = manga_dir.exists();

    for chapter in chapters {
        let filename = chapter_filename(title, &chapter.number.to_string(), width);
        if dir_exists && manga_dir.join(&filename).exists() {
            existing.push(chapter.clone());
        } else {
            missing.push(chapter.clone());
        }
    }

    (width, existing, missing)
}
