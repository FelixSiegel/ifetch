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
        total_chapters: usize,
        existing_chapters: usize,
        missing_chapters: usize,
    },
    Complete {
        manga_title: &'a str,
        manga_url: &'a str,
        total_chapters: usize,
        downloaded_now: usize,
        failed_now: usize,
        total_available: usize,
        first_error: Option<&'a str>,
    },
    Error {
        manga_title: &'a str,
        manga_url: &'a str,
        error_msg: &'a str,
    },
}

pub fn send_webhook(client: &Client, notify_type: NotificationType) {
    let Some(webhook_url) = crate::config::DISCORD_WEBHOOK_URL.as_deref() else {
        return;
    };

    let author = Author {
        name: "iFetch",
        icon_url: "https://raw.githubusercontent.com/FelixSiegel/ifetch/refs/heads/main/assets/iFetch-logo.png",
    };

    let embed = match notify_type {
        NotificationType::Start {
            manga_title,
            manga_url,
            description,
            total_chapters,
            existing_chapters,
            missing_chapters,
        } => Embed {
            author,
            title: manga_title,
            url: Some(manga_url),
            description: Some(description.to_string()),
            color: 0x3498db, // Blue
            fields: vec![
                Field {
                    name: "Status",
                    value: "Download Started".to_string(),
                    inline: true,
                },
                Field {
                    name: "To Download",
                    value: format!("{} chapters", missing_chapters),
                    inline: true,
                },
                Field {
                    name: "Progress",
                    value: format!("{}/{} downloaded", existing_chapters, total_chapters),
                    inline: true,
                },
            ],
        },
        NotificationType::Complete {
            manga_title,
            manga_url,
            total_chapters,
            downloaded_now,
            failed_now,
            total_available,
            first_error,
        } => {
            let (status_text, color) = if failed_now == 0 {
                ("Download Completed", 0x2ecc71) // Green
            } else if downloaded_now > 0 {
                ("Partially Completed", 0xf39c12) // Orange / Amber
            } else {
                ("Download Failed", 0xe74c3c) // Red
            };

            let mut fields = vec![
                Field {
                    name: "Status",
                    value: status_text.to_string(),
                    inline: true,
                },
                Field {
                    name: "Downloaded Now",
                    value: format!("{} chapters", downloaded_now),
                    inline: true,
                },
            ];

            if failed_now > 0 {
                fields.push(Field {
                    name: "Failed",
                    value: format!("{} chapters", failed_now),
                    inline: true,
                });
            }

            fields.push(Field {
                name: "Progress",
                value: format!("{}/{} available", total_available, total_chapters),
                inline: true,
            });

            if let Some(err) = first_error {
                fields.push(Field {
                    name: "Error Details",
                    value: crate::utils::truncate_str(err, 250).to_string(),
                    inline: false,
                });
            }

            Embed {
                author,
                title: manga_title,
                url: Some(manga_url),
                description: None,
                color,
                fields,
            }
        }
        NotificationType::Error {
            manga_title,
            manga_url,
            error_msg,
        } => Embed {
            author,
            title: manga_title,
            url: Some(manga_url),
            description: Some(error_msg.to_string()),
            color: 0xe74c3c, // Red
            fields: vec![Field {
                name: "Status",
                value: "Failed to Fetch Manga".to_string(),
                inline: true,
            }],
        },
    };

    let payload = WebhookPayload {
        embeds: vec![embed],
    };

    match client.post(webhook_url).json(&payload).send() {
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
