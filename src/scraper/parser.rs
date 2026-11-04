use crate::{
    models::{Chapter, Manga},
    utils::{CHAPTER_RE, MANGA_RE},
};
use anyhow::{Result, bail};
use regex::Regex;
use rust_decimal::Decimal;
use scraper::{ElementRef, Html, Selector};
use std::{collections::HashSet, sync::LazyLock};
use url::Url;

static THZQ_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)var\s+thzq\s*=\s*\[(.*?)\]\s*;").unwrap());
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"['"](https?://[^'"]+)['"]"#).unwrap());

static H1_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("h1").unwrap());
static COVER_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".cover img").unwrap());
static STATUS_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".status").unwrap());
static BOOK_LIST_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("#book_list").unwrap());
static SINGLE_BOOK_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("#single_book").unwrap());
static CHAPTERS_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".chapters").unwrap());
static ITEM_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".item").unwrap());
static TITLE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".title a").unwrap());
static WRAP_IMG_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".wrap_img img").unwrap());
static SUMMARY_P_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".summary p").unwrap());
static GENRES_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".genres a").unwrap());
static AUTHORS_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".authors a.author").unwrap());
static ALT_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".alt_name").unwrap());
static ANCHOR_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());

/// Extracts and normalizes text content from a parsed HTML element.
pub fn element_text(el: &ElementRef) -> String {
    let mut iter = el.text().map(|s| s.trim()).filter(|s| !s.is_empty());
    let first = match iter.next() {
        Some(f) => f,
        None => return String::new(),
    };
    match iter.next() {
        None => first.to_string(),
        Some(second) => {
            let mut out = String::with_capacity(first.len() + second.len() + 16);
            out.push_str(first);
            out.push(' ');
            out.push_str(second);
            for piece in iter {
                out.push(' ');
                out.push_str(piece);
            }
            out
        }
    }
}

/// Extracts a clean list of non-empty text strings matching a selector.
fn extract_text_list(scope: &ElementRef, selector: &Selector) -> Vec<String> {
    scope
        .select(selector)
        .map(|e| element_text(&e))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Extracts the manga identifier slug from a URL path.
fn manga_id_from_path(path: &str) -> &str {
    path.split('/').next_back().unwrap_or("")
}

/// Constructs a `Manga` model for search results where extended metadata is not yet populated.
fn search_result_manga(
    id: &str,
    title: String,
    url: String,
    cover_url: String,
    status: String,
) -> Manga {
    Manga {
        id: id.to_string(),
        title,
        url,
        cover_url,
        status,
        description: String::new(),
        genres: Vec::new(),
        authors: Vec::new(),
        alt_names: Vec::new(),
    }
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

    // If redirected directly to a single manga details page
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

        return Ok(vec![search_result_manga(
            manga_id_from_path(path),
            title,
            url.to_string(),
            cover_url,
            status,
        )]);
    }

    let mut results = Vec::new();
    let mut seen = HashSet::new();
    let base_url_parsed = Url::parse(base_url)?;

    let container = doc
        .select(&BOOK_LIST_SEL)
        .next()
        .unwrap_or_else(|| doc.root_element());

    for item in container.select(&ITEM_SEL) {
        if let Some(anchor) = item.select(&TITLE_SEL).next()
            && let Some(href) = anchor.value().attr("href")
            && let Ok(joined) = base_url_parsed.join(href)
        {
            let p = joined.path().trim_end_matches('/');
            let id = manga_id_from_path(p);
            if MANGA_RE.is_match(p) && seen.insert(id.to_string()) {
                let title = element_text(&anchor);

                let cover_url = item
                    .select(&WRAP_IMG_SEL)
                    .next()
                    .and_then(|img| {
                        img.value()
                            .attr("data-src")
                            .or_else(|| img.value().attr("src"))
                    })
                    .unwrap_or("")
                    .to_string();

                let status = item
                    .select(&STATUS_SEL)
                    .next()
                    .map(|e| element_text(&e))
                    .unwrap_or_default();

                results.push(search_result_manga(
                    id,
                    title,
                    joined.to_string(),
                    cover_url,
                    status,
                ));
            }
        }
    }
    Ok(results)
}

/// Parses an anchor element into a `Chapter` if it matches the manga's chapter URL pattern.
fn parse_chapter_anchor(
    anchor: &ElementRef,
    base_url: &Url,
    original_manga_path: &str,
) -> Option<Chapter> {
    let href = anchor.value().attr("href")?;
    if !href.contains("/c") {
        return None;
    }
    let joined = base_url.join(href).ok()?;
    let path = joined.path().trim_end_matches('/');
    let caps = CHAPTER_RE.captures(path)?;

    if !caps.get(1)?.as_str().ends_with(original_manga_path) {
        return None;
    }

    let num_str = caps.get(2)?.as_str();
    let number = Decimal::from_str_exact(num_str).ok()?;

    let text = element_text(anchor);
    let label = if text.is_empty() {
        format!("Chapter {}", num_str)
    } else {
        text
    };

    Some(Chapter {
        number,
        label,
        url: joined.to_string(),
    })
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
    let id = manga_id_from_path(manga_path).to_string();

    let info_scope = doc
        .select(&SINGLE_BOOK_SEL)
        .next()
        .unwrap_or_else(|| doc.root_element());

    let title = info_scope
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

    let description = info_scope
        .select(&SUMMARY_P_SEL)
        .next()
        .map(|e| element_text(&e))
        .unwrap_or_default();

    let genres = extract_text_list(&info_scope, &GENRES_SEL);
    let authors = extract_text_list(&info_scope, &AUTHORS_SEL);

    let alt_names: Vec<String> = info_scope
        .select(&ALT_SEL)
        .next()
        .map(|e| element_text(&e))
        .unwrap_or_default()
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let cover_url = info_scope
        .select(&COVER_SEL)
        .next()
        .and_then(|img| {
            img.value()
                .attr("src")
                .or_else(|| img.value().attr("data-src"))
        })
        .unwrap_or("")
        .to_string();

    let status = info_scope
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

    let base_url_parsed = Url::parse(base_url)?;
    let original_manga_path = Url::parse(url)?.path().trim_end_matches('/').to_string();

    let chapters_scope = info_scope
        .select(&CHAPTERS_SEL)
        .next()
        .unwrap_or(info_scope);

    let mut chapters: Vec<Chapter> = chapters_scope
        .select(&ANCHOR_SEL)
        .filter_map(|anchor| parse_chapter_anchor(&anchor, &base_url_parsed, &original_manga_path))
        .collect();

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
    let array_content = THZQ_RE.captures(text)?.get(1)?.as_str();
    let urls: Vec<String> = URL_RE
        .captures_iter(array_content)
        .filter_map(|m| {
            let s = m.get(1)?.as_str();
            if s.contains("&amp;") {
                Some(s.replace("&amp;", "&"))
            } else {
                Some(s.to_string())
            }
        })
        .collect();

    if urls.is_empty() { None } else { Some(urls) }
}
