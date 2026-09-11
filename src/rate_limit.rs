use crate::server::helpers::lock_mutex;
use log::{debug, info, warn};
use std::{
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

pub const BURST_CAPACITY: usize = 10;
pub const BURST_REPLENISH_IDLE_SECS: u64 = 15;
pub const BASE_DELAY_MS: u64 = 200;
pub const MAX_DELAY_MS: u64 = 2000;
pub const DELAY_STEP_UP_MS: u64 = 150;
pub const DELAY_STEP_DOWN_MS: u64 = 25;
pub const CONSECUTIVE_SUCCESS_RELAX: usize = 20;
/// Baseline cooldown duration upon encountering the first rate limit block.
pub const BASE_COOLDOWN_SECS: u64 = 30;
/// Maximum duration for adaptive cooldown escalation.
/// Explicit upstream `Retry-After` headers can exceed this cap.
pub const MAX_COOLDOWN_SECS: u64 = 180;

#[derive(Debug, Clone)]
pub struct RateLimitStatsSnapshot {
    pub total_successful_chapters: usize,
    pub total_blocks_hit: usize,
    pub burst_permits_since_reset: usize,
    pub total_permits_since_reset: usize,
    pub last_total_permits_before_block: usize,
    pub last_burst_permits_before_block: usize,
    pub min_burst_before_block: Option<usize>,
    pub min_total_before_block: Option<usize>,
    pub max_burst_achieved: usize,
    pub current_delay_ms: u64,
    pub current_cooldown_secs: u64,
}

struct RateLimiterInner {
    /// Number of zero-delay tokens available for immediate burst downloads before pacing kicks in.
    burst_tokens: usize,
    /// Number of active worker threads currently waiting on scheduled paced slots.
    reserved_permits: usize,
    /// Timestamp when the last chapter download permit was actually granted (burst or paced).
    /// Used as the authority for idle replenishment detection.
    last_permit_granted_at: Instant,
    /// Paced schedule timeline pointer; subsequent paced permits are scheduled incrementally starting from this point.
    next_permit_at: Instant,
    /// Timestamp until which all chapter downloads are paused due to an active Cloudflare/HTTP rate limit block.
    cooldown_until: Option<Instant>,
    /// Current inter-chapter pacing delay, dynamically adjusted via additive increase on block (+150ms) and
    /// multiplicative decrease of excess delay above baseline on sustained success.
    current_delay: Duration,
    /// Duration of the cooldown period applied when a rate limit block occurs.
    /// Scaled adaptively up to `MAX_COOLDOWN_SECS` on repeated blocks, or set higher
    /// if an upstream `Retry-After` header specifies a larger duration.
    current_cooldown: Duration,
    /// Number of consecutive successful chapter downloads since the last delay relaxation or rate limit block.
    consecutive_successes: usize,
    /// Number of zero-delay burst tokens consumed in the current sequence.
    burst_permits_since_reset: usize,
    /// Total chapters actually granted a permit (burst + paced) since the last idle reset or block.
    total_permits_since_reset: usize,
    /// Total chapters granted a permit (burst + paced) before the most recent rate limit block.
    last_total_permits_before_block: usize,
    /// Zero-delay burst tokens consumed before the most recent rate limit block.
    last_burst_permits_before_block: usize,
    /// Monotonically incremented whenever the schedule timeline is reset (block hit, cooldown transition, or idle replenishment)
    /// to invalidate any outstanding sleeping reservations.
    schedule_generation: u64,
    /// Cumulative count of chapters successfully downloaded across the entire process lifetime.
    total_successful_chapters: usize,
    /// Cumulative count of Cloudflare challenge or HTTP 429 rate limit events encountered.
    total_blocks_hit: usize,
    /// Smallest zero-delay burst size observed immediately prior to encountering a rate limit block.
    min_burst_before_block: Option<usize>,
    /// Smallest total permits (burst + paced) observed immediately prior to encountering a rate limit block.
    min_total_before_block: Option<usize>,
    /// Maximum number of consecutive zero-delay burst tokens consumed in any burst sequence.
    max_burst_achieved: usize,
    /// Timestamp when the most recent rate limit block occurred, used for cooldown escalation and baseline recovery.
    last_block_at: Option<Instant>,
}

fn update_min(target: &mut Option<usize>, val: usize) {
    if val > 0 {
        *target = Some(target.map_or(val, |prev| prev.min(val)));
    }
}

/// Centralizes the cooldown expiration transition so both `wait_for_chapter_permit`
/// and `on_rate_limit_hit` share identical lifecycle semantics.
/// Returns `true` if cooldown was expired.
fn expire_cooldown_if_needed(inner: &mut RateLimiterInner, now: Instant) -> bool {
    if let Some(until) = inner.cooldown_until {
        if now >= until {
            inner.cooldown_until = None;
            inner.schedule_generation = inner.schedule_generation.wrapping_add(1);
            inner.next_permit_at = now;
            // Set to `until` (cooldown expiration instant) so true post-cooldown idle time
            // is measured accurately without stalling an extra 15s or causing clock skew.
            inner.last_permit_granted_at = until;
            info!("Rate limit cooldown expired. Resuming chapter downloads.");
            return true;
        }
    }
    false
}

/// Scope guard to ensure `reserved_permits` is decremented even if a thread panics
/// while waiting or holding a reservation.
struct PermitGuard<'a> {
    inner: &'a Mutex<RateLimiterInner>,
    active: bool,
}

