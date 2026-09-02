use reqwest::blocking::Client;
use serde::Serialize;

#[derive(Serialize)]
pub struct WebhookPayload<'a> {
    pub embeds: Vec<Embed<'a>>,
}

#[derive(Serialize)]
pub struct Embed<'a> {
    pub author: Author<'a>,
    pub title: &'a str,
    pub url: Option<&'a str>,
    pub description: Option<String>,
    pub color: u32,
    pub fields: Vec<Field<'a>>,
}

#[derive(Serialize)]
pub struct Author<'a> {
    pub name: &'a str,
    pub icon_url: &'a str,
}

#[derive(Serialize)]
pub struct Field<'a> {
    pub name: &'a str,
    pub value: String,
    pub inline: bool,
}

pub enum NotificationType<'a> {
    Start {
        manga_title: &'a str,
        manga_url: &'a str,
        description: &'a str,
        chapter_count: usize,
    },
    Success {
        manga_title: &'a str,
        manga_url: &'a str,
        chapter_count: usize,
    },
    Error {
        manga_title: &'a str,
        manga_url: &'a str,
        error_msg: &'a str,
    },
}

pub fn send_webhook(client: &Client, notify_type: NotificationType) {
    let webhook_url = match std::env::var("DISCORD_WEBHOOK_URL") {
        Ok(url) => url.trim_matches('"').trim_matches('\'').to_string(),
        Err(_) => return,
    };

    let author = Author {
        name: "iFetch",
        icon_url: "https://raw.githubusercontent.com/FelixSiegel/ifetch/refs/heads/main/assets/iFetch-logo.png", // Generic fetch/download icon
    };

    let embed = match notify_type {
        NotificationType::Start {
            manga_title,
            manga_url,
            description,
            chapter_count,
        } => Embed {
            author,
            title: "Download Started",
            url: Some(manga_url),
            description: Some(description.to_string()),
            color: 0x3498db, // Blue
            fields: vec![
                Field {
                    name: "Manga",
                    value: manga_title.to_string(),
                    inline: true,
                },
                Field {
                    name: "Chapters",
                    value: chapter_count.to_string(),
                    inline: true,
                },
            ],
        },
        NotificationType::Success {
            manga_title,
            manga_url,
            chapter_count,
        } => Embed {
            author,
            title: "Download Completed",
            url: Some(manga_url),
            description: None,
            color: 0x2ecc71, // Green
            fields: vec![
                Field {
                    name: "Manga",
                    value: manga_title.to_string(),
                    inline: true,
                },
                Field {
                    name: "Chapters",
                    value: chapter_count.to_string(),
                    inline: true,
                },
            ],
        },
        NotificationType::Error {
            manga_title,
            manga_url,
            error_msg,
        } => Embed {
            author,
            title: "Download Failed",
            url: Some(manga_url),
            description: Some(error_msg.to_string()),
            color: 0xe74c3c, // Red
            fields: vec![Field {
                name: "Manga",
                value: manga_title.to_string(),
                inline: true,
            }],
        },
    };

    let payload = WebhookPayload {
        embeds: vec![embed],
    };

    match client.post(&webhook_url).json(&payload).send() {
        Ok(res) => {
            if !res.status().is_success() {
                log::error!("Discord webhook failed with status: {}", res.status());
                if let Ok(text) = res.text() {
                    log::error!("Discord response: {}", text);
                }
            }
        }
        Err(e) => log::error!("Failed to send Discord webhook: {}", e),
    }
}
