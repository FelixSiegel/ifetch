use crate::utils::lock_mutex;
use log::{debug, info, warn};
use std::{
    sync::{Condvar, Mutex, MutexGuard},
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

#[derive(Default)]
struct RateLimitMetrics {
    total_successful_chapters: usize,
    total_blocks_hit: usize,
    burst_permits_since_reset: usize,
    total_permits_since_reset: usize,
    last_total_permits_before_block: usize,
    last_burst_permits_before_block: usize,
    min_burst_before_block: Option<usize>,
    min_total_before_block: Option<usize>,
    max_burst_achieved: usize,
}

impl RateLimitMetrics {
    fn on_burst_granted(&mut self) {
        self.burst_permits_since_reset += 1;
        self.total_permits_since_reset += 1;
        self.max_burst_achieved = self.max_burst_achieved.max(self.burst_permits_since_reset);
    }

    fn record_block(&mut self) -> (usize, usize) {
        let total_at_block = self.total_permits_since_reset;
        let burst_at_block = self.burst_permits_since_reset;
        self.total_blocks_hit += 1;
        self.last_total_permits_before_block = total_at_block;
        self.last_burst_permits_before_block = burst_at_block;

        update_min(&mut self.min_burst_before_block, burst_at_block);
        update_min(&mut self.min_total_before_block, total_at_block);

        self.burst_permits_since_reset = 0;
        self.total_permits_since_reset = 0;

        (total_at_block, burst_at_block)
    }

    fn format_min_burst(&self) -> String {
        self.min_burst_before_block.map_or_else(
            || "unknown".to_string(),
            |t| format!("~{} burst chapters", t),
        )
    }

    fn format_min_total(&self) -> String {
        self.min_total_before_block.map_or_else(
            || "unknown".to_string(),
            |t| format!("~{} total chapters", t),
        )
    }

    fn snapshot(
        &self,
        current_delay: Duration,
        current_cooldown: Duration,
    ) -> RateLimitStatsSnapshot {
        RateLimitStatsSnapshot {
            total_successful_chapters: self.total_successful_chapters,
            total_blocks_hit: self.total_blocks_hit,
            burst_permits_since_reset: self.burst_permits_since_reset,
            total_permits_since_reset: self.total_permits_since_reset,
            last_total_permits_before_block: self.last_total_permits_before_block,
            last_burst_permits_before_block: self.last_burst_permits_before_block,
            min_burst_before_block: self.min_burst_before_block,
            min_total_before_block: self.min_total_before_block,
            max_burst_achieved: self.max_burst_achieved,
            current_delay_ms: current_delay.as_millis() as u64,
            current_cooldown_secs: current_cooldown.as_secs(),
        }
    }
}

fn update_min(target: &mut Option<usize>, val: usize) {
    if val > 0 {
        *target = Some(target.map_or(val, |prev| prev.min(val)));
    }
}

struct CooldownTracker {
    until: Option<Instant>,
    duration: Duration,
    last_block_at: Option<Instant>,
}

impl CooldownTracker {
    fn new() -> Self {
        Self {
            until: None,
            duration: Duration::from_secs(BASE_COOLDOWN_SECS),
            last_block_at: None,
        }
    }

    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.until
            .and_then(|u| (now < u).then(|| u.saturating_duration_since(now)))
    }

    fn restore_baseline_if_idle(&mut self, now: Instant) {
        if let Some(last_block) = self.last_block_at
            && now.saturating_duration_since(last_block) > Duration::from_secs(600)
        {
            self.duration = Duration::from_secs(BASE_COOLDOWN_SECS);
        }
    }

    fn escalate_and_trigger(&mut self, now: Instant, retry_after: Option<Duration>) -> Duration {
        let calculated = match self.last_block_at {
            Some(last) if now.saturating_duration_since(last) < Duration::from_secs(300) => {
                self.duration.mul_f64(1.5).clamp(
                    Duration::from_secs(BASE_COOLDOWN_SECS),
                    self.duration.max(Duration::from_secs(MAX_COOLDOWN_SECS)),
                )
            }
            _ => Duration::from_secs(BASE_COOLDOWN_SECS),
        };

        let cooldown = retry_after.map_or(calculated, |ra| calculated.max(ra));
        self.duration = cooldown;
        self.last_block_at = Some(now);
        self.until = Some(now + cooldown);
        cooldown
    }
}

