//! What lazybox says upstream while an agent owns a task (#1922).
//!
//! A claim is two things, and the split is deliberate:
//!
//! - **Presence** is the single stable [`WORKING_LABEL_NAME`] label. It rides
//!   free in the poll payload, so "is this claimed?" costs no GitHub call on
//!   any tick, and attaching it needs repository write access — the property
//!   [`DeclarationScope::LabelsOnly`](crate::DeclarationScope) rests on
//!   (#1600).
//! - **Identity** is one sticky comment, marked with
//!   [`WORKING_CLAIM_COMMENT_MARKER`] and edited in place on every heartbeat.
//!   It names the holder, the agent and model, when the work started and when
//!   the lease lapses — a sentence a human reading the thread can act on, and
//!   the lease detail a second box needs when it is about to spawn on a task
//!   somebody else claimed.
//!
//! The predecessor encoded the whole lease in the label *name*
//! (`lazybox:w:<device>:<session>:<expiry>`), which minted one label per claim
//! on the repository forever and told a human nothing. Those labels are still
//! parsed — see [`QualifiedWorkingClaim`](crate::QualifiedWorkingClaim) — so a
//! claim held by a box on the older build keeps being honoured.
//!
//! ## Trust
//!
//! Anyone who can comment can write this marker. A note is therefore only ever
//! a claim when its comment was authored by the authenticated lazybox login;
//! the reader enforces that, not this module (which only renders and parses).
//! The pairing is what makes each half recoverable: a label stripped while the
//! comment stands is detectable at the next decision point and re-attached,
//! and a comment standing with no label — or with a lapsed expiry — is a
//! record of finished work, not a live claim.

use chrono::{DateTime, Utc};

/// HTML marker opening lazybox's sticky claim comment. Invisible in rendered
/// markdown and unique to this comment, so the writer can find its own comment
/// and edit it rather than appending a new one every heartbeat.
pub const WORKING_CLAIM_COMMENT_MARKER: &str = "<!-- lazybox:claim -->";

/// Field prefix for the machine-readable half of the claim comment. Mirrors
/// the `Lazybox-*` commit-trailer convention already used for cost
/// ([`PrTrailers`](crate::PrTrailers)) so a reader of either recognises the
/// shape.
const FIELD_PREFIX: &str = "Lazybox-Claim-";

/// The claim lazybox states in its sticky comment.
///
/// `device` and `session` carry the same truncated fingerprints the legacy
/// label encoded, so a holder is comparable across the two shapes and against
/// the daemon's own claim rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingClaimNote {
    /// Truncated box fingerprint — enough to tell "this box" from "another
    /// box" without naming anyone.
    pub device: String,
    /// Truncated claim-session fingerprint. One lease.
    pub session: String,
    /// The workspace the holder is working in, for a human chasing the work.
    pub workspace: Option<String>,
    /// Agent id (`claude`, `codex`, …) when one is running.
    pub agent: Option<String>,
    /// The model label the agent reported, when it reported one.
    pub model: Option<String>,
    /// When this lease was first taken.
    pub started_at: DateTime<Utc>,
    /// When the holder last renewed it.
    pub heartbeat_at: DateTime<Utc>,
    /// When the lease lapses without a further heartbeat.
    pub expires_at: DateTime<Utc>,
    /// Set once the holder has let the claim go. A released note is a record
    /// of finished work, never a live claim.
    pub released_at: Option<DateTime<Utc>>,
}

impl WorkingClaimNote {
    /// A fresh lease.
    pub fn new(
        device: impl Into<String>,
        session: impl Into<String>,
        started_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Self {
        Self {
            device: device.into(),
            session: session.into(),
            workspace: None,
            agent: None,
            model: None,
            started_at,
            heartbeat_at: started_at,
            expires_at,
            released_at: None,
        }
    }

    /// Whether this note asserts a live claim at `now`: still held, and not
    /// yet lapsed.
    pub fn is_active_at(&self, now: DateTime<Utc>) -> bool {
        self.released_at.is_none() && self.expires_at > now
    }

    /// Whether this note and `other` name the same lease.
    pub fn same_owner(&self, other: &Self) -> bool {
        self.device == other.device && self.session == other.session
    }

    /// Whether this note names the same lease as a legacy qualified label.
    pub fn same_owner_as_label(&self, other: &crate::QualifiedWorkingClaim) -> bool {
        self.device == other.device && self.session == other.session
    }

    /// The comment body: the marker, one sentence a human can act on, then the
    /// machine-readable fields folded away behind a disclosure so a long
    /// thread is not dominated by hex.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(512);
        out.push_str(WORKING_CLAIM_COMMENT_MARKER);
        out.push_str("\n\n");
        out.push_str(&self.headline());
        out.push_str("\n\n<details><summary>claim detail</summary>\n\n```\n");
        for (key, value) in self.fields() {
            out.push_str(FIELD_PREFIX);
            out.push_str(key);
            out.push_str(": ");
            out.push_str(&value);
            out.push('\n');
        }
        out.push_str("```\n\n</details>\n");
        out
    }

