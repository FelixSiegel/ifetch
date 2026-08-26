use rust_decimal::Decimal;

#[derive(Debug, Clone)]
pub struct Manga {
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct Chapter {
    pub number: Decimal,
    pub label: String,
    pub url: String,
}
