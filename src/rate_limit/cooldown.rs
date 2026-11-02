use std::time::{Duration, Instant};

/// Baseline cooldown duration upon encountering the first rate limit block.
pub const BASE_COOLDOWN_SECS: u64 = 60;
/// Maximum duration for adaptive cooldown escalation.
/// Explicit upstream `Retry-After` headers can exceed this cap.
pub const MAX_COOLDOWN_SECS: u64 = 180;

pub(crate) struct CooldownTracker {
    pub(crate) until: Option<Instant>,
    pub(crate) duration: Duration,
    pub(crate) last_block_at: Option<Instant>,
}

impl CooldownTracker {
    pub(crate) fn new() -> Self {
        Self {
            until: None,
            duration: Duration::from_secs(BASE_COOLDOWN_SECS),
            last_block_at: None,
        }
    }

    pub(crate) fn remaining(&self, now: Instant) -> Option<Duration> {
        self.until
            .and_then(|u| (now < u).then(|| u.saturating_duration_since(now)))
    }

    pub(crate) fn restore_baseline_if_idle(&mut self, now: Instant) {
        if let Some(last_block) = self.last_block_at
            && now.saturating_duration_since(last_block) > Duration::from_secs(600)
        {
            self.duration = Duration::from_secs(BASE_COOLDOWN_SECS);
        }
    }

    pub(crate) fn escalate_and_trigger(
        &mut self,
        now: Instant,
        retry_after: Option<Duration>,
    ) -> Duration {
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
