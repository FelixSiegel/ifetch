use crate::{
    models::{Chapter, Manga},
    rate_limit::{RateLimitError, is_rate_limit_error},
    utils::{chapter_filename, image_extension},
};
use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::{
    blocking::Client,
    header::{HeaderMap, HeaderValue, USER_AGENT},
};
use rust_decimal::Decimal;
use scraper::{Html, Selector};
use std::{
    collections::HashSet,
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::LazyLock,
    thread,
    time::Duration,
};
use url::Url;
use zip::write::SimpleFileOptions;

static MANGA_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/manga/[^/]+\.\d+$").unwrap());
static CHAPTER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(/manga/[^/]+\.\d+)/c([^/]+)$").unwrap());
static THZQ_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"var\s+thzq\s*=\s*\[(.*?)\]\s*;").unwrap());
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"['"](https?://[^'"]+)['"]"#).unwrap());

static H1_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("h1").unwrap());
static COVER_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".cover img").unwrap());
static STATUS_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".status").unwrap());
static ITEM_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".item").unwrap());
static TITLE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".title a").unwrap());
static WRAP_IMG_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".wrap_img img").unwrap());
static SUMMARY_P_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".summary p").unwrap());
static GENRES_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".genres a").unwrap());
static AUTHORS_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".authors a.author").unwrap());
static ALT_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".alt_name").unwrap());
static ANCHOR_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());

const BASE_URL: &str = "https://mangakatana.com/";

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

/// Detects whether an HTTP response indicates a Cloudflare challenge, bot verification,
/// or rate limiting response.
pub fn is_cloudflare_or_rate_limited(status: reqwest::StatusCode, body: &str) -> bool {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::FORBIDDEN
        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
    {
        return true;
    }

    // Cloudflare challenges and bot blocks are always in the HTML head/top section.
    // Inspect only the first 4KB to avoid scanning large HTML payloads.
    let prefix = if body.len() > 4096 {
        match body.char_indices().take_while(|(i, _)| *i <= 4096).last() {
            Some((i, c)) => &body[..i + c.len_utf8()],
            None => body,
        }
    } else {
        body
    };

    prefix.contains("<title>Just a moment...</title>")
        || prefix.contains("cf-chl-")
        || prefix.contains("cf-browser-verification")
        || prefix.contains("Checking your browser")
        || prefix.contains("Attention Required! | Cloudflare")
        || (prefix.contains("Cloudflare")
            && prefix.contains("Ray ID")
            && !prefix.contains("var thzq"))
}

/// Searches MangaKatana for manga matching the given query keyword.
///
/// Returns a list of matching `Manga` entries, or an error if rate-limited or unavailable.
pub fn search_manga(client: &Client, query: &str) -> Result<Vec<Manga>> {
    let res = client
        .get(Url::parse_with_params(BASE_URL, &[("search", query)])?)
        .send()?
        .error_for_status()?;
    let status = res.status();
    let url = res.url().clone();
    let path = url.path().trim_end_matches('/');

    let text = res.text()?;
    if is_cloudflare_or_rate_limited(status, &text) {
        return Err(RateLimitError::new(
            Some(status),
            "MangaKatana search temporarily blocked by Cloudflare or rate limit",
        )
        .into());
    }
    let doc = Html::parse_document(&text);

    if MANGA_RE.is_match(path) {
        let title = doc
            .select(&H1_SEL)
            .next()
            .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
            .unwrap_or_else(|| query.to_string());

        let cover_url = doc
            .select(&COVER_SEL)
            .next()
            .and_then(|img| img.value().attr("src").or(img.value().attr("data-src")))
            .unwrap_or("")
            .to_string();

        let status = doc
            .select(&STATUS_SEL)
            .next()
            .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
            .unwrap_or("".to_string());

        return Ok(vec![Manga {
            id: path.split('/').next_back().unwrap_or("").to_string(),
            title,
            url: url.to_string(),
            cover_url,
            status,
            description: String::new(),
            genres: vec![],
            authors: vec![],
            alt_names: vec![],
        }]);
    }

    let mut results = Vec::new();
    let mut seen = HashSet::new();
    let base_url_parsed = Url::parse(BASE_URL)?;

    for item in doc.select(&ITEM_SEL) {
        if let Some(anchor) = item.select(&TITLE_SEL).next()
            && let Some(href) = anchor.value().attr("href")
        {
            let joined = base_url_parsed.join(href)?;
            let p = joined.path().trim_end_matches('/');
            if MANGA_RE.is_match(p) && !seen.contains(joined.as_str()) {
                seen.insert(joined.to_string());
                let title = anchor
                    .text()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .to_string();

                let cover_url = item
                    .select(&WRAP_IMG_SEL)
                    .next()
                    .and_then(|img| img.value().attr("data-src").or(img.value().attr("src")))
                    .unwrap_or("")
                    .to_string();

                let status = item
                    .select(&STATUS_SEL)
                    .next()
                    .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
                    .unwrap_or("".to_string());

                results.push(Manga {
                    id: p.split('/').next_back().unwrap_or("").to_string(),
                    title,
                    url: joined.to_string(),
                    cover_url,
                    status,
                    description: String::new(),
                    genres: vec![],
                    authors: vec![],
                    alt_names: vec![],
                });
            }
        }
    }
    Ok(results)
}

