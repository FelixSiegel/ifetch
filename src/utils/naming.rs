use regex::Regex;
use std::sync::LazyLock;

static INVALID_CHARS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[<>:"/\\|?*\x00-\x1f]"#).unwrap());
static SPACES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

/// Sanitizes a title or folder name by removing invalid filesystem characters,
/// collapsing whitespaces, trimming dots and spaces, and limiting length.
pub fn sanitize_name(name: &str, max_chars: usize) -> String {
    let cleaned = INVALID_CHARS_RE.replace_all(name, "");
    let cleaned = SPACES_RE.replace_all(&cleaned, " ");
    let mut cleaned = cleaned.trim_matches([' ', '.']).to_string();
    if cleaned.is_empty() {
        return "manga".to_string();
    }

    let mut chars: Vec<char> = cleaned.chars().collect();
    if chars.len() > max_chars {
        chars.truncate(max_chars);
        cleaned = chars.into_iter().collect();
        cleaned = cleaned.trim_end_matches([' ', '.']).to_string();
    }

    if cleaned.is_empty() {
        "manga".to_string()
    } else {
        cleaned
    }
}

/// Generates a sanitized, zero-padded filename for a chapter CBZ archive.
///
/// For example: `"Initial D - Chapter 001.cbz"`
pub fn chapter_filename(title: &str, number_str: &str, width: usize) -> String {
    let parts: Vec<&str> = number_str.split('.').collect();
    let whole = parts[0];
    let whole_trimmed = whole.trim_start_matches('0');
    let whole = if whole_trimmed.is_empty() {
        "0"
    } else {
        whole_trimmed
    };

    let mut padded = String::new();
    if whole.len() < width {
        padded.push_str(&"0".repeat(width - whole.len()));
    }
    padded.push_str(whole);
    if parts.len() > 1 {
        padded.push('.');
        padded.push_str(parts[1]);
    }

    let clean_title = sanitize_name(title, 100);
    format!("{} - Chapter {}.cbz", clean_title, padded)
}

/// Computes a filesystem-safe directory name for a given manga title.
pub fn get_folder_name(title: &str) -> String {
    sanitize_name(title, 120)
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
