//! The question in: `--since 6h` to a bounded window.
//!
//! The cap is the point. A window is billed in work, not in output — deriving
//! edges costs roughly a second per day of span on this fleet's database — so an
//! unbounded `--since` is a way to hang the collector from the CLI. The parse
//! therefore CLAMPS rather than rejecting, and records that it did, because a
//! silently shortened window would make the answer a lie about its own range.
//!
//! A bare number is refused. `--since 6` is ambiguous between seconds and hours,
//! and guessing would make every such invocation wrong half the time.

use anyhow::Result;

use super::MAX_WINDOW_SECS;

/// A parsed `--since`, and whether the cap moved it.
pub(super) struct SinceWindow {
    pub(super) secs: u64,
    pub(super) clamped: bool,
}

/// Parse `90s` / `30m` / `6h` / `2d` into seconds, capped at [`MAX_WINDOW_SECS`].
///
/// A unit is required. A bare `6` could mean seconds or hours depending on who
/// wrote the caller, and a window silently a hundred times wider than intended
/// is worse than a rejected flag.
pub(super) fn parse_since(s: &str) -> Result<SinceWindow> {
    let s = s.trim();
    let (digits, unit) = s.split_at(s.len().saturating_sub(1));
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => anyhow::bail!("--since needs a unit: 90s, 30m, 6h or 2d (got {s:?})"),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| anyhow::anyhow!("--since needs a whole number before the unit (got {s:?})"))?;
    if n == 0 {
        anyhow::bail!("--since must be greater than zero (got {s:?})");
    }
    // Saturating, so `999999999d` clamps to the cap rather than wrapping into a
    // tiny window — an overflow that silently NARROWS the window would be the
    // one failure mode a caller could not see in the output.
    let secs = n.saturating_mul(multiplier);
    Ok(SinceWindow {
        secs: secs.min(MAX_WINDOW_SECS),
        clamped: secs > MAX_WINDOW_SECS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_accepts_every_unit() {
        for (text, secs) in [("90s", 90), ("30m", 1_800), ("6h", 21_600), ("2d", 172_800)] {
            let w = parse_since(text).unwrap();
            assert_eq!(w.secs, secs, "{text}");
            assert!(!w.clamped);
        }
    }

    /// A bare number is rejected rather than guessed at: `--since 6` meaning six
    /// seconds when the caller meant six hours is a wrong answer that looks
    /// right.
    #[test]
    fn since_requires_a_unit() {
        assert!(parse_since("6").is_err());
        assert!(parse_since("").is_err());
        assert!(parse_since("6y").is_err());
        assert!(parse_since("hh").is_err());
        assert!(parse_since("0h").is_err());
    }

    #[test]
    fn since_is_capped_and_says_so() {
        let w = parse_since("30d").unwrap();
        assert_eq!(w.secs, MAX_WINDOW_SECS);
        assert!(w.clamped);
    }

    /// A window so large it overflows `u64` seconds must clamp UP to the cap,
    /// never wrap down to a few seconds — a silently narrowed window is the one
    /// error a caller could not detect from the output.
    #[test]
    fn an_overflowing_window_clamps_to_the_cap() {
        let w = parse_since("999999999999999999999d").is_err();
        assert!(w, "a value past u64 is a parse error, not a wrap");
        let w = parse_since("500000000000000d").unwrap();
        assert_eq!(w.secs, MAX_WINDOW_SECS);
        assert!(w.clamped);
    }
}
