use std::{env::var, sync::LazyLock};

pub static CRON_HOURS: LazyLock<i64> = LazyLock::new(|| {
    var("IFETCH_CRON_HOURS")
        .unwrap_or_else(|_| "12".to_string())
        .parse()
        .unwrap_or(12)
});

pub static DISCORD_WEBHOOK_URL: LazyLock<Option<String>> = LazyLock::new(|| {
    var("DISCORD_WEBHOOK_URL")
        .ok()
        .map(|url| url.trim_matches('"').trim_matches('\'').trim().to_string())
        .filter(|url| !url.is_empty())
});
