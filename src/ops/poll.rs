//! Shared cadence for the verbs that watch rather than ask once.
//!
//! Both watching verbs poll — `wait` until a predicate holds, `tail` forever —
//! and both must poll at the rate the data actually changes. That rate is
//! `intervals.local_secs`: a transition does not exist until a sampler observes
//! it, so asking faster returns the same answer again and asking slower loses
//! resolution. This is the same fact that decided against a subscribe path on
//! the wire (see `shared::ipc`), applied to the client side.

use std::time::Duration;

/// How long to sleep so the NEXT poll starts one `interval` after the last one
/// did — not one interval after the last one *finished*.
///
/// Pure, so the cadence is testable without sleeping. Sleeping a flat interval
/// after each query makes the real period `interval + query_time`, which drifts
/// against the sampler it is trying to track: a 200 ms query against a 5 s
/// sampler yields a 5.2 s cadence, and the two grids walk out of phase until the
/// poller is reliably observing each tick late. Saturating, so a poll that
/// overran its own interval sleeps not at all rather than panicking.
pub(crate) fn remaining(elapsed: Duration, interval: Duration) -> Duration {
    interval.saturating_sub(elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sleep_is_the_interval_minus_what_the_poll_already_spent() {
        assert_eq!(
            remaining(Duration::from_millis(200), Duration::from_secs(5)),
            Duration::from_millis(4_800)
        );
    }

    /// A poll slower than its own interval must not panic and must not sleep —
    /// it is already late, and the next query is the only way to catch up.
    #[test]
    fn a_poll_that_overran_its_interval_sleeps_not_at_all() {
        assert_eq!(
            remaining(Duration::from_secs(9), Duration::from_secs(5)),
            Duration::ZERO
        );
    }
}