/// Fetches manga metadata and the complete sorted list of chapters from a series URL.
pub fn manga_chapters(client: &Client, url: &str) -> Result<(Manga, Vec<Chapter>)> {
    let res = client.get(url).send()?.error_for_status()?;
    let status = res.status();
    let text = res.text()?;
    if is_cloudflare_or_rate_limited(status, &text) {
        return Err(RateLimitError::new(
            Some(status),
            "MangaKatana chapter list blocked by Cloudflare or rate limit",
        )
        .into());
    }
    let doc = Html::parse_document(&text);

    let title = doc
        .select(&H1_SEL)
        .next()
        .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
        .unwrap_or_else(|| {
            let parsed = Url::parse(url).unwrap();
            parsed
                .path()
                .split('/')
                .next_back()
                .unwrap_or("manga")
                .to_string()
        });

    let description = doc
        .select(&SUMMARY_P_SEL)
        .next()
        .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
        .unwrap_or_default();

    let genres: Vec<String> = doc
        .select(&GENRES_SEL)
        .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let authors: Vec<String> = doc
        .select(&AUTHORS_SEL)
        .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let alt_names: Vec<String> = doc
        .select(&ALT_SEL)
        .next()
        .map(|e| e.text().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let cover_url = doc
        .select(&COVER_SEL)
        .next()
        .and_then(|img| img.value().attr("src").or(img.value().attr("data-src")))
        .unwrap_or("")
        .to_string();

    let status = doc
        .select(&STATUS_SEL)
        .next()
        .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
        .unwrap_or("".to_string());

    let id = Url::parse(url)?
        .path()
        .trim_end_matches('/')
        .split('/')
        .next_back()
        .unwrap_or("")
        .to_string();

    let manga = Manga {
        id,
        title,
        url: url.to_string(),
        cover_url,
        status,
        description,
        genres,
        authors,
        alt_names,
    };

    let mut chapters = Vec::new();
    let base_url_parsed = Url::parse(BASE_URL)?;
    let manga_path = Url::parse(url)?.path().trim_end_matches('/').to_string();

    for anchor in doc.select(&ANCHOR_SEL) {
        if let Some(href) = anchor.value().attr("href") {
            let joined = base_url_parsed.join(href)?;
            let p = joined.path().trim_end_matches('/');
            if let Some(caps) = CHAPTER_RE.captures(p)
                && caps.get(1).unwrap().as_str().ends_with(&manga_path)
            {
                let num_str = caps.get(2).unwrap().as_str();
                if let Ok(num) = Decimal::from_str_exact(num_str) {
                    let label = anchor
                        .text()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .trim()
                        .to_string();
                    let label = if label.is_empty() {
                        format!("Chapter {}", num_str)
                    } else {
                        label
                    };
                    chapters.push(Chapter {
                        number: num,
                        label,
                        url: joined.to_string(),
                    });
                }
            }
        }
    }

    // Sort chapters ascending by number
    chapters.sort_by_key(|a| a.number);
    chapters.dedup_by(|a, b| a.number == b.number);

    if chapters.is_empty() {
        bail!("No chapters found on manga page");
    }
    Ok((manga, chapters))
}

/// Filters chapters by a user-specified string (e.g. `"all"`, `"1-10"`, `"1,3,5.5"`).
pub fn select_chapters(chapters: &[Chapter], spec: &str) -> Result<Vec<Chapter>> {
    let spec = spec.trim().to_lowercase();
    if spec == "all" || spec.is_empty() {
        return Ok(chapters.to_vec());
    }

    let mut selected = HashSet::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start_txt, end_txt)) = part.split_once('-') {
            let start = Decimal::from_str_exact(start_txt.trim())
                .with_context(|| format!("Invalid start range: {}", start_txt))?;
            let end = Decimal::from_str_exact(end_txt.trim())
                .with_context(|| format!("Invalid end range: {}", end_txt))?;
            if start > end {
                bail!("Invalid descending range: {}", part);
            }
            for ch in chapters {
                if ch.number >= start && ch.number <= end {
                    selected.insert(ch.number);
                }
            }
        } else {
            let target = Decimal::from_str_exact(part)
                .with_context(|| format!("Invalid chapter selection: {}", part))?;
            selected.insert(target);
        }
    }

    let res: Vec<Chapter> = chapters
        .iter()
        .filter(|c| selected.contains(&c.number))
        .cloned()
        .collect();
    if res.is_empty() {
        bail!("Selection matches no chapters");
    }
    Ok(res)
}