impl<'a> Drop for PermitGuard<'a> {
    fn drop(&mut self) {
        if self.active {
            let mut inner = match self.inner.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            inner.reserved_permits = inner.reserved_permits.saturating_sub(1);
        }
    }
}

pub struct AdaptiveRateLimiter {
    inner: Mutex<RateLimiterInner>,
    cvar: Condvar,
}

impl Default for AdaptiveRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveRateLimiter {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            inner: Mutex::new(RateLimiterInner {
                burst_tokens: BURST_CAPACITY,
                reserved_permits: 0,
                last_permit_granted_at: now - Duration::from_secs(BURST_REPLENISH_IDLE_SECS + 1),
                next_permit_at: now,
                cooldown_until: None,
                current_delay: Duration::from_millis(BASE_DELAY_MS),
                current_cooldown: Duration::from_secs(BASE_COOLDOWN_SECS),
                consecutive_successes: 0,
                burst_permits_since_reset: 0,
                total_permits_since_reset: 0,
                last_total_permits_before_block: 0,
                last_burst_permits_before_block: 0,
                schedule_generation: 0,
                total_successful_chapters: 0,
                total_blocks_hit: 0,
                min_burst_before_block: None,
                min_total_before_block: None,
                max_burst_achieved: 0,
                last_block_at: None,
            }),
            cvar: Condvar::new(),
        }
    }

    /// Called before starting each chapter download.
    /// Manages burst permits, inter-chapter delays, and pauses during cooldown.
    ///
    /// # Invariants
    /// 1. `last_permit_granted_at` is updated and permit counters are incremented ONLY when
    ///    a chapter permit is actually granted.
    /// 2. `reserved_permits` strictly tracks threads waiting for paced permits.
    /// 3. Idleness means no queued permit reservations (`reserved_permits == 0`),
    ///    the pacing schedule has caught up (`now >= next_permit_at`),
    ///    no rate-limit cooldown is active, and no permit has been granted for at least `BURST_REPLENISH_IDLE_SECS`.
    /// 4. The mutex lock is retained across condition evaluation and handed directly to `Condvar::wait_timeout`.
    ///    Any change in schedule timeline increments `schedule_generation` and notifies waiting threads.
    pub fn wait_for_chapter_permit(&self) {
        let mut inner = lock_mutex(&self.inner);

        loop {
            let now = Instant::now();

            // Centralized cooldown expiration check
            if expire_cooldown_if_needed(&mut inner, now) {
                self.cvar.notify_all();
            }

            if let Some(until) = inner.cooldown_until {
                let sleep_dur = until.saturating_duration_since(now);
                let (guard, _) = self
                    .cvar
                    .wait_timeout(inner, sleep_dur)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                inner = guard;
                continue;
            }

            // Baseline recovery check: if 5 minutes elapsed without any rate limit blocks,
            // return delay and cooldown to initial baseline.
            if inner
                .last_block_at
                .is_some_and(|t| now.saturating_duration_since(t) >= Duration::from_secs(300))
            {
                inner.current_cooldown = Duration::from_secs(BASE_COOLDOWN_SECS);
                inner.current_delay = Duration::from_millis(BASE_DELAY_MS);
                inner.last_block_at = None;
                info!(
                    "Rate limiter fully recovered to baseline delay and cooldown after 5 minutes of stability."
                );
            }

            // Idle replenishment check:
            // Limiter is idle when no threads are waiting on reservations, schedule has elapsed,
            // and wall-clock duration since the last granted permit exceeds `BURST_REPLENISH_IDLE_SECS`.
            if inner.reserved_permits == 0
                && now >= inner.next_permit_at
                && now.saturating_duration_since(inner.last_permit_granted_at)
                    >= Duration::from_secs(BURST_REPLENISH_IDLE_SECS)
            {
                if inner.burst_tokens < BURST_CAPACITY {
                    debug!(
                        "Downloader was idle for {:?}. Replenished burst tokens to {}",
                        now.saturating_duration_since(inner.last_permit_granted_at),
                        BURST_CAPACITY
                    );
                }
                inner.burst_tokens = BURST_CAPACITY;
                inner.burst_permits_since_reset = 0;
                inner.total_permits_since_reset = 0;
                inner.next_permit_at = now;
                inner.schedule_generation = inner.schedule_generation.wrapping_add(1);
            }

            // Burst permit: granted immediately with 0 delay
            if inner.burst_tokens > 0 {
                inner.burst_tokens -= 1;
                inner.burst_permits_since_reset += 1;
                inner.total_permits_since_reset += 1;
                inner.max_burst_achieved = inner
                    .max_burst_achieved
                    .max(inner.burst_permits_since_reset);
                inner.last_permit_granted_at = now;
                inner.next_permit_at = inner.next_permit_at.max(now);
                debug!(
                    "Burst token granted ({} remaining in burst)",
                    inner.burst_tokens
                );
                return;
            }

            // Paced permit: calculate next scheduled slot authoritative to this thread
            let slot = inner.next_permit_at.max(now);
            let scheduled_time = slot
                .checked_add(inner.current_delay)
                .expect("rate limiter schedule timeline overflowed Instant representable range");
            inner.next_permit_at = scheduled_time;
            inner.reserved_permits += 1;

            let my_gen = inner.schedule_generation;
            let wait_dur = slot.saturating_duration_since(now);

            // Arm scope guard for panic safety
            let mut guard_tracker = PermitGuard {
                inner: &self.inner,
                active: true,
            };

            // Wait on condvar while holding the lock; lock is automatically re-acquired when wait_timeout returns
            let mut guard = if wait_dur > Duration::ZERO {
                let (g, _) = self
                    .cvar
                    .wait_timeout(inner, wait_dur)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                g
            } else {
                inner
            };

            guard.reserved_permits = guard.reserved_permits.saturating_sub(1);
            guard_tracker.active = false;

            // Verify that the schedule generation that granted this reservation is still authoritative
            // and no new cooldown was triggered while waiting
            if guard.schedule_generation != my_gen || guard.cooldown_until.is_some() {
                inner = guard;
                continue;
            }

            let finish_now = Instant::now();
            guard.last_permit_granted_at = finish_now;
            guard.total_permits_since_reset += 1;
            return;
        }
    }

    /// Called when a chapter download succeeds.
    /// Updates statistics and gradually relaxes the inter-chapter delay via AIMD.
    pub fn on_chapter_success(&self) {
        let mut inner = lock_mutex(&self.inner);
        inner.total_successful_chapters += 1;
        inner.consecutive_successes += 1;

        // AIMD: Multiplicative decrease of excess delay above baseline every 20 successful chapters
        if inner.consecutive_successes >= CONSECUTIVE_SUCCESS_RELAX {
            inner.consecutive_successes = 0;
            let min_delay = Duration::from_millis(BASE_DELAY_MS);
            if inner.current_delay > min_delay {
                let excess = inner.current_delay.saturating_sub(min_delay);
                let step = (excess / 4).max(Duration::from_millis(DELAY_STEP_DOWN_MS));
                inner.current_delay = inner.current_delay.saturating_sub(step).max(min_delay);
                debug!(
                    "Relaxed adaptive chapter delay to {:?}",
                    inner.current_delay
                );
            }

            // Also gradually relax escalated cooldown towards BASE_COOLDOWN_SECS on sustained success
            let base_cooldown = Duration::from_secs(BASE_COOLDOWN_SECS);
            if inner.current_cooldown > base_cooldown {
                inner.current_cooldown = inner
                    .current_cooldown
                    .saturating_sub(Duration::from_secs(10))
                    .max(base_cooldown);
            }
        }
    }

    /// Called when a rate limit or Cloudflare challenge is encountered.
    /// Activates a synchronized cooldown, increases the inter-chapter delay,
    /// and records empirical statistics on limits.
    pub fn on_rate_limit_hit(&self, reason: &str) -> Duration {
        self.on_rate_limit_hit_with_retry(reason, None)
    }

    /// Called when a rate limit or Cloudflare challenge is encountered, optionally with a Retry-After duration.
    pub fn on_rate_limit_hit_with_retry(
        &self,
        reason: &str,
        retry_after: Option<Duration>,
    ) -> Duration {
        let mut inner = lock_mutex(&self.inner);
        let now = Instant::now();

        if expire_cooldown_if_needed(&mut inner, now) {
            self.cvar.notify_all();
        }

        // If already in an active cooldown, do not escalate penalties; return remaining duration
        if let Some(until) = inner.cooldown_until {
            debug!(
                "Concurrent rate limit hit detected (reason: {}). Already in active cooldown, skipping duplicate escalation.",
                reason
            );
            return until.saturating_duration_since(now);
        }

        let total_at_block = inner.total_permits_since_reset;
        let burst_at_block = inner.burst_permits_since_reset;
        inner.total_blocks_hit += 1;
        inner.last_total_permits_before_block = total_at_block;
        inner.last_burst_permits_before_block = burst_at_block;
        inner.schedule_generation = inner.schedule_generation.wrapping_add(1);

        update_min(&mut inner.min_burst_before_block, burst_at_block);
        update_min(&mut inner.min_total_before_block, total_at_block);

        // Compute adaptive cooldown: escalate by 1.5x if blocked repeatedly in under 5 minutes, capped at MAX_COOLDOWN_SECS
        let calculated_cooldown = match inner.last_block_at {
            Some(last) if now.saturating_duration_since(last) < Duration::from_secs(300) => {
                inner.current_cooldown.mul_f64(1.5).clamp(
                    Duration::from_secs(BASE_COOLDOWN_SECS),
                    inner
                        .current_cooldown
                        .max(Duration::from_secs(MAX_COOLDOWN_SECS)), // retry header can force a cooldown longer than MAX_COOLDOWN_SECS
                )
            }
            _ => Duration::from_secs(BASE_COOLDOWN_SECS),
        };

        // If upstream provided an explicit Retry-After duration, respect it (can exceed MAX_COOLDOWN_SECS)
        let cooldown = match retry_after {
            Some(ra) => calculated_cooldown.max(ra),
            None => calculated_cooldown,
        };

        inner.current_cooldown = cooldown;
        inner.last_block_at = Some(now);
        inner.cooldown_until = Some(now + cooldown);
        inner.next_permit_at = now + cooldown;
        inner.last_permit_granted_at = now;

        // AIMD: Additive increase of delay on block (+150ms)
        inner.current_delay = (inner.current_delay + Duration::from_millis(DELAY_STEP_UP_MS))
            .min(Duration::from_millis(MAX_DELAY_MS));

        inner.burst_tokens = 0;
        inner.burst_permits_since_reset = 0;
        inner.total_permits_since_reset = 0;
        inner.consecutive_successes = 0;

        let min_burst_str = inner
            .min_burst_before_block
            .map(|t| format!("~{} burst chapters", t))
            .unwrap_or_else(|| "unknown".to_string());

        let min_total_str = inner
            .min_total_before_block
            .map(|t| format!("~{} total chapters", t))
            .unwrap_or_else(|| "unknown".to_string());

        warn!(
            "Cloudflare/MangaKatana rate limit hit! Reason: {}. {} total permits granted ({} in burst) before block. Entering {:?} cooldown. Adaptive delay increased to {:?}. Smallest observed burst: {}, total: {}. Total blocks: {}.",
            reason,
            total_at_block,
            burst_at_block,
            cooldown,
            inner.current_delay,
            min_burst_str,
            min_total_str,
            inner.total_blocks_hit
        );

        // Immediately wake any sleeping threads so they can notice the block and abort obsolete reservations
        drop(inner);
        self.cvar.notify_all();

        cooldown
    }

    /// Snapshot current statistics for logging and reporting
    pub fn get_stats(&self) -> RateLimitStatsSnapshot {
        let inner = lock_mutex(&self.inner);
        RateLimitStatsSnapshot {
            total_successful_chapters: inner.total_successful_chapters,
            total_blocks_hit: inner.total_blocks_hit,
            burst_permits_since_reset: inner.burst_permits_since_reset,
            total_permits_since_reset: inner.total_permits_since_reset,
            last_total_permits_before_block: inner.last_total_permits_before_block,
            last_burst_permits_before_block: inner.last_burst_permits_before_block,
            min_burst_before_block: inner.min_burst_before_block,
            min_total_before_block: inner.min_total_before_block,
            max_burst_achieved: inner.max_burst_achieved,
            current_delay_ms: inner.current_delay.as_millis() as u64,
            current_cooldown_secs: inner.current_cooldown.as_secs(),
        }
    }
}

