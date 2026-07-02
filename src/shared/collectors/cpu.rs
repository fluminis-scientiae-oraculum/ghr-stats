//! Shared CPU%-from-cgroup-usage rate tracker.
//!
//! cgroup `cpu.stat` exposes a cumulative `usage_usec` counter; a percentage
//! only exists relative to a previous observation. Both the collector daemon
//! and the live TUI keep one of these so the delta math lives in a single,
//! tested place.

use std::collections::HashMap;
use std::time::Instant;

#[derive(Default)]
pub struct CpuRateTracker {
    prev: HashMap<i64, (u64, Instant)>,
}

impl CpuRateTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// CPU percent for `id` since its previous observation. `None` on the first
    /// sample, when usage is unavailable, or when the counter appears to reset
    /// (which also clears stale state). May exceed 100% across multiple cores.
    pub fn rate(&mut self, id: i64, usage_usec: Option<u64>, at: Instant) -> Option<f32> {
        let Some(cur) = usage_usec else {
            self.prev.remove(&id);
            return None;
        };
        let pct = self.prev.get(&id).and_then(|(prev_usec, prev_at)| {
            let dt = at.duration_since(*prev_at).as_secs_f64();
            (dt > 0.0 && cur >= *prev_usec)
                .then(|| ((cur - prev_usec) as f64 / 1_000_000.0 / dt * 100.0) as f32)
        });
        self.prev.insert(id, (cur, at));
        pct
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn first_sample_is_none_then_percent_is_computed() {
        let mut t = CpuRateTracker::new();
        let t0 = Instant::now();
        assert_eq!(t.rate(1, Some(0), t0), None);

        // +500_000 µs of CPU over 1 s wall = 50%.
        let t1 = t0 + Duration::from_secs(1);
        let pct = t.rate(1, Some(500_000), t1).expect("second sample");
        assert!((pct - 50.0).abs() < 0.01, "got {pct}");
    }

    #[test]
    fn counter_reset_and_missing_usage_yield_none() {
        let mut t = CpuRateTracker::new();
        let t0 = Instant::now();
        t.rate(1, Some(1_000_000), t0);
        // A lower value than before ⇒ treated as a reset ⇒ None.
        assert_eq!(t.rate(1, Some(10), t0 + Duration::from_secs(1)), None);
        // Missing usage clears state and returns None.
        assert_eq!(t.rate(1, None, t0 + Duration::from_secs(2)), None);
    }
}
