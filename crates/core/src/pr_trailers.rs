//! The git-trailer contract lazybox writes onto a merged PR's commit (#1559).
//!
//! lazybox performs the merge itself, so it — and nothing else in the loop —
//! owns the moment the squash/merge commit body is written. That makes a git
//! trailer the durable place to record what a PR *took*: it travels with the
//! code (`git log --grep`, no API, no rate budget), it is machine-readable for
//! free (`git interpret-trailers --parse`), and it is written exactly when the
//! total is final.
//!
//! Four keys, each answering one question and nothing decorative:
//!
//! ```text
//! Lazybox-Cost: $0.42 · 1.24M in · 84.1k out
//! Lazybox-Agents: claude-opus-5 ×3, codex ×1
//! Lazybox-Effort: 14 turns · 2 human handoffs · 3 CI repairs
//! Lazybox-Time: issue→merge 3d4h · work→merge 52m
//! ```
//!
//! Because these land in permanent, public history and become the query
//! surface (`git log`, a store index, an MCP tool), the format is a **stable
//! interface**: fixed key names, one value shape per key, never reformatted. A
//! cosmetic change to the wording silently breaks every historical query, so
//! the exact rendering is pinned by the `format_lock` test the same way a wire
//! contract is fingerprinted.
//!
//! Two rules the shape enforces:
//!
//! 1. **Omit rather than zero.** A field that could not be measured is `None`
//!    and renders nothing — never `0`. `Lazybox-Cost: $0.00` reads as "this was
//!    free" instead of "this wasn't metered". A trailer line with no measurable
//!    field is dropped entirely, and [`PrTrailers::render`] of an all-empty set
//!    is the empty string, so the caller writes no block at all. A genuinely
//!    *measured* zero (say, zero human handoffs — the good outcome worth
//!    showing) is `Some(0)` and does render.
//! 2. **One value shape per key**, so a reader can decode it years later.

use crate::pricing::TokenCounts;

/// Trailer key: what the work spent.
pub const KEY_COST: &str = "Lazybox-Cost";
/// Trailer key: which agents did it, and at which tier.
pub const KEY_AGENTS: &str = "Lazybox-Agents";
/// Trailer key: how hard it was, and how autonomous.
pub const KEY_EFFORT: &str = "Lazybox-Effort";
/// Trailer key: the two clocks — lead time and cycle time.
pub const KEY_TIME: &str = "Lazybox-Time";

/// The full set of lazybox trailers for one merged PR. Every field is
/// optional; an absent field is one that could not be measured and is omitted
/// from the rendered block (never rendered as zero).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrTrailers {
    pub cost: Option<CostTrailer>,
    /// Ordered so the busiest / primary agent can lead. Empty omits the line.
    pub agents: Vec<AgentCount>,
    pub effort: EffortTrailer,
    pub time: TimeTrailer,
}

/// `$0.42 · 1.24M in · 84.1k out`. Dollars and tokens are independently
/// optional so a public-repo form can carry the shape of the work (tokens,
/// models) while withholding the commercial figure (dollars).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CostTrailer {
    /// Metered cost in micro-USD. `None` when metering never ran — the common
    /// case with metering off by default, and precisely when a `$0.00` would
    /// mislead.
    pub micros: Option<u64>,
    /// Token totals across the PR's agent runs. `None` when unmeasured.
    pub tokens: Option<TokenCounts>,
}

/// One entry of the agents line: an identifier (model id at its tier, or an
/// agent id) and how many runs of it the PR involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCount {
    pub label: String,
    pub count: u32,
}

/// `14 turns · 2 human handoffs · 3 CI repairs`. Each metric is independently
/// optional.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffortTrailer {
    /// Agent conversation turns.
    pub turns: Option<u64>,
    /// Times a human had to answer an `InputNeeded` — the delegability signal.
    pub human_handoffs: Option<u64>,
    /// Auto-fix attempts that repaired failing CI.
    pub ci_repairs: Option<u64>,
}

