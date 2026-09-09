use crate::server::helpers::lock_mutex;
use log::{debug, info, warn};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

pub const BURST_CAPACITY: usize = 10;
pub const BURST_REPLENISH_IDLE_SECS: u64 = 15;
pub const BASE_DELAY_MS: u64 = 200;
pub const MAX_DELAY_MS: u64 = 2000;
pub const DELAY_STEP_UP_MS: u64 = 150;
pub const DELAY_STEP_DOWN_MS: u64 = 25;
pub const CONSECUTIVE_SUCCESS_RELAX: usize = 20;
pub const BASE_COOLDOWN_SECS: u64 = 30;
pub const MAX_COOLDOWN_SECS: u64 = 180;

#[derive(Debug, Clone)]
pub struct RateLimitStatsSnapshot {
    pub total_chapters: usize,
    pub total_blocks_hit: usize,
    pub current_burst_count: usize,
    pub min_burst_before_block: Option<usize>,
    pub max_burst_achieved: usize,
    pub current_delay_ms: u64,
    pub current_cooldown_secs: u64,
}

struct RateLimiterInner {
    burst_tokens: usize,
    last_permit_time: Instant,
    cooldown_until: Option<Instant>,
    current_delay: Duration,
    current_cooldown: Duration,
    consecutive_successes: usize,
    current_burst_count: usize,
    // Statistics
    total_chapters: usize,
    total_blocks_hit: usize,
    min_burst_before_block: Option<usize>,
    max_burst_achieved: usize,
    last_block_at: Option<Instant>,
}

pub struct AdaptiveRateLimiter {
    inner: Mutex<RateLimiterInner>,
}

