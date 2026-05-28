//! Pure-logic classifier for `pilot slack prune`. Takes a Slack
//! channel listing + the set of channel names belonging to live
//! pilot terminals + a filter, returns a disposition per channel.
//!
//! Lives in `pilot-slack` (not `pilot-tui`) so the classification is
//! decoupled from the store, the TUI, and the network. The CLI
//! wrapper feeds it `Vec<ChannelInfo>` from `conversations.list` and
//! `HashSet<String>` from the daemon's session table, and decides
//! what to print and what to archive.
//!
//! ## Naming pattern
//!
//! Pilot-managed per-(session, agent) channels are formatted by
//! [`channel_name_for_terminal`](crate::api::channel_name_for_terminal)
//! as `<prefix><workspace>-<session8-hex>-<agent>`, all slugged so
//! every character is ASCII-safe for Slack. The classifier matches
//! that shape structurally: prefix-stripped, ≥3 `-`-separated
//! segments, the second-to-last segment exactly 8 lowercase hex chars.
//! Anything else is `NotManaged` and gets skipped — pruning must
//! never touch channels created by humans / other bots.

use std::collections::HashSet;

/// One channel as returned by `conversations.list`. Trimmed down to
/// the fields the classifier needs so the input is easy to fake in
/// tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    pub id: String,
    pub name: String,
    /// Unix seconds the channel was created. Sourced from Slack's
    /// `created` field on each listing entry.
    pub created_unix: i64,
}

/// What `pilot slack prune` should do with a given channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// Channel doesn't match pilot's `<prefix><ws>-<8hex>-<agent>`
    /// pattern. Skip silently — user-created channels must never be
    /// touched by prune.
    NotManaged,
    /// Channel matches the naming pattern AND a pilot terminal for
    /// it still exists in the store. Skip even if `--older-than`
    /// would otherwise nominate it: live sessions can't be safely
    /// archived without breaking their slack mirror.
    LiveSession,
    /// Channel matches the pattern and is stale, but the user's
    /// filter (`--older-than` / `--workspace`) excluded it. Skip
    /// with a less-verbose reason than `NotManaged`.
    FilteredOut,
    /// Stale pilot-managed channel: archive it.
    Prune,
}

/// User-provided filters on what to prune.
///
/// Both fields are AND-ed: a channel must pass every set filter to
/// be a prune candidate. `None` on a field disables that filter.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Skip channels younger than this many seconds (channel
    /// creation timestamp). `None` → no age filter.
    pub older_than_secs: Option<i64>,
    /// Slugged workspace key (lowercase, hyphenated). If set, only
    /// channels whose parsed workspace portion contains this string
    /// are pruned. Pass through [`crate::api::sluggify`] before
    /// constructing so user-typed keys like `acme/widget#186` match
    /// channels named `acme-widget-186-...`.
    pub workspace_slug: Option<String>,
}

/// Classification result with parsed parts attached when the
/// channel is pilot-managed. Callers print the parts in the
/// dry-run / confirmation table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    pub channel: ChannelInfo,
    pub disposition: Disposition,
    /// Parsed `(workspace_slug, session8, agent)` when the name
    /// matches pilot's pattern. `None` for `NotManaged`.
    pub parts: Option<ChannelParts>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelParts {
    pub workspace_slug: String,
    pub session8: String,
    pub agent: String,
}

/// Structurally parse a channel name into its pilot parts. Returns
/// `None` if the name doesn't fit `<prefix><ws>-<8hex>-<agent>`.
///
/// The agent segment is required to be non-empty AND contain at
/// least one non-hex char — otherwise an unrelated channel like
/// `deadbeef-cafef00d-12345678` (three hex segments) would parse as
/// `ws=deadbeef, session8=cafef00d, agent=12345678`. Real pilot
/// agent ids are `claude` / `codex` / `cursor` / `shell` etc, all
/// containing at least one alpha char outside `[a-f]`.
pub fn parse_channel_name(name: &str, prefix: &str) -> Option<ChannelParts> {
    let body = name.strip_prefix(prefix)?;
    let segments: Vec<&str> = body.split('-').collect();
    if segments.len() < 3 {
        return None;
    }
    let agent = *segments.last()?;
    let session8 = segments[segments.len() - 2];
    if !is_eight_hex_lower(session8) {
        return None;
    }
    if agent.is_empty() || is_all_hex_lower(agent) {
        // All-hex agent rules out a pure-hex coincidence. See doc
        // above.
        return None;
    }
    let workspace = segments[..segments.len() - 2].join("-");
    if workspace.is_empty() {
        return None;
    }
    Some(ChannelParts {
        workspace_slug: workspace,
        session8: session8.to_string(),
        agent: agent.to_string(),
    })
}