    /// The human sentence. Deliberately states what a claim *is* — an
    /// assertion with an expiry — because a reader who takes it as proof that
    /// a process is alive draws the wrong conclusion from a crashed holder.
    fn headline(&self) -> String {
        let who = match (self.agent.as_deref(), self.model.as_deref()) {
            (Some(agent), Some(model)) => format!("`{agent}` ({model})"),
            (Some(agent), None) => format!("`{agent}`"),
            (None, _) => "an agent".to_string(),
        };
        match self.released_at {
            Some(released) => format!(
                "**lazybox has finished working on this.** {who} held it from {} until {}. \
                 The `{}` label has been removed; this comment is the record.",
                stamp(self.started_at),
                stamp(released),
                crate::WORKING_LABEL_NAME,
            ),
            None => format!(
                "**A lazybox agent is working on this.** {who} started at {}, last renewed the \
                 claim at {}, and the claim lapses at {} unless it is renewed again. A claim is \
                 an assertion with an expiry, not proof that a process is alive.",
                stamp(self.started_at),
                stamp(self.heartbeat_at),
                stamp(self.expires_at),
            ),
        }
    }

    fn fields(&self) -> Vec<(&'static str, String)> {
        let mut fields = vec![
            ("Device", self.device.clone()),
            ("Session", self.session.clone()),
        ];
        if let Some(workspace) = &self.workspace {
            fields.push(("Workspace", workspace.clone()));
        }
        if let Some(agent) = &self.agent {
            fields.push(("Agent", agent.clone()));
        }
        if let Some(model) = &self.model {
            fields.push(("Model", model.clone()));
        }
        fields.push(("Started", rfc3339(self.started_at)));
        fields.push(("Heartbeat", rfc3339(self.heartbeat_at)));
        fields.push(("Expires", rfc3339(self.expires_at)));
        if let Some(released) = self.released_at {
            fields.push(("Released", rfc3339(released)));
        }
        fields
    }

    /// Parse a comment body back into a claim.
    ///
    /// `None` unless the body opens with [`WORKING_CLAIM_COMMENT_MARKER`] and
    /// carries every field a lease needs. A body that merely *mentions* the
    /// marker further down is not a claim comment: the marker is how the
    /// writer recognises its own comment, so anchoring the match at the start
    /// keeps a quoted marker inside somebody else's comment from reading as
    /// one.
    ///
    /// This says nothing about who wrote it. Authorship is the caller's check
    /// and it is not optional — see the module docs.
    pub fn parse(body: &str) -> Option<Self> {
        if !body.trim_start().starts_with(WORKING_CLAIM_COMMENT_MARKER) {
            return None;
        }
        let mut device = None;
        let mut session = None;
        let mut workspace = None;
        let mut agent = None;
        let mut model = None;
        let mut started_at = None;
        let mut heartbeat_at = None;
        let mut expires_at = None;
        let mut released_at = None;
        for line in body.lines() {
            let Some(rest) = line.trim().strip_prefix(FIELD_PREFIX) else {
                continue;
            };
            let Some((key, value)) = rest.split_once(':') else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match key {
                "Device" => device = Some(value.to_string()),
                "Session" => session = Some(value.to_string()),
                "Workspace" => workspace = Some(value.to_string()),
                "Agent" => agent = Some(value.to_string()),
                "Model" => model = Some(value.to_string()),
                "Started" => started_at = parse_stamp(value),
                "Heartbeat" => heartbeat_at = parse_stamp(value),
                "Expires" => expires_at = parse_stamp(value),
                "Released" => released_at = parse_stamp(value),
                _ => {}
            }
        }
        let started_at = started_at?;
        Some(Self {
            device: device?,
            session: session?,
            workspace,
            agent,
            model,
            started_at,
            // A note written before this field existed, or one whose stamp is
            // unreadable, is treated as never renewed rather than renewed
            // now: guessing "now" would make a stale lease look fresh, which
            // is the one error that lets the fleet double-spawn.
            heartbeat_at: heartbeat_at.unwrap_or(started_at),
            expires_at: expires_at?,
            released_at,
        })
    }
}

