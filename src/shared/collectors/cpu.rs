//! Shared CPU%-from-cgroup-usage rate tracker.
//!
//! cgroup `cpu.stat` exposes a cumulative `usage_usec` counter; a percentage
//! only exists relative to a previous observation. Both the collector daemon
//! and the live TUI keep one of these so the delta math lives in a single,
//! tested place.
//!
//! The tracker is keyed by a caller-chosen identity `K` that MUST be locally
//! unique per runner. The install directory (`PathBuf`, the default) is — every
//! runner on a host has its own. GitHub's numeric `agentId` is NOT: it is unique
//! only *within* an org (or repo), so two runners registered in different orgs
//! can share one. Keying by `agentId` made those two runners alternate their
//! *different* cgroup counters under one key microseconds apart, so a normal
//! delta divided by a near-zero `dt` produced wildly inflated percentages (an
//! idle runner rendering as 785% CPU). Key by the dir; the counters stay apart.

use std::collections::HashMap;
use std::hash::Hash;
use std::path::PathBuf;
use std::time::Instant;

pub struct CpuRateTracker<K: Eq + Hash = PathBuf> {
    prev: HashMap<K, (u64, Instant)>,
}

impl<K: Eq + Hash> Default for CpuRateTracker<K> {
    fn default() -> Self {
        Self {
            prev: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash> CpuRateTracker<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// CPU percent for `id` since its previous observation. `None` on the first
    /// sample, when usage is unavailable, or when the counter appears to reset
    /// (which also clears stale state). May exceed 100% across multiple cores.
    pub fn rate(&mut self, id: K, usage_usec: Option<u64>, at: Instant) -> Option<f32> {
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
        let mut t = CpuRateTracker::<i64>::new();
        let t0 = Instant::now();
        assert_eq!(t.rate(1, Some(0), t0), None);

        // +500_000 µs of CPU over 1 s wall = 50%.
        let t1 = t0 + Duration::from_secs(1);
        let pct = t.rate(1, Some(500_000), t1).expect("second sample");
        assert!((pct - 50.0).abs() < 0.01, "got {pct}");
    }

    #[test]
    fn counter_reset_and_missing_usage_yield_none() {
        let mut t = CpuRateTracker::<i64>::new();
        let t0 = Instant::now();
        t.rate(1, Some(1_000_000), t0);
        // A lower value than before ⇒ treated as a reset ⇒ None.
        assert_eq!(t.rate(1, Some(10), t0 + Duration::from_secs(1)), None);
        // Missing usage clears state and returns None.
        assert_eq!(t.rate(1, None, t0 + Duration::from_secs(2)), None);
    }

    #[test]
    fn distinct_keys_never_cross_contaminate_within_a_tick() {
        // Regression: two runners that collide on GitHub `agentId` but have
        // distinct install dirs. Keyed by dir, each runner's cumulative cgroup
        // counter stays its own — even when the two are sampled microseconds
        // apart in the same tick, the shape that produced the 785% bug when a
        // single shared `agentId` key alternated between two different counters.
        let mut t = CpuRateTracker::<PathBuf>::new();
        let a = PathBuf::from("/srv/runners/org-a-runner-1"); // cumulative ~20_000_000 µs
        let b = PathBuf::from("/srv/runners/org-b-runner-1"); // cumulative ~99_000_000 µs
        let t0 = Instant::now();

        // First tick: both seed, both None (no prior observation).
        assert_eq!(t.rate(a.clone(), Some(20_000_000), t0), None);
        assert_eq!(
            t.rate(b.clone(), Some(99_000_000), t0 + Duration::from_micros(50)),
            None
        );

        // Second tick, 5 s later: each advanced by 250_000 µs of its OWN counter
        // ⇒ 5% each. If the two counters were conflated under one key, `b` would
        // read the (99_250_000 − 20_250_000) gap over a ~microsecond dt and blow
        // up to millions of percent.
        let t1 = t0 + Duration::from_secs(5);
        let ra = t.rate(a.clone(), Some(20_250_000), t1).expect("a second");
        let rb = t
            .rate(b.clone(), Some(99_250_000), t1 + Duration::from_micros(50))
            .expect("b second");
        assert!((ra - 5.0).abs() < 0.1, "runner a cpu% = {ra}");
        assert!((rb - 5.0).abs() < 0.1, "runner b cpu% = {rb}");
    }
}
