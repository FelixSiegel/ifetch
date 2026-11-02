use crate::utils::lock_mutex;
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
pub const BASE_COOLDOWN_SECS: u64 = 60;
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
    if let Some(until) = inner.cooldown_until
        && now >= until
    {
        inner.cooldown_until = None;
        inner.schedule_generation = inner.schedule_generation.wrapping_add(1);
        inner.next_permit_at = now;
        // Set to `until` (cooldown expiration instant) so true post-cooldown idle time
        // is measured accurately without stalling an extra 15s or causing clock skew.
        inner.last_permit_granted_at = until;
        info!("Rate limit cooldown expired. Resuming operations.");
        return true;
    }
    false
}

/// Dynamic, thread-safe rate limiter balancing fast burst downloads for single chapter reading
/// with adaptive paced delays and automatic cooldown backoff for bulk downloads.
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
                last_permit_granted_at: now,
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

    /// Blocks until a permit is granted to start downloading a chapter.
    ///
    /// 1. If currently in a cooldown, waits until the cooldown period expires.
    /// 2. If idle for $\ge 15\text{s}$, replenishes zero-delay burst tokens (up to 10).
    /// 3. If burst tokens are available and no worker threads are queued, grants immediate zero-delay execution.
    /// 4. Once burst tokens are exhausted, schedules permits consecutively at `current_delay` intervals.
    pub fn wait_for_chapter_permit(&self) {
        let mut inner = lock_mutex(&self.inner);

        loop {
            let now = Instant::now();
            let _ = expire_cooldown_if_needed(&mut inner, now);

            // Active cooldown: sleep until expiration or until woken up by notify_all()
            if let Some(until) = inner.cooldown_until {
                if now < until {
                    let wait_dur = until.saturating_duration_since(now);
                    let (new_inner, _) = self
                        .cvar
                        .wait_timeout(inner, wait_dur)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    inner = new_inner;
                    continue;
                }
                let _ = expire_cooldown_if_needed(&mut inner, Instant::now());
            }

            let now = Instant::now();

            // Replenish burst tokens if idle for >= 15s and no reserved permits are queued
            if inner.reserved_permits == 0
                && now.saturating_duration_since(inner.last_permit_granted_at)
                    >= Duration::from_secs(BURST_REPLENISH_IDLE_SECS)
            {
                if inner.burst_tokens < BURST_CAPACITY {
                    debug!(
                        "Replenishing burst tokens from {} to {} (idle for >= {}s)",
                        inner.burst_tokens, BURST_CAPACITY, BURST_REPLENISH_IDLE_SECS
                    );
                    inner.burst_tokens = BURST_CAPACITY;
                    inner.burst_permits_since_reset = 0;
                    inner.total_permits_since_reset = 0;
                    inner.next_permit_at = now;
                    inner.schedule_generation = inner.schedule_generation.wrapping_add(1);
                }

                // If no blocks have occurred for > 10 minutes, gradually restore cooldown baseline
                if let Some(last_block) = inner.last_block_at
                    && now.saturating_duration_since(last_block) > Duration::from_secs(600)
                {
                    inner.current_cooldown = Duration::from_secs(BASE_COOLDOWN_SECS);
                }
            }

            // Path 1: Fast zero-delay burst token available
            if inner.burst_tokens > 0 && inner.reserved_permits == 0 {
                inner.burst_tokens -= 1;
                inner.burst_permits_since_reset += 1;
                inner.total_permits_since_reset += 1;
                inner.max_burst_achieved = inner
                    .max_burst_achieved
                    .max(inner.burst_permits_since_reset);
                inner.last_permit_granted_at = now;
                inner.next_permit_at = now;
                return;
            }

            // Path 2: Scheduled pacing
            let scheduled_at = if inner.next_permit_at <= now {
                now + inner.current_delay
            } else {
                inner.next_permit_at + inner.current_delay
            };
            inner.next_permit_at = scheduled_at;
            inner.reserved_permits += 1;

            let my_generation = inner.schedule_generation;

            while Instant::now() < scheduled_at {
                let wait_rem = scheduled_at.saturating_duration_since(Instant::now());
                if wait_rem.is_zero() {
                    break;
                }

                let (new_inner, _) = self
                    .cvar
                    .wait_timeout(inner, wait_rem)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                inner = new_inner;

                // If schedule generation changed (e.g. rate limit hit or cooldown started),
                // our scheduled slot is invalidated; decrement reserved permit and re-evaluate
                if inner.schedule_generation != my_generation {
                    inner.reserved_permits = inner.reserved_permits.saturating_sub(1);
                    break;
                }
            }

            if inner.schedule_generation == my_generation {
                inner.reserved_permits = inner.reserved_permits.saturating_sub(1);
                let grant_now = Instant::now();
                inner.last_permit_granted_at = grant_now;
                inner.total_permits_since_reset += 1;
                return;
            }
        }
    }

    /// Records a successful chapter download.
    ///
    /// Slices adaptive delay back toward `BASE_DELAY_MS` every 20 consecutive successes.
    pub fn on_chapter_success(&self) {
        let mut inner = lock_mutex(&self.inner);
        inner.total_successful_chapters += 1;
        inner.consecutive_successes += 1;

        if inner.consecutive_successes >= CONSECUTIVE_SUCCESS_RELAX {
            inner.consecutive_successes = 0;
            let current_ms = inner.current_delay.as_millis() as u64;
            if current_ms > BASE_DELAY_MS {
                let excess = current_ms.saturating_sub(BASE_DELAY_MS);
                let reduction = (excess / 4).max(DELAY_STEP_DOWN_MS);
                let new_ms = current_ms.saturating_sub(reduction).max(BASE_DELAY_MS);
                inner.current_delay = Duration::from_millis(new_ms);
                debug!(
                    "Adaptive rate limiter relaxed delay to {:?} after sustained success",
                    inner.current_delay
                );
            }
        }
    }

    /// Convenience wrapper for `on_rate_limit_hit_with_retry` without an explicit `Retry-After` duration.
    pub fn on_rate_limit_hit(&self, reason: &str) -> Duration {
        self.on_rate_limit_hit_with_retry(reason, None)
    }

    /// Records a rate limit or anti-bot block.
    ///
    /// Enters a cooldown period, resets burst tokens, steps up the adaptive pacing delay (+150ms),
    /// and wakes all queued worker threads.
    pub fn on_rate_limit_hit_with_retry(
        &self,
        reason: &str,
        retry_after: Option<Duration>,
    ) -> Duration {
        let mut inner = lock_mutex(&self.inner);
        let now = Instant::now();

        // Expire any existing cooldown if its time has elapsed
        let _ = expire_cooldown_if_needed(&mut inner, now);

        // If already in an active cooldown that hasn't expired yet, do not re-escalate cooldown or delay
        if let Some(until) = inner.cooldown_until
            && now < until
        {
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