/// Fetches image URLs for a chapter by probing MangaKatana server mirrors (`""`, `"?sv=mk"`, `"?sv=3"`).
pub fn chapter_images(client: &Client, chapter_url: &str) -> Result<Vec<String>> {
    let mut hit_rate_limit = false;
    let mut last_status = None;

    for suffix in ["", "?sv=mk", "?sv=3"] {
        let url = format!("{}{}", chapter_url, suffix);
        let res = match client.get(&url).send() {
            Ok(r) => r,
            Err(e) => {
                log::warn!("Failed request to {}: {}", url, e);
                continue;
            }
        };
        let status = res.status();
        last_status = Some(status);

        let text = match res.text() {
            Ok(t) => t,
            Err(_) => continue,
        };

        if is_cloudflare_or_rate_limited(status, &text) {
            hit_rate_limit = true;
            break;
        }

        if !status.is_success() {
            continue;
        }

        if let Some(caps) = THZQ_RE.captures(&text) {
            let array_content = caps.get(1).unwrap().as_str();
            let mut urls = Vec::new();
            for m in URL_RE.captures_iter(array_content) {
                let mut u = m.get(1).unwrap().as_str().to_string();
                if u.contains("&amp;") {
                    u = u.replace("&amp;", "&");
                }
                urls.push(u);
            }
            if !urls.is_empty() {
                return Ok(urls);
            }
        }
    }

    if hit_rate_limit {
        return Err(RateLimitError::new(
            last_status,
            "Cloudflare challenge or rate limit detected while fetching chapter images",
        )
        .into());
    }
    bail!("Chapter contains no downloadable images")
}

/// Generates the `ComicInfo.xml` metadata file contents for a CBZ archive.
pub fn generate_comic_info(manga: &Manga, chapter: &Chapter) -> String {
    use crate::utils::escape_xml;
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<ComicInfo xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <Title>{}</Title>
  <Series>{}</Series>
  <Number>{}</Number>
  <Summary>{}</Summary>
  <Genre>{}</Genre>
  <Writer>{}</Writer>
  <AlternateSeries>{}</AlternateSeries>
</ComicInfo>"#,
        escape_xml(&chapter.label),
        escape_xml(&manga.title),
        chapter.number,
        escape_xml(&manga.description),
        escape_xml(&manga.genres.join(", ")),
        escape_xml(&manga.authors.join(", ")),
        escape_xml(&manga.alt_names.join(", ")),
    )
}

/// Verifies whether an existing CBZ file matches the expected page count (including ComicInfo.xml).
pub fn is_chapter_up_to_date(dest: &Path, expected_page_count: usize) -> bool {
    (|| -> Result<bool> {
        let file = File::open(dest)?;
        let archive = zip::ZipArchive::new(file)?;
        Ok(archive.len() == expected_page_count + 1)
    })()
    .unwrap_or(false)
}