/// Category of rate limiting event encountered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitKind {
    Http429,
    CloudflareChallenge,
    Other,
}

/// Dedicated typed error representing an HTTP rate limit or Cloudflare challenge.
#[derive(Debug, Clone)]
pub struct RateLimitError {
    pub kind: RateLimitKind,
    pub status: Option<reqwest::StatusCode>,
    pub retry_after: Option<Duration>,
    pub reason: String,
}

impl RateLimitError {
    pub fn new(status: Option<reqwest::StatusCode>, reason: impl Into<String>) -> Self {
        Self::with_retry(status, reason, None)
    }

    pub fn with_retry(
        status: Option<reqwest::StatusCode>,
        reason: impl Into<String>,
        retry_after: Option<Duration>,
    ) -> Self {
        let reason_str = reason.into();
        let kind = if status == Some(reqwest::StatusCode::TOO_MANY_REQUESTS) {
            RateLimitKind::Http429
        } else if reason_str.to_ascii_lowercase().contains("cloudflare") {
            RateLimitKind::CloudflareChallenge
        } else {
            RateLimitKind::Other
        };
        Self {
            kind,
            status,
            retry_after,
            reason: reason_str,
        }
    }

    pub fn with_kind(
        status: Option<reqwest::StatusCode>,
        reason: impl Into<String>,
        kind: RateLimitKind,
    ) -> Self {
        Self {
            kind,
            status,
            retry_after: None,
            reason: reason.into(),
        }
    }