/// `issue→merge 3d4h · work→merge 52m`. Lead time (any GitHub tool can compute
/// it) and cycle time (only lazybox knows when work actually started).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimeTrailer {
    /// Seconds from issue creation to merge (lead time).
    pub issue_to_merge_secs: Option<u64>,
    /// Seconds from the first agent spawn to merge (cycle time).
    pub work_to_merge_secs: Option<u64>,
}

impl PrTrailers {
    /// The trailer lines, in fixed key order, each `Key: value`. Lines whose
    /// every field is unmeasured are omitted.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut push = |key: &str, value: Option<String>| {
            if let Some(value) = value {
                out.push(format!("{key}: {value}"));
            }
        };
        push(KEY_COST, self.cost.as_ref().and_then(CostTrailer::value));
        push(KEY_AGENTS, agents_value(&self.agents));
        push(KEY_EFFORT, self.effort.value());
        push(KEY_TIME, self.time.value());
        out
    }

    /// The trailer block, lines joined by `\n`. Empty when nothing was
    /// measurable — the caller then writes no block at all.
    pub fn render(&self) -> String {
        self.lines().join("\n")
    }

    /// True when no field was measurable, so there is nothing to write.
    pub fn is_empty(&self) -> bool {
        self.lines().is_empty()
    }
}