/// `2026-10-03 14:02 UTC` — the stamp a human reads.
fn stamp(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// RFC 3339 to the second — the stamp a parser reads back.
fn rfc3339(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn parse_stamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_790_000_000 + secs, 0).expect("fixture timestamp")
    }

    fn note() -> WorkingClaimNote {
        let mut note = WorkingClaimNote::new(
            "effacd542b611010656e",
            "4c46621014",
            at(0),
            at(lazybox_ttl()),
        );
        note.workspace = Some("github-acme-widget-1922".into());
        note.agent = Some("claude".into());
        note.model = Some("Opus 5".into());
        note.heartbeat_at = at(900);
        note
    }

    fn lazybox_ttl() -> i64 {
        crate::WORKING_CLAIM_TTL_SECS
    }

    #[test]
    fn a_rendered_note_round_trips() {
        let note = note();
        let parsed = WorkingClaimNote::parse(&note.render()).expect("render must parse back");
        assert_eq!(parsed, note);
    }

    #[test]
    fn a_released_note_round_trips_and_is_not_active() {
        let mut note = note();
        note.released_at = Some(at(2_000));
        let parsed = WorkingClaimNote::parse(&note.render()).expect("render must parse back");
        assert_eq!(parsed, note);
        // Released beats an expiry still in the future: the holder said it is
        // done, and that is the stronger statement.
        assert!(note.expires_at > at(2_000));
        assert!(!parsed.is_active_at(at(2_000)));
    }

    #[test]
    fn the_headline_is_human_readable_and_names_agent_model_and_times() {
        let body = note().render();
        for needle in [
            "A lazybox agent is working on this",
            "`claude` (Opus 5)",
            "not proof that a process is alive",
        ] {
            assert!(body.contains(needle), "missing {needle:?} in:\n{body}");
        }
        // The hex is present for machines but folded away for humans.
        assert!(body.contains("<details>"), "{body}");
        assert!(
            body.starts_with(WORKING_CLAIM_COMMENT_MARKER),
            "the marker must open the body so the writer can anchor on it"
        );
    }

    #[test]
    fn a_released_headline_says_the_work_is_over() {
        let mut note = note();
        note.released_at = Some(at(2_000));
        let body = note.render();
        assert!(body.contains("has finished working on this"), "{body}");
        assert!(
            !body.contains("is working on this.**"),
            "a released note must not read as live: {body}"
        );
    }

    #[test]
    fn activity_follows_the_expiry() {
        let note = note();
        assert!(note.is_active_at(at(0)));
        assert!(note.is_active_at(at(lazybox_ttl() - 1)));
        assert!(!note.is_active_at(at(lazybox_ttl())));
        assert!(!note.is_active_at(at(lazybox_ttl() + 1)));
    }

    #[test]
    fn a_body_without_the_marker_is_not_a_claim() {
        let body = note().render();
        let stripped = body
            .strip_prefix(WORKING_CLAIM_COMMENT_MARKER)
            .expect("fixture opens with the marker");
        assert!(
            WorkingClaimNote::parse(stripped).is_none(),
            "the fields alone must not read as a claim"
        );
    }

    /// Someone quoting lazybox's own comment back at it must not produce a
    /// second thing that parses as a claim — the marker is an anchor, not a
    /// substring match.
    #[test]
    fn a_quoted_marker_further_down_is_not_a_claim() {
        let quoted = format!("Why is this label here?\n\n> {}", note().render());
        assert!(WorkingClaimNote::parse(&quoted).is_none(), "{quoted}");
    }

    #[test]
    fn a_note_missing_a_required_field_is_not_a_claim() {
        let body = note().render();
        for field in ["Device", "Session", "Started", "Expires"] {
            let needle = format!("{FIELD_PREFIX}{field}: ");
            let broken = body
                .lines()
                .filter(|line| !line.trim().starts_with(&needle))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                WorkingClaimNote::parse(&broken).is_none(),
                "a note with no {field} must not read as a claim"
            );
        }
    }

    /// The optional fields are optional: an agent that never reported a model,
    /// or a claim taken before a terminal existed, still yields a usable lease.
    #[test]
    fn the_optional_fields_are_optional() {
        let bare = WorkingClaimNote::new("dev", "sess", at(0), at(3600));
        let parsed = WorkingClaimNote::parse(&bare.render()).expect("a bare note must parse");
        assert_eq!(parsed, bare);
        assert_eq!(parsed.agent, None);
        assert_eq!(parsed.model, None);
        assert_eq!(parsed.workspace, None);
        assert!(parsed.render().contains("an agent started at"));
    }

    /// A heartbeat stamp that cannot be read must not default to "now" — that
    /// would refresh a lapsed lease on every read and hide an abandoned claim.
    #[test]
    fn an_unreadable_heartbeat_falls_back_to_the_start_not_now() {
        let body = note()
            .render()
            .replace(&format!("{FIELD_PREFIX}Heartbeat: "), "Not-A-Field: ");
        let parsed = WorkingClaimNote::parse(&body).expect("the lease fields are still there");
        assert_eq!(parsed.heartbeat_at, parsed.started_at);
    }

    #[test]
    fn same_owner_compares_the_lease_not_the_timestamps() {
        let a = note();
        let mut b = note();
        b.heartbeat_at = at(99_999);
        assert!(a.same_owner(&b));
        let mut other = note();
        other.session = "0000000000".into();
        assert!(!a.same_owner(&other));
    }

    /// Migration: a note and a legacy label naming one lease must compare
    /// equal, or the holder's own claim reads as a second, competing owner.
    #[test]
    fn a_note_matches_the_legacy_label_for_the_same_lease() {
        let label = crate::qualified_working_claim_label(
            "effacd542b611010656e0000",
            uuid::Uuid::nil(),
            at(lazybox_ttl()),
        )
        .expect("a well-formed box id yields a label");
        let parsed = crate::QualifiedWorkingClaim::parse(&label).expect("round trip");
        let mut note = note();
        note.device = parsed.device.clone();
        note.session = parsed.session.clone();
        assert!(note.same_owner_as_label(&parsed));
    }
}
