use anyhow::{Result, bail};
use regex::Regex;
use url::Url;

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

    let chapter_re = Regex::new(r"^(/manga/[^/]+\.\d+)/c([^/]+)$").unwrap();
    let manga_re = Regex::new(r"^/manga/[^/]+\.\d+$").unwrap();

    let path = if let Some(caps) = chapter_re.captures(path) {
        caps.get(1).unwrap().as_str()
    } else if path.ends_with("/download") {
        path.strip_suffix("/download").unwrap()
    } else {
        path
    };

    if !manga_re.is_match(path) {
        bail!("Expected MangaKatana manga or chapter URL");
    }

    let base = Url::parse("https://mangakatana.com/")?;
    Ok(Some(base.join(path)?.to_string()))
}

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

    let mut name = format!("{} - Chapter {}", title, padded);

    let invalid_re = Regex::new(r#"[<>:"/\\|?*\x00-\x1f]"#).unwrap();
    name = invalid_re.replace_all(&name, "").to_string();

    let spaces_re = Regex::new(r"\s+").unwrap();
    name = spaces_re.replace_all(&name, " ").to_string();
    name = name.trim_matches(|c| c == ' ' || c == '.').to_string();

    if name.is_empty() {
        name = "manga".to_string();
    }

    let mut chars: Vec<char> = name.chars().collect();
    if chars.len() > 140 {
        chars.truncate(140);
        name = chars.into_iter().collect();
    }

    format!("{}.cbz", name)
}

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

pub fn escape_xml(text: &str) -> String {
    text.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
}