struct RateLimiterInner {
    burst_tokens: usize,
    reserved_permits: usize,
    last_permit_granted_at: Instant,
    next_permit_at: Instant,
    current_delay: Duration,
    consecutive_successes: usize,
    schedule_generation: u64,
    cooldown: CooldownTracker,
    metrics: RateLimitMetrics,
}

impl RateLimiterInner {
    fn new(now: Instant) -> Self {
        Self {
            burst_tokens: BURST_CAPACITY,
            reserved_permits: 0,
            last_permit_granted_at: now,
            next_permit_at: now,
            current_delay: Duration::from_millis(BASE_DELAY_MS),
            consecutive_successes: 0,
            schedule_generation: 0,
            cooldown: CooldownTracker::new(),
            metrics: RateLimitMetrics::default(),
        }
    }

    fn expire_cooldown_if_needed(&mut self, now: Instant) {
        if let Some(until) = self.cooldown.until
            && now >= until
        {
            self.cooldown.until = None;
            self.schedule_generation = self.schedule_generation.wrapping_add(1);
            self.next_permit_at = now;
            self.last_permit_granted_at = until;
            info!("Rate limit cooldown expired. Resuming operations.");
        }
    }

    fn replenish_burst_if_idle(&mut self, now: Instant) {
        if self.reserved_permits != 0
            || now.saturating_duration_since(self.last_permit_granted_at)
                < Duration::from_secs(BURST_REPLENISH_IDLE_SECS)
        {
            return;
        }

        if self.burst_tokens < BURST_CAPACITY {
            debug!(
                "Replenishing burst tokens from {} to {} (idle for >= {}s)",
                self.burst_tokens, BURST_CAPACITY, BURST_REPLENISH_IDLE_SECS
            );
            self.burst_tokens = BURST_CAPACITY;
            self.next_permit_at = now;
            self.schedule_generation = self.schedule_generation.wrapping_add(1);
            self.metrics.burst_permits_since_reset = 0;
            self.metrics.total_permits_since_reset = 0;
        }

        self.cooldown.restore_baseline_if_idle(now);
    }

    fn try_acquire_burst_token(&mut self, now: Instant) -> bool {
        if self.burst_tokens > 0 && self.reserved_permits == 0 {
            self.burst_tokens -= 1;
            self.last_permit_granted_at = now;
            self.next_permit_at = now;
            self.metrics.on_burst_granted();
            true
        } else {
            false
        }
    }

    fn relax_delay_if_sustained(&mut self) {
        self.consecutive_successes += 1;
        if self.consecutive_successes < CONSECUTIVE_SUCCESS_RELAX {
            return;
        }
        self.consecutive_successes = 0;
        let current_ms = self.current_delay.as_millis() as u64;
        if current_ms > BASE_DELAY_MS {
            let excess = current_ms.saturating_sub(BASE_DELAY_MS);
            let reduction = (excess / 4).max(DELAY_STEP_DOWN_MS);
            let new_ms = current_ms.saturating_sub(reduction).max(BASE_DELAY_MS);
            self.current_delay = Duration::from_millis(new_ms);
            debug!(
                "Adaptive rate limiter relaxed delay to {:?} after sustained success",
                self.current_delay
            );
        }
    }

