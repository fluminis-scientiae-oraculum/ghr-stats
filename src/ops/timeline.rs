//! `ghr-stats timeline` — what changed over a window.
//!
//! `status` answers *is the fleet healthy now*; `explain` answers *why isn't
//! it*. Neither can answer *when did this start, and what moved first* — and
//! that ordering is what turns a set of symptoms into a cause. The 2026-07-25
//! investigation reached for it by hand: two SQL queries against a private
//! schema, because the ordering existed in the database and nowhere in the
//! interface.
//!
//! Two properties are load-bearing.
//!
//! **Edges, not samples.** A six-hour window on this fleet is tens of thousands
//! of rows that mostly say "still idle". The same window as transitions is a
//! handful of lines, and the three streams — local liveness, GitHub's view, the
//! per-org reconcile outcome — are kept separate precisely so their
//! disagreement stays visible.
//!
//! **Bounded by construction.** Every collection carries whether it was cut, the
//! window is capped before it leaves this process and clamped again by the
//! collector, and a window reaching past what `db prune` left says so. An agent
//! that cannot trust the bound has to fetch everything to find out, which is the
//! situation the verb exists to end.
//!
//! Unlike `status` and `explain` there is no local-scan fallback: history exists
//! only in the collector's database. A live scan can say what is true now; it
//! cannot say what was true an hour ago, and inventing a one-point "timeline"
//! from the present would answer a different question than the one asked.

use std::process::ExitCode;

use anyhow::Result;

use crate::cli::TimelineArgs;
use crate::shared::config::Config;
use crate::shared::ipc::client::Client;
use crate::shared::ipc::{Query, Request, Response};
use crate::shared::models::GhView;
use crate::shared::models::timeline::{Bounded, Edge, ReconcileEdge, Timeline, TimelineQuery};
use crate::shared::util::{BUILD_VERSION, now_epoch};

/// The furthest back a window may reach.
///
/// Not a storage limit — `db prune` keeps 14 days by default — but a bound on
/// what one call may be asked to produce. Seven days is past every "what
/// happened overnight / over the weekend" question while staying an order of
/// magnitude short of a window whose transitions would swamp any caller.
const MAX_WINDOW_SECS: u64 = 7 * 86_400;

/// Whether the question could be answered at all.
///
/// `timeline` makes no health call, so its exit code must not pretend to: a
/// window with no transitions in it is a perfectly healthy answer, and mapping
/// that to [`crate::shared::models::Verdict::Ok`] would let `timeline && echo
/// healthy` claim something this verb never assessed. What it *can* report is
/// availability — and the two codes it uses keep the meanings the `status` table
/// already gives them, so one vocabulary spans the verbs.
pub(crate) enum Availability {
    /// The window was answered.
    Answered,
    /// No collector, so there is no history to answer from — "cannot
    /// determine", the same 2 `status` returns when it cannot say anything.
    Unavailable,
}

impl From<Availability> for ExitCode {
    fn from(a: Availability) -> Self {
        ExitCode::from(match a {
            Availability::Answered => 0,
            Availability::Unavailable => 2,
        })
    }
}

/// Run the verb.
pub fn run(args: &TimelineArgs, _cfg: &Config) -> Result<Availability> {
    let window = parse_since(&args.since)?;
    if window.clamped {
        // The effective window is in the payload either way, but an operator who
        // asked for 30d should not have to read a field to discover they got 7.
        eprintln!(
            "note: --since {} exceeds the {}d maximum window; using 7d",
            args.since,
            MAX_WINDOW_SECS / 86_400
        );
    }

    let mut client = match Client::connect_any() {
        Ok(c) => c,
        Err(reason) => {
            eprintln!(
                "cannot read history: {} — timeline needs the collector, which is the only \
                 thing that keeps a record; a local scan can only see the present.",
                reason.word()
            );
            return Ok(Availability::Unavailable);
        }
    };

    let query = TimelineQuery {
        since_ts: now_epoch() - window.secs as i64,
        limit: args.limit,
        org: args.org.clone(),
        runner: args.runner.clone(),
        samples: args.samples,
    };
    let timeline = match client.request(&Request::Query(Query::Timeline(query)))? {
        Response::Timeline(t) => *t,
        // The collector is up and speaks our wire version, so this is its own
        // fault (a database error, almost always) — the same distinction
        // `explain` draws between "absent" and "broken".
        Response::Error(e) => {
            eprintln!("cannot read history: the collector answered with an error: {e}");
            return Ok(Availability::Unavailable);
        }
        other => {
            eprintln!("cannot read history: unexpected reply from the collector: {other:?}");
            return Ok(Availability::Unavailable);
        }
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&timeline)?);
    } else {
        print!("{}", human(&timeline, &args.since));
    }
    Ok(Availability::Answered)
}

