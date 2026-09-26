//! CPU% from cgroup `usage_usec`, a cumulative counter: a percentage only exists
//! against a previous observation of the same runner, keyed by its install dir.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Default)]
pub struct CpuRateTracker {
    prev: HashMap<PathBuf, (u64, Instant)>,
}

impl CpuRateTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// CPU percent for the runner at `dir` since its previous observation. `None`
    /// on the first sample, when usage is unavailable, or when the counter
    /// appears to reset. May exceed 100% across multiple cores.
    pub fn rate(&mut self, dir: &Path, usage_usec: Option<u64>, at: Instant) -> Option<f32> {
        let Some(cur) = usage_usec else {
            self.prev.remove(dir);
            return None;
        };
        let pct = self.prev.get(dir).and_then(|(prev_usec, prev_at)| {
            let dt = at.duration_since(*prev_at).as_secs_f64();
            (dt > 0.0 && cur >= *prev_usec)
                .then(|| ((cur - prev_usec) as f64 / 1_000_000.0 / dt * 100.0) as f32)
        });
        self.prev.insert(dir.to_path_buf(), (cur, at));
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
        let r = Path::new("/srv/runners/r0");
        let t0 = Instant::now();
        assert_eq!(t.rate(r, Some(0), t0), None);
        let pct = t
            .rate(r, Some(500_000), t0 + Duration::from_secs(1))
            .unwrap();
        assert!((pct - 50.0).abs() < 0.01, "got {pct}");
    }

    #[test]
    fn counter_reset_and_missing_usage_yield_none() {
        let mut t = CpuRateTracker::new();
        let r = Path::new("/srv/runners/r0");
        let t0 = Instant::now();
        t.rate(r, Some(1_000_000), t0);
        assert_eq!(t.rate(r, Some(10), t0 + Duration::from_secs(1)), None);
        assert_eq!(t.rate(r, None, t0 + Duration::from_secs(2)), None);
    }
}
