use crate::{
    archive::{comic_info::generate_comic_info, image::fetch_single_image},
    models::{Chapter, Manga},
    utils::image_extension,
};
use anyhow::{Context, Result};
use indicatif::ProgressBar;
use reqwest::blocking::Client;
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};
use zip::write::SimpleFileOptions;

/// Packages a chapter's pages and `ComicInfo.xml` metadata into an atomic CBZ archive (.part -> .cbz).
pub fn write_cbz_archive(
    dest: &Path,
    manga: &Manga,
    chapter: &Chapter,
    urls: &[String],
    client: &Client,
    pb: &ProgressBar,
) -> Result<()> {
    let mut temp = dest.to_path_buf();
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
            std::fs::rename(&temp, dest)?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}