    fn record_block(&mut self, now: Instant, cooldown: Duration) -> (usize, usize) {
        self.schedule_generation = self.schedule_generation.wrapping_add(1);
        self.next_permit_at = now + cooldown;
        self.last_permit_granted_at = now;

        // AIMD: Additive increase of delay on block (+150ms)
        self.current_delay = (self.current_delay + Duration::from_millis(DELAY_STEP_UP_MS))
            .min(Duration::from_millis(MAX_DELAY_MS));

        self.burst_tokens = 0;
        self.consecutive_successes = 0;

        self.metrics.record_block()
    }
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
        Self {
            inner: Mutex::new(RateLimiterInner::new(Instant::now())),
            cvar: Condvar::new(),
        }
    }

    fn wait_timeout<'a>(
        &self,
        guard: MutexGuard<'a, RateLimiterInner>,
        dur: Duration,
    ) -> MutexGuard<'a, RateLimiterInner> {
        self.cvar
            .wait_timeout(guard, dur)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0
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
            inner.expire_cooldown_if_needed(now);

            // Active cooldown: sleep until expiration or until woken up by notify_all()
            if let Some(wait_dur) = inner.cooldown.remaining(now) {
                inner = self.wait_timeout(inner, wait_dur);
                inner.expire_cooldown_if_needed(Instant::now());
                continue;
            }

            let now = Instant::now();
            inner.replenish_burst_if_idle(now);

            // Path 1: Fast zero-delay burst token available
            if inner.try_acquire_burst_token(now) {
                return;
            }

            // Path 2: Scheduled pacing
            let scheduled_at = inner.next_permit_at.max(now) + inner.current_delay;
            inner.next_permit_at = scheduled_at;
            inner.reserved_permits += 1;
            let my_generation = inner.schedule_generation;

            while Instant::now() < scheduled_at {
                let wait_rem = scheduled_at.saturating_duration_since(Instant::now());
                if wait_rem.is_zero() {
                    break;
                }
                inner = self.wait_timeout(inner, wait_rem);
                if inner.schedule_generation != my_generation {
                    inner.reserved_permits = inner.reserved_permits.saturating_sub(1);
                    break;
                }
            }

            if inner.schedule_generation == my_generation {
                inner.reserved_permits = inner.reserved_permits.saturating_sub(1);
                inner.last_permit_granted_at = Instant::now();
                inner.metrics.total_permits_since_reset += 1;
                return;
            }
        }
    }

    /// Records a successful chapter download.
    ///
    /// Slices adaptive delay back toward `BASE_DELAY_MS` every 20 consecutive successes.
    pub fn on_chapter_success(&self) {
        let mut inner = lock_mutex(&self.inner);
        inner.metrics.total_successful_chapters += 1;
        inner.relax_delay_if_sustained();
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
        inner.expire_cooldown_if_needed(now);

        // If already in an active cooldown that hasn't expired yet, do not re-escalate cooldown or delay
        if let Some(remaining) = inner.cooldown.remaining(now) {
            debug!(
                "Concurrent rate limit hit detected (reason: {}). Already in active cooldown, skipping duplicate escalation.",
                reason
            );
            return remaining;
        }

        let cooldown = inner.cooldown.escalate_and_trigger(now, retry_after);
        let (total_at_block, burst_at_block) = inner.record_block(now, cooldown);

        warn!(
            "Cloudflare/MangaKatana rate limit hit! Reason: {}. {} total permits granted ({} in burst) before block. Entering {:?} cooldown. Adaptive delay increased to {:?}. Smallest observed burst: {}, total: {}. Total blocks: {}.",
            reason,
            total_at_block,
            burst_at_block,
            cooldown,
            inner.current_delay,
            inner.metrics.format_min_burst(),
            inner.metrics.format_min_total(),
            inner.metrics.total_blocks_hit
        );

        // Immediately wake any sleeping threads so they can notice the block and abort obsolete reservations
        drop(inner);
        self.cvar.notify_all();

        cooldown
    }

    /// Snapshot current statistics for logging and reporting
    pub fn get_stats(&self) -> RateLimitStatsSnapshot {
        let inner = lock_mutex(&self.inner);
        inner
            .metrics
            .snapshot(inner.current_delay, inner.cooldown.duration)
    }
}