/// A parsed `--since`, and whether the cap moved it.
struct SinceWindow {
    secs: u64,
    clamped: bool,
}

/// Parse `90s` / `30m` / `6h` / `2d` into seconds, capped at [`MAX_WINDOW_SECS`].
///
/// A unit is required. A bare `6` could mean seconds or hours depending on who
/// wrote the caller, and a window silently a hundred times wider than intended
/// is worse than a rejected flag.
fn parse_since(s: &str) -> Result<SinceWindow> {
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

/// The human rendering. Plain text, chronological, one line per change — so a
/// terminal reader and `grep` see the same thing.
fn human(t: &Timeline, since: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "ghr-stats {BUILD_VERSION}  ·  timeline  ·  last {since}  ·  {} → {}",
        t.window.since, t.generated_at
    );
    if let Some(first) = t.window.truncated_at {
        // States what is known — where the data starts — and NOT why, which the
        // collector cannot tell: a pruned history and a young one look identical
        // from here, and naming a cause we did not observe is the habit this
        // whole verb exists to break.
        let _ = writeln!(
            out,
            "! window truncated: no data before {} — the window reaches past what is held, \
             so an empty stretch before that is absence of record, not absence of change",
            crate::shared::util::to_rfc3339_utc(first)
        );
    }

    let _ = writeln!(out, "{}", section("transition", &t.transitions));
    for tr in &t.transitions.items {
        let _ = writeln!(out, "  {}  {}  {}", tr.at, tr.org, edge_line(&tr.edge));
    }
    if t.transitions.items.is_empty() {
        let _ = writeln!(out, "  (nothing changed in this window)");
    }

    if let Some(samples) = &t.samples {
        let _ = writeln!(out, "{}", section("sample", samples));
        for p in &samples.items {
            let _ = writeln!(
                out,
                "  {}  {}  {}  {}  github={}",
                crate::shared::util::to_rfc3339_utc(p.ts),
                p.org,
                p.runner,
                p.liveness.as_str(),
                gh_word(p.github)
            );
        }
    }
    out
}

/// A section header that states the count AND whether it is the whole count —
/// "500" and "500, and there are more" are different answers and must not print
/// the same. Takes the singular noun and pluralises it (both nouns here are
/// regular), so a one-row window does not read as machine output.
fn section<T>(singular: &str, b: &Bounded<T>) -> String {
    let n = b.items.len();
    let noun = match n {
        1 => singular.to_string(),
        _ => format!("{singular}s"),
    };
    if b.limited {
        format!("{n} {noun} (limited — older rows exist; narrow --since or raise --limit)")
    } else {
        format!("{n} {noun}")
    }
}

/// One transition, rendered. Both ends are shown even for the boolean edges,
/// where the previous value is derived rather than stored — a reader should not
/// have to know which fields the wire chose to carry.
fn edge_line(edge: &Edge) -> String {
    match edge {
        Edge::Liveness { runner, from, to } => {
            format!("{runner}  liveness: {} → {}", from.as_str(), to.as_str())
        }
        Edge::GithubOnline { runner, online } => format!(
            "{runner}  github: {} → {}",
            online_word(!online),
            online_word(*online)
        ),
        Edge::Reconcile(ReconcileEdge::Recovered) => "reconcile: failing → ok".to_string(),
        Edge::Reconcile(ReconcileEdge::Failed {
            error_kind,
            http_status,
        }) => {
            let mut why: Vec<String> = Vec::new();
            if let Some(kind) = error_kind {
                why.push(kind.clone());
            }
            if let Some(code) = http_status {
                why.push(format!("http {code}"));
            }
            match why.is_empty() {
                true => "reconcile: ok → failing".to_string(),
                false => format!("reconcile: ok → failing ({})", why.join(", ")),
            }
        }
    }
}

fn online_word(online: bool) -> &'static str {
    if online { "online" } else { "offline" }
}

