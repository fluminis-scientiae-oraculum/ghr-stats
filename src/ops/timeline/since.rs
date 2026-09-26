//! `--since` to a bounded window. Deriving edges costs work proportional to the
//! span, so an uncapped window could stall the collector.

use anyhow::Result;

use super::MAX_WINDOW_SECS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SinceWindow {
    pub(crate) secs: u64,
    /// The request asked for more than [`MAX_WINDOW_SECS`].
    pub(crate) clamped: bool,
}

/// The window in its largest whole unit, e.g. `7d` or `90m`.
impl std::fmt::Display for SinceWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.secs;
        let (n, unit) = [(86_400, "d"), (3_600, "h"), (60, "m")]
            .into_iter()
            .find(|(u, _)| s.is_multiple_of(*u))
            .map_or((s, "s"), |(u, name)| (s / u, name));
        write!(f, "{n}{unit}")
    }
}

/// A unit is required: a bare `6` could mean seconds or hours.
pub(crate) fn parse_since(s: &str) -> Result<SinceWindow> {
    let s = s.trim();
    let unit_at = s.char_indices().last().map_or(0, |(i, _)| i);
    let (digits, unit) = s.split_at(unit_at);
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => anyhow::bail!("--since needs a unit: 90s, 30m, 6h or 2d (got {s:?})"),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        anyhow::bail!("--since needs a whole number before the unit (got {s:?})");
    }
    let n: u64 = digits
        .parse()
        .map_err(|_| anyhow::anyhow!("--since needs a whole number before the unit (got {s:?})"))?;
    if n == 0 {
        anyhow::bail!("--since must be greater than zero (got {s:?})");
    }
    // Saturating: an overflow clamps to the cap instead of wrapping to a narrow window.
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
    fn since_accepts_every_unit_and_prints_its_window() {
        for (text, secs, shown) in [
            ("90s", 90, "90s"),
            ("30m", 1_800, "30m"),
            ("120m", 7_200, "2h"),
            ("6h", 21_600, "6h"),
            ("2d", 172_800, "2d"),
        ] {
            let w = parse_since(text).unwrap();
            assert_eq!(w.secs, secs, "{text}");
            assert!(!w.clamped);
            assert_eq!(w.to_string(), shown);
        }
    }

    #[test]
    fn since_requires_a_unit() {
        assert!(parse_since("6").is_err());
        assert!(parse_since("").is_err());
        assert!(parse_since("6y").is_err());
        assert!(parse_since("hh").is_err());
        assert!(parse_since("0h").is_err());
        assert!(parse_since("+5h").is_err());
        assert!(parse_since("5é").is_err());
    }

    #[test]
    fn since_is_capped_and_says_so() {
        let w = parse_since("30d").unwrap();
        assert_eq!(w.secs, MAX_WINDOW_SECS);
        assert!(w.clamped);
    }

    #[test]
    fn an_overflowing_window_clamps_to_the_cap() {
        let w = parse_since("999999999999999999999d").is_err();
        assert!(w, "a value past u64 is a parse error, not a wrap");
        let w = parse_since("500000000000000d").unwrap();
        assert_eq!(w.secs, MAX_WINDOW_SECS);
        assert!(w.clamped);
    }
}