    pub fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }

    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }
}

impl std::fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(
                f,
                "Rate limited by Cloudflare/MangaKatana (status {}): {}",
                status, self.reason
            ),
            None => write!(f, "Rate limited by Cloudflare/MangaKatana: {}", self.reason),
        }
    }
}

impl std::error::Error for RateLimitError {}

/// Checks whether an error represents a Cloudflare challenge or HTTP rate limit
/// by inspecting typed RateLimitErrors, reqwest status codes, and error chains.
pub fn is_rate_limit_error(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if cause.downcast_ref::<RateLimitError>().is_some() {
            return true;
        }

        if let Some(req_err) = cause.downcast_ref::<reqwest::Error>() {
            if let Some(status) = req_err.status() {
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    return true;
                }
            }
        }
    }

    let msg = format!("{:?}", e);
    let lower = msg.to_ascii_lowercase();
    lower.contains("cf-chl-bypass")
        || lower.contains("challenge-platform")
        || lower.contains("just a moment...")
        || (lower.contains("turnstile") && lower.contains("challenge"))
        || lower.contains("rate limited")
        || lower.contains("429 too many requests")
}

/// Helper to extract an explicit Retry-After duration if present in the error chain.
pub fn extract_retry_after(e: &anyhow::Error) -> Option<Duration> {
    e.chain()
        .find_map(|c| c.downcast_ref::<RateLimitError>())
        .and_then(|rle| rle.retry_after())
}
