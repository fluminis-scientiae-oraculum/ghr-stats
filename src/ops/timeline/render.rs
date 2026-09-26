//! A `Timeline` as plain text: chronological, one line per change, no ANSI.

use crate::shared::models::GhView;
use crate::shared::models::timeline::{
    Bounded, Edge, JobEdge, JobTransition, ReconcileEdge, Timeline,
};
use crate::shared::util::BUILD_VERSION;

pub(super) fn human(t: &Timeline, since: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "ghr-stats {BUILD_VERSION}  ·  timeline  ·  last {since}  ·  {} → {}",
        t.window.since, t.generated_at
    );
    if let Some(first) = t.window.truncated_at {
        // Where the data starts, not why: a pruned history and a young one look
        // identical from here.
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

    // Own section: job churn would bury the state changes that explain it.
    if !t.jobs.items.is_empty() || t.jobs.limited {
        let _ = writeln!(out, "{}", section("job event", &t.jobs));
        for j in &t.jobs.items {
            let _ = writeln!(
                out,
                "  {}  {}  {}  {}  {}",
                j.at,
                j.org,
                j.runner,
                job_word(&j.edge),
                job_name(j)
            );
        }
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

/// States whether the count is the whole count.
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

/// An unresolved conclusion prints `?`: only the reconcile learns whether a job passed.
fn job_word(edge: &JobEdge) -> String {
    match edge {
        JobEdge::Started => "job started".to_string(),
        JobEdge::Completed { conclusion: None } => "job completed (conclusion ?)".to_string(),
        JobEdge::Completed {
            conclusion: Some(c),
        } => format!("job completed ({c})"),
    }
}

fn job_name(j: &JobTransition) -> String {
    match (j.repo.as_str(), j.job.as_str()) {
        ("", "") => "(unnamed)".to_string(),
        ("", job) => job.to_string(),
        (repo, "") => repo.to_string(),
        (repo, job) => format!("{repo}/{job}"),
    }
}

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
    use crate::shared::models::timeline::{TimelinePoint, Transition, Window};
    use crate::shared::models::{ApiState, Liveness};

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
            jobs: Bounded {
                items: Vec::new(),
                limited: false,
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
        assert!(text.contains("1 transition\n"), "{text}");
    }

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

    #[test]
    fn an_empty_window_says_nothing_changed() {
        assert!(human(&timeline(Vec::new(), false), "6h").contains("(nothing changed"));
    }

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

    #[test]
    fn a_truncated_window_is_called_out() {
        let mut t = timeline(Vec::new(), false);
        t.window.truncated_at = Some(3_600);
        assert!(human(&t, "6h").contains("window truncated"));
    }

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
}
