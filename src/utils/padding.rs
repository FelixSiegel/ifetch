use super::naming::{chapter_filename, find_chapter_cbz};

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
/// and marks each chapter's `downloaded` status in-place.
/// Returns `(padding_width, existing_count)`.
pub fn scan_manga_chapters(
    manga_dir: &std::path::Path,
    title: &str,
    chapters: &mut [crate::models::Chapter],
) -> (usize, usize) {
    let width = determine_width(chapters);
    upgrade_padding(title, chapters, manga_dir, width);

    let dir_exists = manga_dir.exists();
    let mut existing_count = 0;

    for chapter in chapters {
        let num_str = chapter.number.to_string();
        let filename = chapter_filename(title, &num_str, width);
        let direct_path = manga_dir.join(&filename);

        if dir_exists
            && (direct_path.exists() || find_chapter_cbz(manga_dir, title, &num_str).is_some())
        {
            chapter.downloaded = true;
            existing_count += 1;
        } else {
            chapter.downloaded = false;
        }
    }

    (width, existing_count)
}
