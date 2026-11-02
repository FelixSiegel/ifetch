use crate::models::Chapter;
use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;
use std::collections::HashSet;

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