/// Fetches a single image with retries, exponential backoff, and Content-Type inspection.
pub fn fetch_single_image(
    client: &Client,
    img_url: &str,
    referer: &str,
    max_attempts: usize,
) -> Result<(Vec<u8>, String)> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let fetch_res = (|| -> Result<(Vec<u8>, String)> {
            let mut res = client.get(img_url).header("Referer", referer).send()?;

            let status = res.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let retry_after = res
                    .headers()
                    .get("Retry-After")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(std::time::Duration::from_secs);

                return Err(RateLimitError::new(
                    Some(status),
                    "Image CDN returned HTTP 429 Too Many Requests",
                )
                .with_retry_after(retry_after)
                .into());
            }

            if !status.is_success() {
                let text = res.text().unwrap_or_default();
                if is_cloudflare_or_rate_limited(status, &text) {
                    return Err(anyhow::Error::from(RateLimitError::new(
                        Some(status),
                        "Image CDN returned Cloudflare challenge page",
                    )));
                }
                bail!("HTTP error {} downloading image from {}", status, img_url);
            }

            let ct = res
                .headers()
                .get("Content-Type")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("")
                .to_string();

            let mut data = match res.content_length() {
                Some(len) if len < 50 * 1024 * 1024 => Vec::with_capacity(len as usize),
                _ => Vec::new(),
            };
            res.copy_to(&mut data)?;
            Ok((data, ct))
        })();

        match fetch_res {
            Ok(val) => return Ok(val),
            Err(e) if is_rate_limit_error(&e) => {
                return Err(e);
            }
            Err(e) if attempts < max_attempts => {
                log::warn!(
                    "Retrying image ({}) after error: {} (attempt {}/{})",
                    img_url,
                    e,
                    attempts,
                    max_attempts
                );
                thread::sleep(Duration::from_millis(500 * attempts as u64));
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
}

/// Downloads all pages for a chapter, builds a `ComicInfo.xml` metadata file,
/// and packages the content into a CBZ archive.
pub fn download_chapter(
    client: &Client,
    manga: &Manga,
    chapter: &Chapter,
    output_dir: &Path,
    width: usize,
    verify: bool,
    pb: &indicatif::ProgressBar,
) -> Result<Option<PathBuf>> {
    std::fs::create_dir_all(output_dir)?;
    let filename = chapter_filename(&manga.title, &chapter.number.to_string(), width);
    let dest = output_dir.join(&filename);

    if !verify && dest.exists() {
        pb.finish_with_message(format!("Skipped Chapter {}", chapter.number));
        return Ok(None);
    }

    pb.set_message(format!("Chapter {}...", chapter.number));
    let urls = chapter_images(client, &chapter.url)?;

    if dest.exists() {
        if is_chapter_up_to_date(&dest, urls.len()) {
            pb.finish_with_message(format!("Skipped Chapter {}", chapter.number));
            return Ok(None);
        }
        pb.set_message(format!(
            "Updating Ch {} ({} pages)",
            chapter.number,
            urls.len()
        ));
    } else {
        pb.set_message(format!("Chapter {} ({} pages)", chapter.number, urls.len()));
    }

    pb.set_length(urls.len() as u64);

    let mut temp = dest.clone();
    temp.set_extension("cbz.part");

    let write_cbz = || -> Result<()> {
        let file = File::create(&temp)?;
        let writer = BufWriter::new(file);
        let mut archive = zip::ZipWriter::new(writer);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

        let comic_info = generate_comic_info(manga, chapter);
        archive.start_file("ComicInfo.xml", options)?;
        archive.write_all(comic_info.as_bytes())?;

        for (i, img_url) in urls.iter().enumerate() {
            let (data, ct) =
                fetch_single_image(client, img_url, &chapter.url, 3).with_context(|| {
                    format!("Chapter {} page {}/{}", chapter.number, i + 1, urls.len())
                })?;

            let ext = image_extension(&ct, &data, img_url);
            archive.start_file(format!("{:03}{}", i + 1, ext), options)?;
            archive.write_all(&data)?;

            pb.inc(1);
        }
        archive.finish()?;
        Ok(())
    };

    match write_cbz() {
        Ok(_) => {
            std::fs::rename(&temp, &dest)?;
            pb.finish_with_message(format!("Saved Chapter {}", chapter.number));
            Ok(Some(dest))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            pb.abandon_with_message(format!("Error Chapter {}", chapter.number));
            Err(e)
        }
    }
}
