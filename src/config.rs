use std::{env::var, sync::LazyLock};

pub static BASE_URL: LazyLock<String> = LazyLock::new(|| {
    var("IFETCH_BASE_URL")
        .unwrap_or_else(|_| "https://mangakatana.com".to_string())
        .trim_end_matches('/')
        .to_string()
});

pub static CRON_HOURS: LazyLock<i64> = LazyLock::new(|| {
    var("IFETCH_CRON_HOURS")
        .unwrap_or_else(|_| "12".to_string())
        .parse()
        .unwrap_or(12)
});

pub static CACHE_TTL_MINS: LazyLock<u64> = LazyLock::new(|| {
    var("IFETCH_CACHE_TTL_MINS")
        .unwrap_or_else(|_| "5".to_string())
        .parse()
        .unwrap_or(5)
});

pub static DISCORD_WEBHOOK_URL: LazyLock<Option<String>> = LazyLock::new(|| {
    var("DISCORD_WEBHOOK_URL")
        .ok()
        .map(|url| url.trim_matches('"').trim_matches('\'').trim().to_string())
        .filter(|url| !url.is_empty())
});