fn is_eight_hex_lower(s: &str) -> bool {
    s.len() == 8 && s.chars().all(is_hex_lower)
}

fn is_all_hex_lower(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_hex_lower)
}

fn is_hex_lower(c: char) -> bool {
    c.is_ascii_digit() || ('a'..='f').contains(&c)
}

/// Classify one channel. The single-channel entry point — see
/// [`plan`] for the bulk version that also tracks counts.
pub fn classify(
    channel: &ChannelInfo,
    prefix: &str,
    live_channel_names: &HashSet<String>,
    filter: &Filter,
    now_unix: i64,
) -> Classified {
    let Some(parts) = parse_channel_name(&channel.name, prefix) else {
        return Classified {
            channel: channel.clone(),
            disposition: Disposition::NotManaged,
            parts: None,
        };
    };
    if live_channel_names.contains(&channel.name) {
        return Classified {
            channel: channel.clone(),
            disposition: Disposition::LiveSession,
            parts: Some(parts),
        };
    }
    if let Some(secs) = filter.older_than_secs {
        let age = now_unix.saturating_sub(channel.created_unix);
        if age < secs {
            return Classified {
                channel: channel.clone(),
                disposition: Disposition::FilteredOut,
                parts: Some(parts),
            };
        }
    }
    if let Some(ws) = &filter.workspace_slug
        && !parts.workspace_slug.contains(ws)
    {
        return Classified {
            channel: channel.clone(),
            disposition: Disposition::FilteredOut,
            parts: Some(parts),
        };
    }
    Classified {
        channel: channel.clone(),
        disposition: Disposition::Prune,
        parts: Some(parts),
    }
}

/// Aggregate prune plan over a whole Slack workspace's channel
/// listing. The returned `archive` vec is the list the CLI offers
/// to the user; the counters drive the summary line.
#[derive(Debug, Default)]
pub struct PrunePlan {
    pub archive: Vec<Classified>,
    pub skipped_not_managed: usize,
    pub skipped_live: usize,
    pub skipped_filter: usize,
}

pub fn plan(
    channels: &[ChannelInfo],
    prefix: &str,
    live_channel_names: &HashSet<String>,
    filter: &Filter,
    now_unix: i64,
) -> PrunePlan {
    let mut plan = PrunePlan::default();
    for c in channels {
        let result = classify(c, prefix, live_channel_names, filter, now_unix);
        match result.disposition {
            Disposition::Prune => plan.archive.push(result),
            Disposition::NotManaged => plan.skipped_not_managed += 1,
            Disposition::LiveSession => plan.skipped_live += 1,
            Disposition::FilteredOut => plan.skipped_filter += 1,
        }
    }
    plan
}

