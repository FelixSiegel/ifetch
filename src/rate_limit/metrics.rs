use std::time::Duration;

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
pub(crate) struct RateLimitMetrics {
    pub(crate) total_successful_chapters: usize,
    pub(crate) total_blocks_hit: usize,
    pub(crate) burst_permits_since_reset: usize,
    pub(crate) total_permits_since_reset: usize,
    pub(crate) last_total_permits_before_block: usize,
    pub(crate) last_burst_permits_before_block: usize,
    pub(crate) min_burst_before_block: Option<usize>,
    pub(crate) min_total_before_block: Option<usize>,
    pub(crate) max_burst_achieved: usize,
}

impl RateLimitMetrics {
    pub(crate) fn on_burst_granted(&mut self) {
        self.burst_permits_since_reset += 1;
        self.total_permits_since_reset += 1;
        self.max_burst_achieved = self.max_burst_achieved.max(self.burst_permits_since_reset);
    }

    pub(crate) fn record_block(&mut self) -> (usize, usize) {
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

    pub(crate) fn format_min_burst(&self) -> String {
        self.min_burst_before_block.map_or_else(
            || "unknown".to_string(),
            |t| format!("~{} burst chapters", t),
        )
    }

    pub(crate) fn format_min_total(&self) -> String {
        self.min_total_before_block.map_or_else(
            || "unknown".to_string(),
            |t| format!("~{} total chapters", t),
        )
    }

    pub(crate) fn snapshot(
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
