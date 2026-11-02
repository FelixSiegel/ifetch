use crate::{
    models::{Chapter, Manga},
    utils::escape_xml,
};
use anyhow::Result;
use std::{fs::File, path::Path};

/// Generates the `ComicInfo.xml` metadata file contents for a CBZ archive.
pub fn generate_comic_info(manga: &Manga, chapter: &Chapter) -> String {
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
