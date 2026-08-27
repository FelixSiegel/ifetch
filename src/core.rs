use crate::models::{Chapter, Manga};
use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use rust_decimal::Decimal;
use scraper::{Html, Selector};
use std::collections::HashSet;
use std::io::Write;
use url::Url;

const BASE_URL: &str = "https://mangakatana.com/";

pub fn build_client() -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124 Safari/537.36")
    );

    Client::builder()
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("Failed to build HTTP client")
}

pub fn search_manga(client: &Client, query: &str) -> Result<Vec<Manga>> {
    let res = client
        .get(Url::parse_with_params(BASE_URL, &[("search", query)])?)
        .send()?
        .error_for_status()?;
    let url = res.url().clone();
    let path = url.path().trim_end_matches('/');

    let manga_re = Regex::new(r"^/manga/[^/]+\.\d+$").unwrap();
    let text = res.text()?;
    let doc = Html::parse_document(&text);

    if manga_re.is_match(path) {
        let title_sel = Selector::parse("h1").unwrap();
        let title = doc
            .select(&title_sel)
            .next()
            .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
            .unwrap_or_else(|| query.to_string());

        let cover_sel = Selector::parse(".cover img").unwrap();
        let cover_url = doc
            .select(&cover_sel)
            .next()
            .and_then(|img| img.value().attr("src").or(img.value().attr("data-src")))
            .unwrap_or("")
            .to_string();

        let status_sel = Selector::parse(".status").unwrap();
        let status = doc
            .select(&status_sel)
            .next()
            .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
            .unwrap_or("".to_string());

        return Ok(vec![Manga {
            id: path.split('/').last().unwrap_or("").to_string(),
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
    let item_sel = Selector::parse(".item").unwrap();
    let base_url_parsed = Url::parse(BASE_URL)?;

    for item in doc.select(&item_sel) {
        let title_sel = Selector::parse(".title a").unwrap();
        if let Some(anchor) = item.select(&title_sel).next() {
            if let Some(href) = anchor.value().attr("href") {
                let joined = base_url_parsed.join(href)?;
                let p = joined.path().trim_end_matches('/');
                if manga_re.is_match(p) && !seen.contains(joined.as_str()) {
                    seen.insert(joined.to_string());
                    let title = anchor
                        .text()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .trim()
                        .to_string();

                    let img_sel = Selector::parse(".wrap_img img").unwrap();
                    let cover_url = item
                        .select(&img_sel)
                        .next()
                        .and_then(|img| img.value().attr("data-src").or(img.value().attr("src")))
                        .unwrap_or("")
                        .to_string();

                    let status_sel = Selector::parse(".status").unwrap();
                    let status = item
                        .select(&status_sel)
                        .next()
                        .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
                        .unwrap_or("".to_string());

                    results.push(Manga {
                        id: p.split('/').last().unwrap_or("").to_string(),
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
    }
    Ok(results)
}

pub fn manga_chapters(client: &Client, url: &str) -> Result<(Manga, Vec<Chapter>)> {
    let res = client.get(url).send()?.error_for_status()?;
    let text = res.text()?;
    let doc = Html::parse_document(&text);

    let title_sel = Selector::parse("h1").unwrap();
    let title = doc
        .select(&title_sel)
        .next()
        .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
        .unwrap_or_else(|| {
            let parsed = Url::parse(url).unwrap();
            parsed
                .path()
                .split('/')
                .last()
                .unwrap_or("manga")
                .to_string()
        });

    let desc_sel = Selector::parse(".summary p").unwrap();
    let description = doc
        .select(&desc_sel)
        .next()
        .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
        .unwrap_or_else(String::new);

    let genres_sel = Selector::parse(".genres a").unwrap();
    let genres: Vec<String> = doc
        .select(&genres_sel)
        .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let authors_sel = Selector::parse(".authors a.author").unwrap();
    let authors: Vec<String> = doc
        .select(&authors_sel)
        .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let alt_sel = Selector::parse(".alt_name").unwrap();
    let alt_names: Vec<String> = doc
        .select(&alt_sel)
        .next()
        .map(|e| e.text().collect::<Vec<_>>().join(" "))
        .unwrap_or_else(String::new)
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let cover_sel = Selector::parse(".cover img").unwrap();
    let cover_url = doc
        .select(&cover_sel)
        .next()
        .and_then(|img| img.value().attr("src").or(img.value().attr("data-src")))
        .unwrap_or("")
        .to_string();

    let status_sel = Selector::parse(".status").unwrap();
    let status = doc
        .select(&status_sel)
        .next()
        .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
        .unwrap_or("".to_string());

    let id = Url::parse(url)?
        .path()
        .split('/')
        .last()
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
    let anchor_sel = Selector::parse("a[href]").unwrap();
    let base_url_parsed = Url::parse(BASE_URL)?;
    let manga_path = Url::parse(url)?.path().trim_end_matches('/').to_string();
    let chapter_re = Regex::new(r"^(/manga/[^/]+\.\d+)/c([^/]+)$").unwrap();

    for anchor in doc.select(&anchor_sel) {
        if let Some(href) = anchor.value().attr("href") {
            let joined = base_url_parsed.join(href)?;
            let p = joined.path().trim_end_matches('/');
            if let Some(caps) = chapter_re.captures(p) {
                if caps.get(1).unwrap().as_str().ends_with(&manga_path) {
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
    }

    // Sort chapters ascending by number
    chapters.sort_by(|a, b| a.number.cmp(&b.number));
    chapters.dedup_by(|a, b| a.number == b.number);

    if chapters.is_empty() {
        bail!("No chapters found on manga page");
    }
    Ok((manga, chapters))
}

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

pub fn chapter_images(client: &Client, chapter_url: &str) -> Result<Vec<String>> {
    for suffix in ["", "?sv=mk", "?sv=3"] {
        let url = format!("{}{}", chapter_url, suffix);
        let res = client.get(&url).send()?;
        if !res.status().is_success() {
            continue;
        }
        let text = res.text()?;

        let thzq_re = Regex::new(r"var\s+thzq\s*=\s*\[(.*?)\]\s*;").unwrap();
        if let Some(caps) = thzq_re.captures(&text) {
            let array_content = caps.get(1).unwrap().as_str();
            let url_re = Regex::new(r#"['"](https?://[^'"]+)['"]"#).unwrap();
            let mut urls = Vec::new();
            for m in url_re.captures_iter(array_content) {
                // html unescape could be needed, but simple URLs rarely have it.
                // We'll use a simple replace for common entities if needed, or rely on URL parsing.
                let mut u = m.get(1).unwrap().as_str().to_string();
                u = u.replace("&amp;", "&");
                urls.push(u);
            }
            if !urls.is_empty() {
                return Ok(urls);
            }
        }
    }
    bail!("Chapter contains no downloadable images")
}

use std::fs::File;
use std::path::Path;

use crate::utils::{chapter_filename, image_extension};
use zip::write::SimpleFileOptions;

pub fn download_chapter(
    client: &Client,
    manga: &Manga,
    chapter: &Chapter,
    output_dir: &Path,
    width: usize,
    verify: bool,
    pb: &indicatif::ProgressBar,
) -> Result<Option<std::path::PathBuf>> {
    let filename = chapter_filename(&manga.title, &chapter.number.to_string(), width);
    let dest = output_dir.join(&filename);

    if !verify && dest.exists() {
        pb.finish_with_message(format!("Skipped Chapter {}", chapter.number));
        return Ok(None);
    }

    pb.set_message(format!("Chapter {}...", chapter.number));
    let urls = chapter_images(client, &chapter.url)?;

    if dest.exists() {
        let is_updated = (|| -> Result<bool> {
            let file = std::fs::File::open(&dest)?;
            let archive = zip::ZipArchive::new(file)?;
            // ComicInfo.xml is 1 extra file
            Ok(archive.len() != urls.len() + 1)
        })()
        .unwrap_or(true);

        if !is_updated {
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

    let result: Result<()> = (|| {
        let file = File::create(&temp)?;
        let mut archive = zip::ZipWriter::new(file);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

        use crate::utils::escape_xml;
        let comic_info = format!(
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
        );
        archive.start_file("ComicInfo.xml", options.clone())?;
        archive.write_all(comic_info.as_bytes())?;

        for (i, img_url) in urls.iter().enumerate() {
            let mut res = client
                .get(img_url)
                .header("Referer", &chapter.url)
                .send()?
                .error_for_status()?;

            let mut data = Vec::new();
            res.copy_to(&mut data)?;

            let ct = res
                .headers()
                .get("Content-Type")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("");
            let ext = image_extension(ct, &data, img_url);

            archive.start_file(format!("{:03}{}", i + 1, ext), options.clone())?;
            archive.write_all(&data)?;

            pb.inc(1);
        }
        archive.finish()?;
        Ok(())
    })();

    match result {
        Ok(_) => {
            std::fs::rename(&temp, &dest)?;
            pb.finish_with_message(format!("Saved Chapter {}", chapter.number));
            Ok(Some(dest))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            pb.finish_with_message(format!("Error Chapter {}", chapter.number));
            Err(e)
        }
    }
}
