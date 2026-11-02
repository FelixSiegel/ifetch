use crate::{
    models::{Chapter, Manga},
    utils::{CHAPTER_RE, MANGA_RE},
};
use anyhow::{Result, bail};
use regex::Regex;
use rust_decimal::Decimal;
use scraper::{Html, Selector};
use std::{collections::HashSet, sync::LazyLock};
use url::Url;

static THZQ_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)var\s+thzq\s*=\s*\[(.*?)\]\s*;").unwrap());
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

/// Extracts and normalizes text content from a parsed HTML element.
pub fn element_text(el: &scraper::ElementRef) -> String {
    el.text().collect::<Vec<_>>().join(" ").trim().to_string()
}

/// Parses search result HTML from MangaKatana into a list of matching `Manga` objects.
pub fn parse_search_results(
    text: &str,
    path: &str,
    url: &Url,
    query: &str,
    base_url: &str,
) -> Result<Vec<Manga>> {
    let doc = Html::parse_document(text);

    if MANGA_RE.is_match(path) {
        let title = doc
            .select(&H1_SEL)
            .next()
            .map(|e| element_text(&e))
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
            .map(|e| element_text(&e))
            .unwrap_or_default();

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
    let base_url_parsed = Url::parse(base_url)?;

    for item in doc.select(&ITEM_SEL) {
        if let Some(anchor) = item.select(&TITLE_SEL).next()
            && let Some(href) = anchor.value().attr("href")
            && let Ok(joined) = base_url_parsed.join(href)
        {
            let p = joined.path().trim_end_matches('/');
            if MANGA_RE.is_match(p) && !seen.contains(joined.as_str()) {
                seen.insert(joined.to_string());
                let title = element_text(&anchor);

                let cover_url = item
                    .select(&WRAP_IMG_SEL)
                    .next()
                    .and_then(|img| img.value().attr("data-src").or(img.value().attr("src")))
                    .unwrap_or("")
                    .to_string();

                let status = item
                    .select(&STATUS_SEL)
                    .next()
                    .map(|e| element_text(&e))
                    .unwrap_or_default();

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

/// Parses a MangaKatana manga series detail page for its metadata and chapter listing.
pub fn parse_manga_page(
    text: &str,
    effective_url: &Url,
    url: &str,
    base_url: &str,
) -> Result<(Manga, Vec<Chapter>)> {
    let doc = Html::parse_document(text);

    let manga_path = effective_url.path().trim_end_matches('/');
    let id = manga_path.split('/').next_back().unwrap_or("").to_string();

    let title = doc
        .select(&H1_SEL)
        .next()
        .map(|e| element_text(&e))
        .unwrap_or_else(|| {
            if id.is_empty() {
                "manga".to_string()
            } else {
                id.clone()
            }
        });

    let description = doc
        .select(&SUMMARY_P_SEL)
        .next()
        .map(|e| element_text(&e))
        .unwrap_or_default();

    let genres: Vec<String> = doc
        .select(&GENRES_SEL)
        .map(|e| element_text(&e))
        .filter(|s| !s.is_empty())
        .collect();

    let authors: Vec<String> = doc
        .select(&AUTHORS_SEL)
        .map(|e| element_text(&e))
        .filter(|s| !s.is_empty())
        .collect();

    let alt_names: Vec<String> = doc
        .select(&ALT_SEL)
        .next()
        .map(|e| element_text(&e))
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
        .map(|e| element_text(&e))
        .unwrap_or_default();

    let manga = Manga {
        id,
        title,
        url: effective_url.to_string(),
        cover_url,
        status,
        description,
        genres,
        authors,
        alt_names,
    };

    let mut chapters = Vec::new();
    let base_url_parsed = Url::parse(base_url)?;
    let original_manga_path = Url::parse(url)?.path().trim_end_matches('/').to_string();

    for anchor in doc.select(&ANCHOR_SEL) {
        if let Some(href) = anchor.value().attr("href")
            && let Ok(joined) = base_url_parsed.join(href)
        {
            let p = joined.path().trim_end_matches('/');
            if let Some(caps) = CHAPTER_RE.captures(p)
                && caps
                    .get(1)
                    .is_some_and(|m| m.as_str().ends_with(&original_manga_path))
            {
                let num_str = caps.get(2).map_or("", |m| m.as_str());
                if let Ok(num) = Decimal::from_str_exact(num_str) {
                    let label = element_text(&anchor);
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

/// Extracts chapter image URLs from the inline JavaScript array `var thzq = [...]`.
pub fn extract_image_urls(text: &str) -> Option<Vec<String>> {
    if let Some(caps) = THZQ_RE.captures(text) {
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
            return Some(urls);
        }
    }
    None
}
