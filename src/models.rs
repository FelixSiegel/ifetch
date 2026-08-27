use rust_decimal::Decimal;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Manga {
    pub id: String,
    pub title: String,
    pub url: String,
    pub cover_url: String,
    pub status: String,
    pub description: String,
    pub genres: Vec<String>,
    pub authors: Vec<String>,
    pub alt_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Chapter {
    pub number: Decimal,
    pub label: String,
    pub url: String,
}