/// Parse `7d` / `12h` / `45m` / `3600s` into seconds. Bare numbers
/// fall back to seconds for symmetry with crontab.
///
/// `pilot slack prune --older-than` takes this shape directly. The
/// units are deliberately limited — anything finer than seconds is
/// nonsense for channel age, anything coarser than days encourages
/// users to commit "delete everything older than a month" patterns
/// that nuke long-lived agent worktrees.
pub fn parse_duration(input: &str) -> Result<i64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let (num, unit_secs): (&str, i64) = if let Some(n) = s.strip_suffix('d') {
        (n, 86_400)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else {
        (s, 1)
    };
    let value: i64 = num
        .parse()
        .map_err(|_| format!("not a number: `{num}` (try `7d`, `12h`, `30m`, `3600s`)"))?;
    if value < 0 {
        return Err(format!("duration must be non-negative (got {value})"));
    }
    value
        .checked_mul(unit_secs)
        .ok_or_else(|| format!("duration `{input}` overflowed i64"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{channel_name_for_terminal, sluggify};

    fn info(name: &str, created: i64) -> ChannelInfo {
        ChannelInfo {
            id: format!("C-{name}"),
            name: name.into(),
            created_unix: created,
        }
    }

    // ── parse_channel_name ──────────────────────────────────────

    #[test]
    fn parse_recognizes_canonical_shape() {
        let parts = parse_channel_name("github-acme-widget-186-a3f1c277-claude", "").unwrap();
        assert_eq!(parts.workspace_slug, "github-acme-widget-186");
        assert_eq!(parts.session8, "a3f1c277");
        assert_eq!(parts.agent, "claude");
    }

    #[test]
    fn parse_honors_prefix() {
        let parts =
            parse_channel_name("pr-github-acme-widget-186-deadbeef-codex", "pr-").unwrap();
        assert_eq!(parts.workspace_slug, "github-acme-widget-186");
        assert_eq!(parts.agent, "codex");
    }

    #[test]
    fn parse_rejects_when_prefix_missing() {
        assert!(parse_channel_name("github-foo-a3f1c277-claude", "pr-").is_none());
    }

    #[test]
    fn parse_rejects_short_inputs() {
        assert!(parse_channel_name("foo", "").is_none());
        assert!(parse_channel_name("foo-bar", "").is_none());
    }

    #[test]
    fn parse_rejects_wrong_session_length() {
        // Only 7 hex chars where 8 expected.
        assert!(parse_channel_name("ws-abc1234-claude", "").is_none());
        // 9 hex chars.
        assert!(parse_channel_name("ws-abc123456-claude", "").is_none());
    }

    #[test]
    fn parse_rejects_non_hex_session() {
        // Has a `g` — not a hex digit.
        assert!(parse_channel_name("ws-deadbeeg-claude", "").is_none());
    }

    #[test]
    fn parse_rejects_uppercase_session() {
        // Sluggify lowercases everything — uppercase hex means the
        // channel wasn't built by us.
        assert!(parse_channel_name("ws-DEADBEEF-claude", "").is_none());
    }

    #[test]
    fn parse_rejects_when_agent_is_all_hex() {
        // Without this rule, `ws-deadbeef-cafef00d` would parse as
        // agent="cafef00d", session8="deadbeef". A real agent id
        // contains at least one non-hex char.
        assert!(parse_channel_name("ws-deadbeef-cafef00d", "").is_none());
    }

    #[test]
    fn parse_accepts_mixed_alpha_agent() {
        // Agent names can contain hex-looking chars as long as
        // they're not ALL hex.
        let p = parse_channel_name("ws-deadbeef-shell", "").unwrap();
        assert_eq!(p.agent, "shell");
    }

    #[test]
    fn parse_round_trips_with_channel_name_for_terminal() {
        // Whatever channel_name_for_terminal produces must be
        // parseable. Pin that contract — if the naming format
        // changes, this test fails loudly.
        let name = channel_name_for_terminal(
            "github-acme-widget-186",
            "a3f1c277-9abc-4d51-8f01-deadbeef0001",
            "claude",
            "pr-",
        );
        let parts = parse_channel_name(&name, "pr-").unwrap();
        assert_eq!(parts.workspace_slug, "github-acme-widget-186");
        assert_eq!(parts.session8, "a3f1c277");
        assert_eq!(parts.agent, "claude");
    }

    // ── classify ────────────────────────────────────────────────

    fn empty_live() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn classify_skips_unmanaged_channels() {
        // Random `#general`-style channel.
        let c = info("general", 0);
        let out = classify(&c, "", &empty_live(), &Filter::default(), 1_000_000);
        assert_eq!(out.disposition, Disposition::NotManaged);
        assert!(out.parts.is_none());
    }

    #[test]
    fn classify_skips_live_session() {
        let mut live = HashSet::new();
        live.insert("ws-deadbeef-claude".to_string());
        let c = info("ws-deadbeef-claude", 0);
        let out = classify(&c, "", &live, &Filter::default(), 1_000_000);
        assert_eq!(out.disposition, Disposition::LiveSession);
    }

    #[test]
    fn classify_skips_live_session_even_when_older_than_set() {
        // Acceptance: "Channels for still-live terminals are skipped
        // regardless of age."
        let mut live = HashSet::new();
        live.insert("ws-deadbeef-claude".to_string());
        let c = info("ws-deadbeef-claude", 0);
        let filter = Filter {
            older_than_secs: Some(1),
            workspace_slug: None,
        };
        let out = classify(&c, "", &live, &filter, 1_000_000);
        assert_eq!(out.disposition, Disposition::LiveSession);
    }

    #[test]
    fn classify_prunes_when_no_filter() {
        let c = info("ws-deadbeef-claude", 0);
        let out = classify(&c, "", &empty_live(), &Filter::default(), 1_000_000);
        assert_eq!(out.disposition, Disposition::Prune);
        assert_eq!(out.parts.unwrap().agent, "claude");
    }

    #[test]
    fn classify_filters_by_age() {
        let now = 1_000_000;
        // Channel created 100 seconds ago.
        let c = info("ws-deadbeef-claude", now - 100);
        // Older-than 200s → too young → filtered out.
        let filter = Filter {
            older_than_secs: Some(200),
            workspace_slug: None,
        };
        let out = classify(&c, "", &empty_live(), &filter, now);
        assert_eq!(out.disposition, Disposition::FilteredOut);
        // Older-than 50s → old enough → prune.
        let filter = Filter {
            older_than_secs: Some(50),
            workspace_slug: None,
        };
        let out = classify(&c, "", &empty_live(), &filter, now);
        assert_eq!(out.disposition, Disposition::Prune);
    }

    #[test]
    fn classify_filters_by_workspace_substring() {
        let c = info("github-acme-widget-186-deadbeef-claude", 0);
        let filter = Filter {
            older_than_secs: None,
            workspace_slug: Some(sluggify("acme/widget")),
        };
        let out = classify(&c, "", &empty_live(), &filter, 1_000_000);
        assert_eq!(out.disposition, Disposition::Prune);

        let filter = Filter {
            older_than_secs: None,
            workspace_slug: Some("globex".into()),
        };
        let out = classify(&c, "", &empty_live(), &filter, 1_000_000);
        assert_eq!(out.disposition, Disposition::FilteredOut);
    }

    // ── plan ────────────────────────────────────────────────────

    #[test]
    fn plan_counts_each_disposition() {
        let live: HashSet<String> = ["ws-aaaaaaaa-claude"].iter().map(|s| s.to_string()).collect();
        let channels = vec![
            info("general", 0),                        // NotManaged
            info("random", 0),                         // NotManaged
            info("ws-aaaaaaaa-claude", 0),             // LiveSession
            info("ws-bbbbbbbb-claude", 100),           // would Prune
            info("ws-cccccccc-codex", 999_900),        // FilteredOut (too young)
        ];
        let filter = Filter {
            older_than_secs: Some(100_000),
            workspace_slug: None,
        };
        let p = plan(&channels, "", &live, &filter, 1_000_000);
        assert_eq!(p.archive.len(), 1);
        assert_eq!(p.archive[0].channel.name, "ws-bbbbbbbb-claude");
        assert_eq!(p.skipped_not_managed, 2);
        assert_eq!(p.skipped_live, 1);
        assert_eq!(p.skipped_filter, 1);
    }

    // ── parse_duration ──────────────────────────────────────────

    #[test]
    fn parse_duration_handles_supported_units() {
        assert_eq!(parse_duration("7d").unwrap(), 7 * 86_400);
        assert_eq!(parse_duration("12h").unwrap(), 12 * 3_600);
        assert_eq!(parse_duration("30m").unwrap(), 30 * 60);
        assert_eq!(parse_duration("3600s").unwrap(), 3600);
        assert_eq!(parse_duration("90").unwrap(), 90);
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("foo").is_err());
        assert!(parse_duration("7w").is_err()); // unit `w` lands in num
        assert!(parse_duration("-5d").is_err());
    }
}