impl CostTrailer {
    fn value(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(micros) = self.micros {
            parts.push(format_dollars(micros));
        }
        if let Some(t) = &self.tokens {
            parts.push(format!("{} in", fmt_count(t.input)));
            parts.push(format!("{} out", fmt_count(t.output)));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

impl EffortTrailer {
    fn value(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(n) = self.turns {
            parts.push(format!("{n} {}", plural(n, "turn")));
        }
        if let Some(n) = self.human_handoffs {
            parts.push(format!("{n} human {}", plural(n, "handoff")));
        }
        if let Some(n) = self.ci_repairs {
            parts.push(format!("{n} CI {}", plural(n, "repair")));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

impl TimeTrailer {
    fn value(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(s) = self.issue_to_merge_secs {
            parts.push(format!("issue→merge {}", fmt_duration(s)));
        }
        if let Some(s) = self.work_to_merge_secs {
            parts.push(format!("work→merge {}", fmt_duration(s)));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

fn agents_value(agents: &[AgentCount]) -> Option<String> {
    (!agents.is_empty()).then(|| {
        agents
            .iter()
            .map(|a| format!("{} ×{}", sanitize_label(&a.label), a.count))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

/// A trailer value is single-line by definition. Agent / model labels are the
/// one caller-supplied free-text field (they originate in user YAML), so a
/// stray newline would split the trailer paragraph — truncating the value when
/// `extract` reads it back, or letting a crafted label inject a forged
/// `Lazybox-*` line into permanent history. Collapse any control character to a
/// space so a rendered value can never break the line it lives on.
fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Append `trailers` to an existing commit `body`, forming the trailing
/// trailer paragraph git and GitHub expect.
///
/// This is the fix for the `mergePullRequest` gotcha: passing `commitBody`
/// *replaces* the default squash body (the concatenated commit messages), so a
/// naive "set body to the trailers" wipes the commit log. Callers fetch the
/// default body and pass it here to append instead of substitute.
///
/// If the body already ends in a trailer paragraph (e.g. `Co-authored-by:`),
/// our lines join it directly so the whole trailing block stays one paragraph —
/// which is what `git interpret-trailers --parse` reads. Otherwise a blank line
/// opens a fresh trailer paragraph. An empty trailer set returns the body
/// unchanged.
pub fn append_to_body(body: &str, trailers: &PrTrailers) -> String {
    let block = trailers.render();
    if block.is_empty() {
        return body.to_string();
    }
    let body = body.trim_end_matches('\n');
    if body.is_empty() {
        return block;
    }
    let separator = if ends_with_trailer(body) {
        "\n"
    } else {
        "\n\n"
    };
    format!("{body}{separator}{block}")
}

/// Extract the `Lazybox-*` trailers from a commit message, in file order, as
/// `(key, value)` pairs — the reader half of the contract, matching what
/// `git interpret-trailers --parse` surfaces. The store index and any query
/// tool reconstruct history from this, so the record lives in the repo, not
/// only in the daemon.
pub fn extract(message: &str) -> Vec<(String, String)> {
    message
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(": ")?;
            let suffix = key.strip_prefix("Lazybox-")?;
            let is_key = !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_alphabetic());
            is_key.then(|| (key.to_string(), value.to_string()))
        })
        .collect()
}

/// Does `text`'s final line read as a git trailer (`Token: value`)? Used to
/// decide whether appended trailers join the body's last paragraph or open a
/// new one.
fn ends_with_trailer(text: &str) -> bool {
    text.lines().next_back().is_some_and(is_trailer_line)
}

fn is_trailer_line(line: &str) -> bool {
    let Some((key, _)) = line.split_once(": ") else {
        return false;
    };
    key.starts_with(|c: char| c.is_ascii_alphabetic())
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Micro-USD as `$0.42`, dropping to finer precision below a cent (`$0.0042`)
/// so a small metered PR shows a real figure rather than a misleading `$0.00`.
fn format_dollars(micros: u64) -> String {
    let dollars = micros as f64 / 1_000_000.0;
    if micros == 0 || dollars >= 0.01 {
        format!("${dollars:.2}")
    } else {
        format!("${dollars:.4}")
    }
}

/// A token count to three significant figures with a `k` / `M` / `B` suffix:
/// `1.24M`, `84.1k`, `320k`, `999`.
fn fmt_count(n: u64) -> String {
    let scaled = |div: f64, suffix: &str| {
        let v = n as f64 / div;
        let decimals = if v >= 100.0 {
            0
        } else if v >= 10.0 {
            1
        } else {
            2
        };
        format!("{v:.decimals$}{suffix}")
    };
    // Tier is chosen on the *rounded* value, not raw magnitude: a mantissa that
    // rounds up to 1000 must promote to the next suffix, so 999_999 reads
    // "1.00M" (not "1000k"). The k/M mantissa is printed at 0 decimals once
    // >= 100, so it renders as 1000 the moment n / div >= 999.5.
    if n < 1_000 {
        n.to_string()
    } else if n < 999_500 {
        scaled(1_000.0, "k")
    } else if n < 999_500_000 {
        scaled(1_000_000.0, "M")
    } else {
        scaled(1_000_000_000.0, "B")
    }
}

/// A span as its two most-significant non-zero units: `3d4h`, `2h5m`, `52m`.
fn fmt_duration(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        if hours > 0 {
            format!("{days}d{hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        if mins > 0 {
            format!("{hours}h{mins}m")
        } else {
            format!("{hours}h")
        }
    } else {
        format!("{mins}m")
    }
}

fn plural(n: u64, singular: &str) -> String {
    if n == 1 {
        singular.to_string()
    } else {
        format!("{singular}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> PrTrailers {
        PrTrailers {
            cost: Some(CostTrailer {
                micros: Some(420_000),
                tokens: Some(TokenCounts {
                    input: 1_240_000,
                    output: 84_100,
                    ..Default::default()
                }),
            }),
            agents: vec![
                AgentCount {
                    label: "claude-opus-5".into(),
                    count: 3,
                },
                AgentCount {
                    label: "codex".into(),
                    count: 1,
                },
            ],
            effort: EffortTrailer {
                turns: Some(14),
                human_handoffs: Some(2),
                ci_repairs: Some(3),
            },
            time: TimeTrailer {
                issue_to_merge_secs: Some(3 * 86_400 + 4 * 3_600),
                work_to_merge_secs: Some(52 * 60),
            },
        }
    }

    /// The exact bytes of the contract. This test IS the stable interface: a
    /// change here changes every historical `git log` query, so it must be a
    /// deliberate, reviewed contract revision — never an incidental reword.
    #[test]
    fn format_lock() {
        assert_eq!(
            full().render(),
            "Lazybox-Cost: $0.42 · 1.24M in · 84.1k out\n\
             Lazybox-Agents: claude-opus-5 ×3, codex ×1\n\
             Lazybox-Effort: 14 turns · 2 human handoffs · 3 CI repairs\n\
             Lazybox-Time: issue→merge 3d4h · work→merge 52m",
        );
    }

    #[test]
    fn omits_unmeasured_fields_and_lines() {
        // Only cost dollars measured: the tokens sub-fields, and the three
        // other lines entirely, are absent — not zeroed.
        let t = PrTrailers {
            cost: Some(CostTrailer {
                micros: Some(1_500_000),
                tokens: None,
            }),
            ..Default::default()
        };
        assert_eq!(t.render(), "Lazybox-Cost: $1.50");
    }

    #[test]
    fn empty_set_renders_nothing() {
        assert!(PrTrailers::default().is_empty());
        assert_eq!(PrTrailers::default().render(), "");
    }

    #[test]
    fn measured_zero_renders_but_none_does_not() {
        // A measured zero (fully autonomous — the outcome worth showing) is
        // Some(0) and renders; an unmeasured field is None and vanishes.
        let t = PrTrailers {
            effort: EffortTrailer {
                turns: None,
                human_handoffs: Some(0),
                ci_repairs: None,
            },
            ..Default::default()
        };
        assert_eq!(t.render(), "Lazybox-Effort: 0 human handoffs");
    }

    #[test]
    fn public_form_carries_tokens_without_dollars() {
        let t = PrTrailers {
            cost: Some(CostTrailer {
                micros: None,
                tokens: Some(TokenCounts {
                    input: 2_000_000,
                    output: 500_000,
                    ..Default::default()
                }),
            }),
            ..Default::default()
        };
        assert_eq!(t.render(), "Lazybox-Cost: 2.00M in · 500k out");
    }

    #[test]
    fn effort_and_time_pluralize_and_singularize() {
        let t = PrTrailers {
            effort: EffortTrailer {
                turns: Some(1),
                human_handoffs: Some(1),
                ci_repairs: Some(1),
            },
            time: TimeTrailer {
                issue_to_merge_secs: Some(90_000),
                work_to_merge_secs: Some(45),
            },
            ..Default::default()
        };
        assert_eq!(
            t.render(),
            "Lazybox-Effort: 1 turn · 1 human handoff · 1 CI repair\n\
             Lazybox-Time: issue→merge 1d1h · work→merge 0m",
        );
    }

    #[test]
    fn dollars_below_a_cent_keep_precision() {
        assert_eq!(format_dollars(420_000), "$0.42");
        assert_eq!(format_dollars(4_200), "$0.0042");
        assert_eq!(format_dollars(12_500_000), "$12.50");
    }

    #[test]
    fn counts_use_three_significant_figures() {
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_000), "1.00k");
        assert_eq!(fmt_count(84_100), "84.1k");
        assert_eq!(fmt_count(320_000), "320k");
        assert_eq!(fmt_count(1_240_000), "1.24M");
        assert_eq!(fmt_count(2_500_000_000), "2.50B");
    }

    #[test]
    fn counts_promote_at_a_rounding_boundary() {
        // Regression: picking the tier by raw magnitude then rounding produced
        // "1000k" (and "1000M") when the mantissa rounded up to 1000. The last
        // value of each tier must stay under 1000, and the first that would
        // round up must promote to the next suffix.
        assert_eq!(fmt_count(999_499), "999k");
        assert_eq!(fmt_count(999_500), "1.00M");
        assert_eq!(fmt_count(999_999), "1.00M");
        assert_eq!(fmt_count(999_499_000), "999M");
        assert_eq!(fmt_count(999_500_000), "1.00B");
    }

    #[test]
    fn durations_show_two_units() {
        assert_eq!(fmt_duration(3 * 86_400 + 4 * 3_600), "3d4h");
        assert_eq!(fmt_duration(2 * 3_600 + 5 * 60), "2h5m");
        assert_eq!(fmt_duration(52 * 60), "52m");
        assert_eq!(fmt_duration(3_600), "1h");
        assert_eq!(fmt_duration(86_400), "1d");
        assert_eq!(fmt_duration(30), "0m");
    }

    #[test]
    fn appends_after_prose_with_a_blank_line() {
        let body = "Squash of three commits.\n\nDetail line.";
        let appended = append_to_body(body, &full());
        assert_eq!(
            appended,
            "Squash of three commits.\n\nDetail line.\n\n\
             Lazybox-Cost: $0.42 · 1.24M in · 84.1k out\n\
             Lazybox-Agents: claude-opus-5 ×3, codex ×1\n\
             Lazybox-Effort: 14 turns · 2 human handoffs · 3 CI repairs\n\
             Lazybox-Time: issue→merge 3d4h · work→merge 52m",
        );
    }

    #[test]
    fn joins_an_existing_trailer_paragraph() {
        // A body already ending in trailers keeps them in ONE trailing
        // paragraph, so `git interpret-trailers --parse` sees co-authors and
        // our keys together.
        let body = "Fix the thing.\n\nCo-authored-by: Someone <s@example.com>";
        let t = PrTrailers {
            cost: Some(CostTrailer {
                micros: Some(500_000),
                tokens: None,
            }),
            ..Default::default()
        };
        assert_eq!(
            append_to_body(body, &t),
            "Fix the thing.\n\n\
             Co-authored-by: Someone <s@example.com>\n\
             Lazybox-Cost: $0.50",
        );
    }

    #[test]
    fn append_of_empty_trailers_leaves_body_untouched() {
        let body = "Just a commit.";
        assert_eq!(append_to_body(body, &PrTrailers::default()), body);
    }

    #[test]
    fn append_to_empty_body_is_the_block_alone() {
        assert_eq!(append_to_body("", &full()), full().render());
        assert_eq!(append_to_body("\n\n", &full()), full().render());
    }

    /// Render → write into a realistic commit → extract recovers exactly our
    /// keys and values, and nothing else. This is the round-trip the queryable
    /// history rests on.
    #[test]
    fn extract_round_trips_from_a_full_commit_message() {
        let commit = format!(
            "feat: do the thing (#1559)\n\n\
             A paragraph of body text describing the change.\n\n\
             Co-authored-by: Someone <s@example.com>\n{}",
            full().render(),
        );
        let extracted = extract(&commit);
        assert_eq!(
            extracted,
            vec![
                (
                    "Lazybox-Cost".to_string(),
                    "$0.42 · 1.24M in · 84.1k out".to_string()
                ),
                (
                    "Lazybox-Agents".to_string(),
                    "claude-opus-5 ×3, codex ×1".to_string()
                ),
                (
                    "Lazybox-Effort".to_string(),
                    "14 turns · 2 human handoffs · 3 CI repairs".to_string()
                ),
                (
                    "Lazybox-Time".to_string(),
                    "issue→merge 3d4h · work→merge 52m".to_string()
                ),
            ],
        );
    }

    #[test]
    fn extract_ignores_non_lazybox_and_malformed_lines() {
        let commit = "Subject\n\n\
             Co-authored-by: Someone <s@example.com>\n\
             Lazybox-Cost: $0.42\n\
             Lazybox-Bogus value without colon\n\
             lazybox-cost: wrong-case";
        assert_eq!(
            extract(commit),
            vec![("Lazybox-Cost".to_string(), "$0.42".to_string())],
        );
    }

    #[test]
    fn agent_labels_cannot_break_out_of_their_line() {
        // A label is the one caller-supplied free-text field and comes from
        // user YAML. A newline in it must not split the trailer paragraph:
        // without sanitizing, this rendered `Lazybox-Agents: claude\nopus ×3`,
        // and extract read the value back as just "claude" — silent truncation.
        // A crafted label likewise must not inject a second `Lazybox-*` line.
        let t = PrTrailers {
            agents: vec![
                AgentCount {
                    label: "claude\nopus".into(),
                    count: 3,
                },
                AgentCount {
                    label: "x\nLazybox-Cost: $999.99".into(),
                    count: 1,
                },
            ],
            ..Default::default()
        };
        let rendered = t.render();
        // The whole set of trailers is exactly one line: no embedded newline.
        assert_eq!(rendered.lines().count(), 1);
        assert!(!rendered.contains('\n'));
        // The forged cost text is neutralized to inline data, not a trailer.
        let extracted = extract(&rendered);
        assert_eq!(
            extracted,
            vec![(
                "Lazybox-Agents".to_string(),
                "claude opus ×3, x Lazybox-Cost: $999.99 ×1".to_string()
            )],
        );
    }
}