/// GitHub's view at a sample, with its freshness attached. `stale` and `unknown`
/// are rendered distinctly for the same reason the type keeps them apart: one
/// means the answer aged out, the other that there was never an answer.
fn gh_word(view: GhView) -> String {
    match view {
        GhView::Fresh { state, age_s } => format!("{} ({age_s}s)", online_word(state.online)),
        GhView::Stale { age_s } => format!("stale ({age_s}s)"),
        GhView::Unknown => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::models::timeline::{Timeline, TimelinePoint, Transition, Window};
    use crate::shared::models::{ApiState, Liveness};

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

    fn timeline(transitions: Vec<Transition>, limited: bool) -> Timeline {
        Timeline {
            schema_version: 1,
            generated_at: "1970-01-01T02:00:00Z".to_string(),
            generated_at_epoch: 7_200,
            window: Window {
                since: "1970-01-01T00:00:00Z".to_string(),
                since_epoch: 0,
                until_epoch: 7_200,
                truncated_at: None,
            },
            transitions: Bounded {
                items: transitions,
                limited,
            },
            samples: None,
        }
    }

    fn at(ts: i64, org: &str, edge: Edge) -> Transition {
        Transition {
            ts,
            at: crate::shared::util::to_rfc3339_utc(ts),
            org: org.to_string(),
            edge,
        }
    }

    /// The rendering must show both ends of a GitHub edge even though the wire
    /// carries only one — otherwise the reader has to know the encoding.
    #[test]
    fn a_github_edge_renders_both_ends() {
        let t = timeline(
            vec![at(
                60,
                "org-b",
                Edge::GithubOnline {
                    runner: "runner-07".to_string(),
                    online: false,
                },
            )],
            false,
        );
        let text = human(&t, "6h");
        assert!(
            text.contains("runner-07  github: online → offline"),
            "{text}"
        );
        // Singular: a one-row window must not read as machine output.
        assert!(text.contains("1 transition\n"), "{text}");
    }

    /// A reconcile failure must name the cause: "the org stopped answering" and
    /// "the org stopped answering because the PAT is dead" lead to different
    /// next actions.
    #[test]
    fn a_reconcile_failure_renders_its_cause() {
        let t = timeline(
            vec![at(
                60,
                "org-b",
                Edge::Reconcile(ReconcileEdge::Failed {
                    error_kind: Some("unauthorized".to_string()),
                    http_status: Some(401),
                }),
            )],
            false,
        );
        assert!(
            human(&t, "6h").contains("reconcile: ok → failing (unauthorized, http 401)"),
            "{}",
            human(&t, "6h")
        );
    }

    /// An empty window must say it is empty. A bare header over no rows reads as
    /// a rendering bug, and "nothing changed" is a real and useful answer.
    #[test]
    fn an_empty_window_says_nothing_changed() {
        assert!(human(&timeline(Vec::new(), false), "6h").contains("(nothing changed"));
    }

    /// A truncated section must be visibly truncated. This is the whole reason
    /// `Bounded` carries the flag: 500 rows and "500 rows, and there are more"
    /// are different answers.
    #[test]
    fn a_limited_section_is_marked() {
        let t = timeline(
            vec![at(
                60,
                "o",
                Edge::Liveness {
                    runner: "r".to_string(),
                    from: Liveness::Idle,
                    to: Liveness::Busy,
                },
            )],
            true,
        );
        let text = human(&t, "6h");
        assert!(text.contains("limited"), "{text}");
        assert!(text.contains("r  liveness: idle → busy"), "{text}");
    }

    /// A pruned window must not read as a quiet one.
    #[test]
    fn a_truncated_window_is_called_out() {
        let mut t = timeline(Vec::new(), false);
        t.window.truncated_at = Some(3_600);
        assert!(human(&t, "6h").contains("window truncated"));
    }

    /// Stale and unknown must render differently — the type keeps them apart
    /// precisely because the operator response differs.
    #[test]
    fn samples_distinguish_stale_from_unknown() {
        let mut t = timeline(Vec::new(), false);
        t.samples = Some(Bounded {
            items: vec![
                TimelinePoint {
                    ts: 10,
                    org: "o".to_string(),
                    runner: "r1".to_string(),
                    liveness: Liveness::Idle,
                    github: GhView::Stale { age_s: 900 },
                },
                TimelinePoint {
                    ts: 20,
                    org: "o".to_string(),
                    runner: "r2".to_string(),
                    liveness: Liveness::Busy,
                    github: GhView::Unknown,
                },
                TimelinePoint {
                    ts: 30,
                    org: "o".to_string(),
                    runner: "r3".to_string(),
                    liveness: Liveness::Idle,
                    github: GhView::Fresh {
                        state: ApiState {
                            online: true,
                            busy: false,
                        },
                        age_s: 12,
                    },
                },
            ],
            limited: false,
        });
        let text = human(&t, "6h");
        assert!(text.contains("github=stale (900s)"), "{text}");
        assert!(text.contains("github=unknown"), "{text}");
        assert!(text.contains("github=online (12s)"), "{text}");
    }

    /// The exit code is availability, never health: an answered window exits 0
    /// even when it is full of failures, and a missing collector exits 2 — the
    /// same "cannot determine" `status` uses.
    #[test]
    fn exit_codes_carry_availability_not_health() {
        assert_eq!(
            format!("{:?}", ExitCode::from(Availability::Answered)),
            format!("{:?}", ExitCode::from(0))
        );
        assert_eq!(
            format!("{:?}", ExitCode::from(Availability::Unavailable)),
            format!("{:?}", ExitCode::from(2))
        );
    }
}