impl Default for AdaptiveRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveRateLimiter {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RateLimiterInner {
                burst_tokens: BURST_CAPACITY,
                last_permit_time: Instant::now(),
                cooldown_until: None,
                current_delay: Duration::from_millis(BASE_DELAY_MS),
                current_cooldown: Duration::from_secs(BASE_COOLDOWN_SECS),
                consecutive_successes: 0,
                current_burst_count: 0,
                total_chapters: 0,
                total_blocks_hit: 0,
                min_burst_before_block: None,
                max_burst_achieved: 0,
                last_block_at: None,
            }),
        }
    }

    /// Called before starting each chapter download.
    /// Manages burst permits, inter-chapter delays, and pauses during cooldown.
    pub fn wait_for_chapter_permit(&self) {
        loop {
            let sleep_duration = {
                let mut inner = lock_mutex(&self.inner);
                let now = Instant::now();

                // If currently under a cooldown block, wait until cooldown passes
                if let Some(until) = inner.cooldown_until {
                    if now < until {
                        until.duration_since(now)
                    } else {
                        inner.cooldown_until = None;
                        info!("Cloudflare cooldown expired. Resuming chapter downloads.");
                        Duration::ZERO
                    }
                } else {
                    // Check if system has been idle; if so, replenish burst allowance
                    if now.duration_since(inner.last_permit_time)
                        >= Duration::from_secs(BURST_REPLENISH_IDLE_SECS)
                    {
                        if inner.burst_tokens < BURST_CAPACITY {
                            debug!(
                                "Downloader was idle for {:?}. Replenished burst tokens to {}",
                                now.duration_since(inner.last_permit_time),
                                BURST_CAPACITY
                            );
                        }
                        inner.burst_tokens = BURST_CAPACITY;
                        inner.current_burst_count = 0;
                    }

                    inner.last_permit_time = now;

                    if inner.burst_tokens > 0 {
                        inner.burst_tokens -= 1;
                        inner.current_burst_count += 1;
                        debug!(
                            "Burst token granted ({} remaining in burst)",
                            inner.burst_tokens
                        );
                        return;
                    }

                    inner.current_burst_count += 1;
                    inner.current_delay
                }
            };

            if sleep_duration > Duration::ZERO {
                std::thread::sleep(sleep_duration);
            } else {
                return;
            }
        }
    }

    /// Called when a chapter download succeeds.
    /// Updates statistics and gradually relaxes the inter-chapter delay.
    pub fn on_chapter_success(&self) {
        let mut inner = lock_mutex(&self.inner);
        inner.total_chapters += 1;
        inner.consecutive_successes += 1;
        if inner.current_burst_count > inner.max_burst_achieved {
            inner.max_burst_achieved = inner.current_burst_count;
        }

        // AIMD: Gradually relax delay after sustained successful chapters
        if inner.consecutive_successes >= CONSECUTIVE_SUCCESS_RELAX {
            inner.consecutive_successes = 0;
            let min_delay = Duration::from_millis(BASE_DELAY_MS);
            if inner.current_delay > min_delay {
                inner.current_delay = inner
                    .current_delay
                    .saturating_sub(Duration::from_millis(DELAY_STEP_DOWN_MS))
                    .max(min_delay);
                debug!(
                    "Relaxed adaptive chapter delay to {:?}",
                    inner.current_delay
                );
            }
        }
    }

    /// Called when a rate limit or Cloudflare challenge is encountered.
    /// Activates a synchronized cooldown, increases the inter-chapter delay,
    /// and records empirical statistics on Cloudflare's limits.
    pub fn on_rate_limit_hit(&self, reason: &str) -> Duration {
        let mut inner = lock_mutex(&self.inner);
        let now = Instant::now();

        let burst_at_block = inner.current_burst_count;
        inner.total_blocks_hit += 1;

        if burst_at_block > 0 {
            inner.min_burst_before_block = Some(
                inner
                    .min_burst_before_block
                    .map_or(burst_at_block, |prev| prev.min(burst_at_block)),
            );
        }

        // Compute cooldown: escalate if blocked repeatedly in under 5 minutes
        let cooldown = match inner.last_block_at {
            Some(last) if now.duration_since(last) < Duration::from_secs(300) => {
                let scaled = inner.current_cooldown.as_secs() * 3 / 2;
                Duration::from_secs(scaled.clamp(BASE_COOLDOWN_SECS, MAX_COOLDOWN_SECS))
            }
            _ => Duration::from_secs(BASE_COOLDOWN_SECS),
        };
        inner.current_cooldown = cooldown;
        inner.last_block_at = Some(now);
        inner.cooldown_until = Some(now + cooldown);

        inner.current_delay = (inner.current_delay + Duration::from_millis(DELAY_STEP_UP_MS))
            .min(Duration::from_millis(MAX_DELAY_MS));

        inner.burst_tokens = 0;
        inner.consecutive_successes = 0;
        inner.current_burst_count = 0;

        let threshold_str = inner
            .min_burst_before_block
            .map(|t| format!("~{} chapters", t))
            .unwrap_or_else(|| "unknown".to_string());

        warn!(
            "Cloudflare/MangaKatana rate limit hit! Reason: {}. Reached burst of {} chapters. Entering {:?} cooldown. Adaptive delay increased to {:?}. Empirical Cloudflare threshold: {}. Total blocks: {}.",
            reason,
            burst_at_block,
            cooldown,
            inner.current_delay,
            threshold_str,
            inner.total_blocks_hit
        );

        cooldown
    }

    /// Snapshot current statistics for logging and reporting
    pub fn get_stats(&self) -> RateLimitStatsSnapshot {
        let inner = lock_mutex(&self.inner);
        RateLimitStatsSnapshot {
            total_chapters: inner.total_chapters,
            total_blocks_hit: inner.total_blocks_hit,
            current_burst_count: inner.current_burst_count,
            min_burst_before_block: inner.min_burst_before_block,
            max_burst_achieved: inner.max_burst_achieved,
            current_delay_ms: inner.current_delay.as_millis() as u64,
            current_cooldown_secs: inner.current_cooldown.as_secs(),
        }
    }
}
