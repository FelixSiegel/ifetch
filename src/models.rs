use rust_decimal::Decimal;
use serde::Serialize;

/// Represents a manga series fetched from MangaKatana or resolved locally.
#[derive(Debug, Clone, Serialize)]
pub struct Manga {
    /// Unique identifier / slug of the manga (e.g. "one-piece.12345").
    pub id: String,
    /// Human-readable title of the manga.
    pub title: String,
    /// Absolute URL to the manga overview page on MangaKatana.
    pub url: String,
    /// URL pointing to the manga's cover artwork, if available.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cover_url: String,
    /// Publication status of the manga (e.g. "Ongoing" or "Completed").
    #[serde(skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// Synopsis or description of the manga series.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// List of genre classifications associated with the manga.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    /// Authors, artists, or creators credited for the manga.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    /// Alternative titles or localized names for the manga.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub alt_names: Vec<String>,
}

/// Represents a single chapter within a manga series.
#[derive(Debug, Clone, Serialize)]
pub struct Chapter {
    /// Decimal chapter number (supports fractional numbers like 10.5).
    pub number: Decimal,
    /// Chapter title or display label as shown on the source website.
    pub label: String,
    /// Direct URL to the chapter reading page on MangaKatana.
    pub url: String,
}
