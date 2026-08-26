use rust_decimal::Decimal;

#[derive(Debug, Clone)]
pub struct Manga {
    pub title: String,
    pub url: String,
    pub description: String,
    pub genres: Vec<String>,
    pub authors: Vec<String>,
    pub alt_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Chapter {
    pub number: Decimal,
    pub label: String,
    pub url: String,
}
