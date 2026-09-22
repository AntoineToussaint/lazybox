//! Review artifacts — the durable handoff from a review to a fixer (#1732).
//!
//! `deepreview` and `fixall` are two halves of one workflow, and until now the
//! only thing joining them was the agent's own conversation: FIXALL asked for
//! "the findings from the review you just produced". That works for exactly one
//! shape of run — same agent, same session, same scrollback — and silently
//! produces an *empty fixer* in every other: a fresh session, a cheaper model, a
//! different CLI, a daemon restart. Nothing reports the loss, because a fixer
//! with no findings still finishes and still says it is done.
//!
//! So the findings become a record instead of a memory. A review submits a
//! [`ReviewArtifact`]: the readable report *plus* structured findings carrying
//! stable ids, file anchors, the evidence that makes each one real, and the
//! remediation it suggests. A fixer binds one artifact by id
//! ([`select_report`]) and works from it — any agent, any strength, any time.
//! Its own outcomes land in a separate [`ReviewResult`], so the original report
//! is never mutated by the thing being judged against it.
//!
//! Three distinctions this module exists to keep straight, each of which read
//! identically before it:
//!
//! - **Draft vs completed.** A malformed or incomplete submission is kept
//!   ([`ArtifactStatus::Draft`], with [`ReviewArtifact::defects`] naming why) but
//!   is never bindable. A review is done when it has been *ingested*, not when
//!   the agent stopped typing.
//! - **Zero findings vs no report.** A clean review is a completed artifact
//!   with an empty findings list, and binds like any other. That is the
//!   opposite of [`ReportSelection::Missing`], where a fixer must not start.
//! - **Current vs stale.** A report is pinned to the exact tree it read
//!   ([`ReviewScope`]). When the head moved or the dirty tree it captured is
//!   gone, the report survives but binds as [`Freshness`]-flagged, requiring
//!   per-finding revalidation rather than blind fixing.

use serde::{Deserialize, Serialize};

/// Schema version stamped on every artifact written by this module.
///
/// Persisted alongside the data, so a future shape change can be recognized on
/// read instead of mis-parsed: an artifact whose `schema` is not this value is
/// not compatible and must not be bound by a fixer.
pub const REVIEW_SCHEMA_VERSION: u32 = 1;

/// How bad a finding is. Ordered worst-first so `sort` puts blockers on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocker,
    Major,
    Minor,
}

impl Severity {
    /// Parse an agent-written severity word. Unknown or empty text is *not*
    /// silently downgraded to a nit — it is refused, so a typo can't quietly
    /// demote a blocker to the bottom of the fixer's queue.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "blocker" | "critical" | "high" => Some(Self::Blocker),
            "major" | "medium" => Some(Self::Major),
            "minor" | "low" | "nit" => Some(Self::Minor),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Blocker => "blocker",
            Self::Major => "major",
            Self::Minor => "minor",
        }
    }
}

/// Where a finding lives — the `file:line` anchor a review already writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAnchor {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

impl FileAnchor {
    /// Parse `path/to/file.rs:120` — the form every review body already asks
    /// for. A trailing segment that isn't a line number is kept as part of the
    /// path (Windows-style `C:\…` and a file literally named `x:y` both survive
    /// rather than losing their tail).
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        match text.rsplit_once(':') {
            Some((file, line)) if !file.is_empty() => match line.trim().parse::<u32>() {
                Ok(line) => Some(Self {
                    file: file.to_string(),
                    line: Some(line),
                }),
                // `src/main.rs:` has a separator and no line; the colon is
                // punctuation, not part of the path.
                Err(_) => {
                    let file = text.trim_end_matches(':');
                    (!file.is_empty()).then(|| Self {
                        file: file.to_string(),
                        line: None,
                    })
                }
            },
            _ => Some(Self {
                file: text.to_string(),
                line: None,
            }),
        }
    }

    pub fn render(&self) -> String {
        match self.line {
            Some(line) => format!("{}:{line}", self.file),
            None => self.file.clone(),
        }
    }
}

/// One finding, as the fixer receives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable within its report — the handle a [`FindingOutcome`] refers back
    /// to. Assigned at ingestion when the submission omits it.
    pub id: String,
    pub title: String,
    pub severity: Severity,
    /// At least one, enforced at ingestion: a finding a fixer cannot locate is
    /// not actionable, however well argued.
    pub anchors: Vec<FileAnchor>,
    /// Why this is real — the concrete input or state that produces the wrong
    /// result. This is the reasoning that would otherwise die with the
    /// reviewer's session.
    pub evidence: String,
    /// What the reviewer suggests doing about it. Advisory: the fixer owns the
    /// real cause, not this text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub remediation: String,
    /// What should pass once it is fixed (a test name, a command).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
}

/// The exact tree a report was taken against.
///
/// This is what makes staleness answerable instead of assumed. `head_sha` pins
/// the commit; `dirty_digest` pins the uncommitted diff on top of it, because a
/// review of a dirty worktree describes code that exists in no commit at all
/// and must not be silently re-bound to a later, different dirty tree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewScope {
    /// What was reviewed, in words: `"diff vs main"`, `"PR #1732 head"`.
    /// Two reports whose labels differ describe different work, which is what
    /// makes a selection [`ReportSelection::Ambiguous`] rather than a guess.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// Digest of the uncommitted diff at review time, when the tree was dirty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty_digest: Option<String>,
}

/// Whether an artifact is bindable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    /// Ingestion found defects. Retained (the prose is still worth reading) but
    /// never selected — see [`ReviewArtifact::defects`].
    Draft,
    Completed,
}

/// How an artifact entered the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOrigin {
    /// Submitted by the agent that performed the review.
    Submitted,
    /// Captured after the fact from a legacy in-conversation review. Explicit:
    /// there is no transcript parser, someone exported it deliberately. An
    /// import carries no `head_sha` of its own unless the importer supplied
    /// one, so it binds as [`Freshness::Unknown`].
    Imported,
}

/// A persisted review report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewArtifact {
    pub schema: u32,
    /// Stable, workspace-scoped handle (`r3`). What a fixer binds.
    pub id: String,
    /// The workspace this review belongs to — a session key string.
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The run that produced it, so a report traces back to its session.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub scope: ReviewScope,
    pub status: ArtifactStatus,
    pub origin: ReviewOrigin,
    pub findings: Vec<Finding>,
    /// Validation the review expects to pass once its findings are addressed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<String>,
    /// The readable report, kept verbatim. The structured findings are a
    /// projection of it, never a replacement: a summary that loses the
    /// reviewer's argument is the failure this whole module exists to avoid.
    pub report: String,
    /// Why this is a draft. Empty on a completed artifact.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub defects: Vec<String>,
    pub created_at_ms: i64,
}

impl ReviewArtifact {
    /// Bindable by a fixer: ingested cleanly and written by a schema this build
    /// understands.
    pub fn is_bindable(&self) -> bool {
        self.status == ArtifactStatus::Completed && self.schema == REVIEW_SCHEMA_VERSION
    }

    /// How this report relates to `current`, the tree as it is now.
    pub fn freshness(&self, current: &ReviewScope) -> Freshness {
        let (Some(reviewed), Some(now)) = (&self.scope.head_sha, &current.head_sha) else {
            return Freshness::Unknown;
        };
        if reviewed != now {
            return Freshness::HeadMoved {
                reviewed: reviewed.clone(),
                current: now.clone(),
            };
        }
        if self.scope.dirty_digest != current.dirty_digest {
            return Freshness::WorktreeChanged;
        }
        Freshness::Current
    }
}

/// How far a report has drifted from the tree a fixer is about to touch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Freshness {
    /// Same commit, same uncommitted diff (or both clean).
    Current,
    /// The commit moved under the report.
    HeadMoved { reviewed: String, current: String },
    /// Same commit, different uncommitted work — the review read a dirty tree
    /// that has since changed, or a clean tree that is now dirty.
    WorktreeChanged,
    /// One side carries no `head_sha` (an import, or a caller that supplied no
    /// scope). Not provably current, so it is treated as needing revalidation
    /// rather than assumed fresh.
    Unknown,
}

impl Freshness {
    /// Whether the fixer must re-check each finding against the code as it is
    /// now before acting on it. Only a provably identical tree escapes.
    pub fn requires_revalidation(&self) -> bool {
        !matches!(self, Self::Current)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::HeadMoved { .. } => "head moved",
            Self::WorktreeChanged => "worktree changed",
            Self::Unknown => "unknown",
        }
    }
}

/// What a fixer should do about the reports it found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum ReportSelection {
    /// Nothing bindable. The fixer stops and offers to run a review or import
    /// findings; it never starts empty.
    Missing,
    /// Exactly one compatible report. `freshness` says whether its findings
    /// need revalidating first.
    Bound { id: String, freshness: Freshness },
    /// Several reports describe different work and no rule picks between them.
    /// Resolved by a person naming one, never by "latest wins" — which is how a
    /// fixer ends up applying a PR review to a scratch branch.
    Ambiguous { candidates: Vec<String> },
}

/// Bind one report for a fixer run, out of everything persisted for a
/// workspace.
///
/// Reports that are drafts, or written by another schema, are not candidates —
/// they cannot be acted on, and treating them as absent is what makes "run a
/// review first" the honest answer.
///
/// Among the rest, a report matching the tree as it is now beats one that
/// doesn't, which is why the scope split comes before the recency tiebreak:
/// a fresh review of the current head is more useful than a newer review of a
/// tree that has since moved. Within a tier, reports whose [`ReviewScope::label`]
/// disagrees describe different work, so the choice is a person's
/// ([`ReportSelection::Ambiguous`]); reports that agree collapse to the newest.
///
/// `reports` is expected oldest-first, as the store returns them, which is what
/// resolves a timestamp tie to the report written second.
pub fn select_report(reports: &[ReviewArtifact], current: &ReviewScope) -> ReportSelection {
    let mut fresh: Vec<&ReviewArtifact> = Vec::new();
    let mut stale: Vec<&ReviewArtifact> = Vec::new();
    for report in reports.iter().filter(|r| r.is_bindable()) {
        if report.freshness(current) == Freshness::Current {
            fresh.push(report);
        } else {
            stale.push(report);
        }
    }
    let tier = if !fresh.is_empty() { fresh } else { stale };
    if tier.is_empty() {
        return ReportSelection::Missing;
    }
    // Ambiguity is keyed on the label, and the label is free text that
    // defaults to empty. Two same-head reviews of genuinely different work —
    // one crate versus the whole diff — both unlabelled would dedup to a
    // single `""` and bind the newest silently, which is exactly the
    // ambiguous-scope case that has to be explicit. An empty label proves
    // nothing about what was reviewed, so it cannot be used to prove two
    // reports describe the same work; only a human can resolve that.
    let mut labels: Vec<&str> = tier.iter().map(|r| r.scope.label.trim()).collect();
    labels.sort_unstable();
    labels.dedup();
    let unprovable = tier.len() > 1 && labels.iter().any(|label| label.is_empty());
    if labels.len() > 1 || unprovable {
        let mut candidates: Vec<&&ReviewArtifact> = tier.iter().collect();
        candidates.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
        return ReportSelection::Ambiguous {
            candidates: candidates.iter().map(|r| r.id.clone()).collect(),
        };
    }
    // `max_by_key` yields the LAST maximum, and reports arrive oldest-first,
    // so two reports stamped in the same millisecond resolve to the one
    // written second rather than to whichever id happens to sort higher.
    let newest = tier
        .iter()
        .max_by_key(|r| r.created_at_ms)
        .expect("tier is non-empty");
    ReportSelection::Bound {
        id: newest.id.clone(),
        freshness: newest.freshness(current),
    }
}

/// A finding as submitted, before ingestion assigns ids and validates it.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FindingInput {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub severity: String,
    /// `file:line` strings, as a review body already writes them.
    #[serde(default)]
    pub anchors: Vec<String>,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub remediation: String,
    #[serde(default)]
    pub checks: Vec<String>,
}

/// A whole review as submitted.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ReviewSubmission {
    pub report: String,
    #[serde(default)]
    pub findings: Vec<FindingInput>,
    #[serde(default)]
    pub scope: ReviewScope,
    #[serde(default)]
    pub checks: Vec<String>,
    #[serde(default)]
    pub open_questions: Vec<String>,
}

/// Identity the daemon knows and the submitter does not get to claim.
#[derive(Debug, Clone)]
pub struct ReviewIngest {
    pub id: String,
    pub workspace: String,
    pub repo: Option<String>,
    pub run_id: String,
    pub agent: Option<String>,
    pub origin: ReviewOrigin,
    pub created_at_ms: i64,
}

impl ReviewSubmission {
    /// Ingest this submission into an artifact.
    ///
    /// Always produces one: a submission that fails validation is kept as a
    /// [`ArtifactStatus::Draft`] listing its defects, because throwing away a
    /// reviewer's prose over a missing anchor loses the expensive half of the
    /// work. Only the *bindable* flag turns on validity.
    ///
    /// An empty findings list is not a defect. A clean review is a real
    /// result, and refusing to complete it would leave the fixer unable to tell
    /// "nothing to fix" from "the review never ran".
    pub fn into_artifact(self, ingest: ReviewIngest) -> ReviewArtifact {
        let mut defects = Vec::new();
        if self.report.trim().is_empty() {
            defects.push("report is empty — submit the readable review text".to_string());
        }
        if ingest.origin == ReviewOrigin::Submitted && self.scope.head_sha.is_none() {
            defects.push(
                "scope.head_sha is missing — pass `git rev-parse HEAD` so a fixer can tell \
                 whether these findings still describe the tree"
                    .to_string(),
            );
        }

        let mut findings = Vec::with_capacity(self.findings.len());
        let mut seen: Vec<String> = Vec::with_capacity(self.findings.len());
        for (index, input) in self.findings.into_iter().enumerate() {
            let position = index + 1;
            let id = match input.id.as_deref().map(str::trim) {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => format!("f{position}"),
            };
            if seen.contains(&id) {
                defects.push(format!(
                    "finding {position}: duplicate id {id:?} — ids must be unique within a report"
                ));
                continue;
            }
            seen.push(id.clone());
            let title = input.title.trim();
            if title.is_empty() {
                defects.push(format!("finding {id}: title is empty"));
            }
            let Some(severity) = Severity::parse(&input.severity) else {
                defects.push(format!(
                    "finding {id}: severity {:?} is not one of blocker / major / minor",
                    input.severity
                ));
                continue;
            };
            let anchors: Vec<FileAnchor> = input
                .anchors
                .iter()
                .filter_map(|a| FileAnchor::parse(a))
                .collect();
            if anchors.is_empty() {
                defects.push(format!(
                    "finding {id}: no `file:line` anchor — a finding a fixer cannot locate is not \
                     actionable"
                ));
            }
            let evidence = input.evidence.trim();
            if evidence.is_empty() {
                defects.push(format!(
                    "finding {id}: evidence is empty — name the concrete input or state that \
                     produces the wrong result"
                ));
            }
            findings.push(Finding {
                id,
                title: title.to_string(),
                severity,
                anchors,
                evidence: evidence.to_string(),
                remediation: input.remediation.trim().to_string(),
                checks: trimmed_lines(input.checks),
            });
        }

        let status = if defects.is_empty() {
            ArtifactStatus::Completed
        } else {
            ArtifactStatus::Draft
        };
        ReviewArtifact {
            schema: REVIEW_SCHEMA_VERSION,
            id: ingest.id,
            workspace: ingest.workspace,
            repo: ingest.repo,
            run_id: ingest.run_id,
            agent: ingest.agent,
            scope: self.scope,
            status,
            origin: ingest.origin,
            findings,
            checks: trimmed_lines(self.checks),
            open_questions: trimmed_lines(self.open_questions),
            report: self.report.trim().to_string(),
            defects,
            created_at_ms: ingest.created_at_ms,
        }
    }
}

/// What the fixer did about one finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// Changed the code so the finding no longer holds.
    Fixed,
    /// The finding was real but the tree already handles it — typically a
    /// stale report whose head moved.
    AlreadyResolved,
    /// Real, not fixed here, and the reason is external (needs a decision,
    /// belongs in another repo).
    Blocked,
    /// Refuted with a concrete, falsifiable reason. The one disposition that
    /// claims the reviewer was wrong, so it carries the burden of proof.
    Refuted,
}

impl Disposition {
    pub fn parse(text: &str) -> Option<Self> {
        match text
            .trim()
            .to_ascii_lowercase()
            .replace(['-', ' '], "_")
            .as_str()
        {
            "fixed" => Some(Self::Fixed),
            "already_resolved" => Some(Self::AlreadyResolved),
            "blocked" => Some(Self::Blocked),
            "refuted" => Some(Self::Refuted),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::AlreadyResolved => "already_resolved",
            Self::Blocked => "blocked",
            Self::Refuted => "refuted",
        }
    }
}

/// One finding's fate, as persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingOutcome {
    pub finding_id: String,
    pub disposition: Disposition,
    /// What backs the claim: the change made, or the reason it doesn't hold.
    pub evidence: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
}

/// A fixer run's result, bound to the report it worked from.
///
/// Separate from [`ReviewArtifact`] on purpose: the report is the reviewer's
/// claim and the result is the fixer's answer to it. Folding the answer back
/// into the claim would destroy the only record of what was originally
/// asserted, which is exactly what a later reader needs to judge a `refuted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewResult {
    pub schema: u32,
    pub id: String,
    /// The [`ReviewArtifact::id`] this answers.
    pub report_id: String,
    pub workspace: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub status: ArtifactStatus,
    pub outcomes: Vec<FindingOutcome>,
    /// Findings in the report that this result says nothing about. A result
    /// with any of these is a draft: silence is not a disposition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncovered: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub defects: Vec<String>,
    pub created_at_ms: i64,
}

/// One finding's outcome as submitted.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct OutcomeInput {
    #[serde(default)]
    pub finding_id: String,
    #[serde(default)]
    pub disposition: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub commits: Vec<String>,
    #[serde(default)]
    pub checks: Vec<String>,
}

/// A fixer run's report, as submitted.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ReviewResultSubmission {
    pub report_id: String,
    #[serde(default)]
    pub outcomes: Vec<OutcomeInput>,
    #[serde(default)]
    pub checks: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

/// Identity for an ingested result, as [`ReviewIngest`] is for a report.
#[derive(Debug, Clone)]
pub struct ResultIngest {
    pub id: String,
    pub run_id: String,
    pub agent: Option<String>,
    pub created_at_ms: i64,
}

impl ReviewResultSubmission {
    /// Ingest this result against the report it claims to answer.
    ///
    /// `report` is the authority on which findings exist, so an outcome naming
    /// an unknown finding is a defect rather than a new row — otherwise a fixer
    /// could report progress on findings nobody made.
    pub fn into_artifact(self, report: &ReviewArtifact, ingest: ResultIngest) -> ReviewResult {
        let mut defects = Vec::new();
        // A draft was never bindable, so a result against one means the fixer
        // worked from something no selection would have handed it. The work is
        // still recorded — refusing it would destroy the only copy — but the
        // result carries the reason it is not trustworthy.
        if !report.is_bindable() {
            defects.push(format!(
                "report {} is not bindable ({:?}); a result against it means the fixer did not \
                 work from a selected report",
                report.id, report.status
            ));
        }
        let mut outcomes = Vec::with_capacity(self.outcomes.len());
        let mut covered: Vec<String> = Vec::with_capacity(self.outcomes.len());

        for (index, input) in self.outcomes.into_iter().enumerate() {
            let finding_id = input.finding_id.trim().to_string();
            if finding_id.is_empty() {
                defects.push(format!("outcome {}: finding_id is empty", index + 1));
                continue;
            }
            if !report.findings.iter().any(|f| f.id == finding_id) {
                defects.push(format!(
                    "outcome {finding_id}: report {} has no such finding",
                    report.id
                ));
                continue;
            }
            if covered.contains(&finding_id) {
                defects.push(format!("outcome {finding_id}: reported twice"));
                continue;
            }
            let Some(disposition) = Disposition::parse(&input.disposition) else {
                defects.push(format!(
                    "outcome {finding_id}: disposition {:?} is not one of fixed / \
                     already_resolved / blocked / refuted",
                    input.disposition
                ));
                continue;
            };
            let evidence = input.evidence.trim();
            if evidence.is_empty() {
                defects.push(format!(
                    "outcome {finding_id}: evidence is empty — a {} needs the change or the \
                     falsifiable reason behind it",
                    disposition.label()
                ));
            }
            covered.push(finding_id.clone());
            outcomes.push(FindingOutcome {
                finding_id,
                disposition,
                evidence: evidence.to_string(),
                commits: trimmed_lines(input.commits),
                checks: trimmed_lines(input.checks),
            });
        }

        let uncovered: Vec<String> = report
            .findings
            .iter()
            .filter(|f| !covered.contains(&f.id))
            .map(|f| f.id.clone())
            .collect();
        if !uncovered.is_empty() {
            defects.push(format!(
                "no outcome for {} — every finding needs one, including the ones you refute",
                uncovered.join(", ")
            ));
        }

        let status = if defects.is_empty() {
            ArtifactStatus::Completed
        } else {
            ArtifactStatus::Draft
        };
        ReviewResult {
            schema: REVIEW_SCHEMA_VERSION,
            id: ingest.id,
            report_id: report.id.clone(),
            workspace: report.workspace.clone(),
            run_id: ingest.run_id,
            agent: ingest.agent,
            status,
            outcomes,
            uncovered,
            checks: trimmed_lines(self.checks),
            notes: self.notes.trim().to_string(),
            defects,
            created_at_ms: ingest.created_at_ms,
        }
    }
}

impl ReviewResultSubmission {
    /// Ingest a result whose report is **gone** — pruned by retention while
    /// the fixer was working, since binding and submitting are hours apart.
    ///
    /// Refusing here was the obvious behaviour and the wrong one: it threw
    /// away every per-finding outcome at the last step, after all the work,
    /// with no partial save. The report is what validates finding ids, so
    /// without it the result cannot be complete — but "cannot be validated"
    /// and "must be destroyed" are different things, and only the first is
    /// true. The outcomes are recorded as a draft that names why.
    pub fn into_orphan_artifact(
        self,
        workspace: String,
        report_id: String,
        ingest: ResultIngest,
    ) -> ReviewResult {
        let mut defects = vec![format!(
            "report {report_id} is no longer retained, so these outcomes could not be checked \
             against its findings"
        )];
        let mut outcomes = Vec::with_capacity(self.outcomes.len());
        let mut covered: Vec<String> = Vec::with_capacity(self.outcomes.len());
        for (index, input) in self.outcomes.into_iter().enumerate() {
            let finding_id = input.finding_id.trim().to_string();
            if finding_id.is_empty() {
                defects.push(format!("outcome {}: finding_id is empty", index + 1));
                continue;
            }
            if covered.contains(&finding_id) {
                defects.push(format!("outcome {finding_id}: reported twice"));
                continue;
            }
            let Some(disposition) = Disposition::parse(&input.disposition) else {
                defects.push(format!(
                    "outcome {finding_id}: disposition {:?} is not one of fixed / \
                     already_resolved / blocked / refuted",
                    input.disposition
                ));
                continue;
            };
            covered.push(finding_id.clone());
            outcomes.push(FindingOutcome {
                finding_id,
                disposition,
                evidence: input.evidence.trim().to_string(),
                commits: trimmed_lines(input.commits),
                checks: trimmed_lines(input.checks),
            });
        }
        ReviewResult {
            schema: REVIEW_SCHEMA_VERSION,
            id: ingest.id,
            report_id,
            workspace,
            run_id: ingest.run_id,
            agent: ingest.agent,
            status: ArtifactStatus::Draft,
            outcomes,
            uncovered: Vec::new(),
            checks: trimmed_lines(self.checks),
            notes: self.notes.trim().to_string(),
            defects,
            created_at_ms: ingest.created_at_ms,
        }
    }
}

/// Trim each entry and drop the blanks — YAML and JSON both make it easy to
/// submit `["", " "]` for "none".
fn trimmed_lines(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .filter_map(|line| {
            let trimmed = line.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ingest(id: &str, at: i64) -> ReviewIngest {
        ReviewIngest {
            id: id.to_string(),
            workspace: "gh:acme/repo#7".to_string(),
            repo: Some("acme/repo".to_string()),
            run_id: "run-1".to_string(),
            agent: Some("claude".to_string()),
            origin: ReviewOrigin::Submitted,
            created_at_ms: at,
        }
    }

    fn good_finding() -> FindingInput {
        FindingInput {
            id: None,
            title: "drops the error".to_string(),
            severity: "blocker".to_string(),
            anchors: vec!["crates/server/src/poll.rs:88".to_string()],
            evidence: "a 500 from the provider returns Ok(vec![]), so the row is archived"
                .to_string(),
            remediation: "propagate the error".to_string(),
            checks: vec!["cargo test -p lazybox-server".to_string()],
        }
    }

    fn submission(findings: Vec<FindingInput>) -> ReviewSubmission {
        ReviewSubmission {
            report: "## Findings\n1. drops the error".to_string(),
            findings,
            scope: ReviewScope {
                label: "diff vs main".to_string(),
                base_sha: Some("aaa".to_string()),
                head_sha: Some("bbb".to_string()),
                dirty_digest: None,
            },
            checks: vec!["make test".to_string()],
            open_questions: vec![],
        }
    }

    fn completed(id: &str, at: i64, scope: ReviewScope) -> ReviewArtifact {
        let mut sub = submission(vec![good_finding()]);
        sub.scope = scope;
        sub.into_artifact(ingest(id, at))
    }

    #[test]
    fn severity_parses_the_words_reviews_actually_write() {
        assert_eq!(Severity::parse("Blocker"), Some(Severity::Blocker));
        assert_eq!(Severity::parse(" high "), Some(Severity::Blocker));
        assert_eq!(Severity::parse("major"), Some(Severity::Major));
        assert_eq!(Severity::parse("nit"), Some(Severity::Minor));
        // Refused, never silently demoted to a nit.
        assert_eq!(Severity::parse("spicy"), None);
        assert_eq!(Severity::parse(""), None);
    }

    #[test]
    fn severity_orders_worst_first() {
        let mut all = vec![Severity::Minor, Severity::Blocker, Severity::Major];
        all.sort();
        assert_eq!(
            all,
            vec![Severity::Blocker, Severity::Major, Severity::Minor]
        );
    }

    #[test]
    fn anchor_parses_file_and_line() {
        let anchor = FileAnchor::parse("src/main.rs:42").expect("anchor");
        assert_eq!(anchor.file, "src/main.rs");
        assert_eq!(anchor.line, Some(42));
        assert_eq!(anchor.render(), "src/main.rs:42");
    }

    #[test]
    fn anchor_keeps_a_non_numeric_tail_in_the_path() {
        let anchor = FileAnchor::parse("weird:name.rs").expect("anchor");
        assert_eq!(anchor.file, "weird:name.rs");
        assert_eq!(anchor.line, None);
        assert_eq!(FileAnchor::parse("   "), None);
    }

    #[test]
    fn a_well_formed_submission_completes_and_numbers_its_findings() {
        let artifact =
            submission(vec![good_finding(), good_finding()]).into_artifact(ingest("r1", 10));
        assert_eq!(artifact.status, ArtifactStatus::Completed);
        assert!(artifact.defects.is_empty(), "{:?}", artifact.defects);
        assert!(artifact.is_bindable());
        assert_eq!(
            artifact
                .findings
                .iter()
                .map(|f| f.id.as_str())
                .collect::<Vec<_>>(),
            vec!["f1", "f2"]
        );
        assert_eq!(artifact.schema, REVIEW_SCHEMA_VERSION);
        assert_eq!(artifact.findings[0].anchors[0].line, Some(88));
    }

    #[test]
    fn a_zero_finding_review_is_completed_not_missing() {
        let artifact = submission(vec![]).into_artifact(ingest("r1", 10));
        assert_eq!(artifact.status, ArtifactStatus::Completed);
        assert!(artifact.findings.is_empty());
        assert!(artifact.is_bindable());
        let scope = artifact.scope.clone();
        assert_eq!(
            select_report(std::slice::from_ref(&artifact), &scope),
            ReportSelection::Bound {
                id: "r1".to_string(),
                freshness: Freshness::Current,
            }
        );
    }

    #[test]
    fn an_anchorless_finding_keeps_the_report_as_a_draft() {
        let mut finding = good_finding();
        finding.anchors.clear();
        let artifact = submission(vec![finding]).into_artifact(ingest("r1", 10));
        assert_eq!(artifact.status, ArtifactStatus::Draft);
        assert!(!artifact.is_bindable());
        assert!(
            artifact.defects.iter().any(|d| d.contains("anchor")),
            "{:?}",
            artifact.defects
        );
        // The prose survives — the expensive half of the review is not thrown
        // away over a missing anchor.
        assert!(artifact.report.contains("drops the error"));
    }

    #[test]
    fn a_draft_is_never_bound() {
        let mut finding = good_finding();
        finding.evidence = "  ".to_string();
        let draft = submission(vec![finding]).into_artifact(ingest("r1", 10));
        let scope = draft.scope.clone();
        assert_eq!(
            select_report(std::slice::from_ref(&draft), &scope),
            ReportSelection::Missing
        );
    }

    #[test]
    fn an_unparseable_severity_is_a_defect_not_a_nit() {
        let mut finding = good_finding();
        finding.severity = "kinda bad".to_string();
        let artifact = submission(vec![finding]).into_artifact(ingest("r1", 10));
        assert_eq!(artifact.status, ArtifactStatus::Draft);
        assert!(artifact.findings.is_empty());
    }

    #[test]
    fn duplicate_finding_ids_are_refused() {
        let mut a = good_finding();
        a.id = Some("dup".to_string());
        let mut b = good_finding();
        b.id = Some("dup".to_string());
        let artifact = submission(vec![a, b]).into_artifact(ingest("r1", 10));
        assert_eq!(artifact.status, ArtifactStatus::Draft);
        assert_eq!(artifact.findings.len(), 1);
        assert!(artifact.defects.iter().any(|d| d.contains("duplicate")));
    }

    #[test]
    fn a_submitted_review_without_a_head_sha_is_a_draft() {
        let mut sub = submission(vec![good_finding()]);
        sub.scope.head_sha = None;
        let artifact = sub.into_artifact(ingest("r1", 10));
        assert_eq!(artifact.status, ArtifactStatus::Draft);
        assert!(artifact.defects.iter().any(|d| d.contains("head_sha")));
    }

    #[test]
    fn an_import_needs_no_head_sha_and_binds_as_unknown() {
        let mut sub = submission(vec![good_finding()]);
        sub.scope.head_sha = None;
        let mut ing = ingest("r1", 10);
        ing.origin = ReviewOrigin::Imported;
        let artifact = sub.into_artifact(ing);
        assert_eq!(artifact.status, ArtifactStatus::Completed);
        let current = ReviewScope {
            head_sha: Some("bbb".to_string()),
            ..artifact.scope.clone()
        };
        assert_eq!(artifact.freshness(&current), Freshness::Unknown);
        assert!(artifact.freshness(&current).requires_revalidation());
    }

    #[test]
    fn freshness_separates_a_moved_head_from_a_changed_worktree() {
        let scope = ReviewScope {
            label: "diff vs main".to_string(),
            base_sha: Some("aaa".to_string()),
            head_sha: Some("bbb".to_string()),
            dirty_digest: Some("d1".to_string()),
        };
        let artifact = completed("r1", 10, scope.clone());
        assert_eq!(artifact.freshness(&scope), Freshness::Current);
        assert!(!artifact.freshness(&scope).requires_revalidation());

        let moved = ReviewScope {
            head_sha: Some("ccc".to_string()),
            ..scope.clone()
        };
        assert_eq!(
            artifact.freshness(&moved),
            Freshness::HeadMoved {
                reviewed: "bbb".to_string(),
                current: "ccc".to_string(),
            }
        );

        let redirtied = ReviewScope {
            dirty_digest: Some("d2".to_string()),
            ..scope.clone()
        };
        assert_eq!(artifact.freshness(&redirtied), Freshness::WorktreeChanged);
        let cleaned = ReviewScope {
            dirty_digest: None,
            ..scope
        };
        assert_eq!(artifact.freshness(&cleaned), Freshness::WorktreeChanged);
    }

    #[test]
    fn selection_is_missing_with_nothing_bindable() {
        assert_eq!(
            select_report(&[], &ReviewScope::default()),
            ReportSelection::Missing
        );
    }

    #[test]
    fn selection_prefers_the_current_tree_over_a_newer_stale_report() {
        let current_scope = ReviewScope {
            label: "diff vs main".to_string(),
            base_sha: Some("aaa".to_string()),
            head_sha: Some("bbb".to_string()),
            dirty_digest: None,
        };
        let matching = completed("r1", 10, current_scope.clone());
        let newer_but_stale = completed(
            "r2",
            99,
            ReviewScope {
                head_sha: Some("zzz".to_string()),
                ..current_scope.clone()
            },
        );
        assert_eq!(
            select_report(&[matching, newer_but_stale], &current_scope),
            ReportSelection::Bound {
                id: "r1".to_string(),
                freshness: Freshness::Current,
            }
        );
    }

    #[test]
    fn selection_binds_the_newest_of_several_reports_on_one_scope() {
        let scope = ReviewScope {
            label: "diff vs main".to_string(),
            head_sha: Some("bbb".to_string()),
            ..ReviewScope::default()
        };
        let old = completed("r1", 10, scope.clone());
        let new = completed("r2", 20, scope.clone());
        assert_eq!(
            select_report(&[old, new], &scope),
            ReportSelection::Bound {
                id: "r2".to_string(),
                freshness: Freshness::Current,
            }
        );
    }

    #[test]
    fn a_tie_on_timestamp_binds_the_report_written_second() {
        let scope = ReviewScope {
            label: "diff vs main".to_string(),
            head_sha: Some("bbb".to_string()),
            ..ReviewScope::default()
        };
        let first = completed("r1", 10, scope.clone());
        let second = completed("r2", 10, scope.clone());
        assert_eq!(
            select_report(&[first, second], &scope),
            ReportSelection::Bound {
                id: "r2".to_string(),
                freshness: Freshness::Current,
            }
        );
    }

    #[test]
    fn selection_is_ambiguous_when_scopes_describe_different_work() {
        let head = Some("bbb".to_string());
        let diff = completed(
            "r1",
            10,
            ReviewScope {
                label: "diff vs main".to_string(),
                head_sha: head.clone(),
                ..ReviewScope::default()
            },
        );
        let pr = completed(
            "r2",
            20,
            ReviewScope {
                label: "PR #1732 head".to_string(),
                head_sha: head.clone(),
                ..ReviewScope::default()
            },
        );
        let current = ReviewScope {
            head_sha: head,
            ..ReviewScope::default()
        };
        assert_eq!(
            select_report(&[diff, pr], &current),
            ReportSelection::Ambiguous {
                candidates: vec!["r2".to_string(), "r1".to_string()],
            }
        );
    }

    /// Ambiguity was keyed on a label that defaults to empty, so two
    /// same-head reviews of different work — both unlabelled — bound the
    /// newest silently. Unprovable is not the same as identical.
    #[test]
    fn two_unlabelled_reports_at_one_head_are_ambiguous_not_silently_bound() {
        let head = Some("bbb".to_string());
        let bare = || ReviewScope {
            label: String::new(),
            head_sha: head.clone(),
            ..ReviewScope::default()
        };
        let crate_only = completed("r1", 10, bare());
        let whole_diff = completed("r2", 20, bare());
        let current = ReviewScope {
            head_sha: head,
            ..ReviewScope::default()
        };
        assert_eq!(
            select_report(&[crate_only, whole_diff], &current),
            ReportSelection::Ambiguous {
                candidates: vec!["r2".to_string(), "r1".to_string()],
            }
        );
    }

    /// One unlabelled report is not ambiguous with itself — the guard must
    /// not turn the ordinary single-review case into a question.
    #[test]
    fn a_single_unlabelled_report_still_binds() {
        let scope = ReviewScope {
            label: String::new(),
            head_sha: Some("bbb".to_string()),
            ..ReviewScope::default()
        };
        let only = completed("r1", 10, scope.clone());
        assert_eq!(
            select_report(&[only], &scope),
            ReportSelection::Bound {
                id: "r1".to_string(),
                freshness: Freshness::Current,
            }
        );
    }

    #[test]
    fn a_stale_report_still_binds_but_demands_revalidation() {
        let scope = ReviewScope {
            label: "diff vs main".to_string(),
            head_sha: Some("bbb".to_string()),
            ..ReviewScope::default()
        };
        let artifact = completed("r1", 10, scope);
        let current = ReviewScope {
            label: "diff vs main".to_string(),
            head_sha: Some("ccc".to_string()),
            ..ReviewScope::default()
        };
        let selection = select_report(&[artifact], &current);
        let ReportSelection::Bound { id, freshness } = selection else {
            panic!("expected a bound stale report, got {selection:?}");
        };
        assert_eq!(id, "r1");
        assert!(freshness.requires_revalidation());
    }

    #[test]
    fn a_report_from_another_schema_is_not_bindable() {
        let mut artifact = completed(
            "r1",
            10,
            ReviewScope {
                head_sha: Some("bbb".to_string()),
                ..ReviewScope::default()
            },
        );
        artifact.schema = REVIEW_SCHEMA_VERSION + 1;
        assert!(!artifact.is_bindable());
        let scope = artifact.scope.clone();
        assert_eq!(select_report(&[artifact], &scope), ReportSelection::Missing);
    }

    #[test]
    fn artifacts_round_trip_through_json() {
        let artifact = completed(
            "r1",
            10,
            ReviewScope {
                label: "diff vs main".to_string(),
                head_sha: Some("bbb".to_string()),
                ..ReviewScope::default()
            },
        );
        let json = serde_json::to_string(&artifact).expect("encode");
        let back: ReviewArtifact = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, artifact);
    }

    fn outcome(finding_id: &str, disposition: &str) -> OutcomeInput {
        OutcomeInput {
            finding_id: finding_id.to_string(),
            disposition: disposition.to_string(),
            evidence: "rewrote the error path to propagate".to_string(),
            commits: vec!["abc1234".to_string()],
            checks: vec!["cargo test -p lazybox-server".to_string()],
        }
    }

    fn result_ingest() -> ResultIngest {
        ResultIngest {
            id: "x1".to_string(),
            run_id: "run-2".to_string(),
            agent: Some("codex".to_string()),
            created_at_ms: 50,
        }
    }

    #[test]
    fn disposition_parses_the_four_words_and_nothing_else() {
        assert_eq!(Disposition::parse("fixed"), Some(Disposition::Fixed));
        assert_eq!(
            Disposition::parse("already-resolved"),
            Some(Disposition::AlreadyResolved)
        );
        assert_eq!(
            Disposition::parse("Already Resolved"),
            Some(Disposition::AlreadyResolved)
        );
        assert_eq!(Disposition::parse("blocked"), Some(Disposition::Blocked));
        assert_eq!(Disposition::parse("refuted"), Some(Disposition::Refuted));
        assert_eq!(Disposition::parse("wontfix"), None);
    }

    #[test]
    fn a_complete_result_covers_every_finding() {
        let report =
            submission(vec![good_finding(), good_finding()]).into_artifact(ingest("r1", 10));
        let result = ReviewResultSubmission {
            report_id: "r1".to_string(),
            outcomes: vec![outcome("f1", "fixed"), outcome("f2", "refuted")],
            checks: vec!["make test".to_string()],
            notes: "green".to_string(),
        }
        .into_artifact(&report, result_ingest());
        assert_eq!(result.status, ArtifactStatus::Completed);
        assert!(result.uncovered.is_empty());
        assert_eq!(result.report_id, "r1");
        assert_eq!(result.workspace, report.workspace);
        assert_eq!(result.outcomes[1].disposition, Disposition::Refuted);
    }

    #[test]
    fn a_result_that_skips_a_finding_is_a_draft_naming_it() {
        let report =
            submission(vec![good_finding(), good_finding()]).into_artifact(ingest("r1", 10));
        let result = ReviewResultSubmission {
            report_id: "r1".to_string(),
            outcomes: vec![outcome("f1", "fixed")],
            ..Default::default()
        }
        .into_artifact(&report, result_ingest());
        assert_eq!(result.status, ArtifactStatus::Draft);
        assert_eq!(result.uncovered, vec!["f2".to_string()]);
        assert!(result.defects.iter().any(|d| d.contains("f2")));
    }

    #[test]
    fn an_outcome_for_an_unknown_finding_is_refused() {
        let report = submission(vec![good_finding()]).into_artifact(ingest("r1", 10));
        let result = ReviewResultSubmission {
            report_id: "r1".to_string(),
            outcomes: vec![outcome("f1", "fixed"), outcome("f9", "fixed")],
            ..Default::default()
        }
        .into_artifact(&report, result_ingest());
        assert_eq!(result.status, ArtifactStatus::Draft);
        assert_eq!(result.outcomes.len(), 1);
        assert!(result.defects.iter().any(|d| d.contains("f9")));
    }

    #[test]
    fn a_refuted_finding_without_evidence_is_a_draft() {
        let report = submission(vec![good_finding()]).into_artifact(ingest("r1", 10));
        let mut refuted = outcome("f1", "refuted");
        refuted.evidence = " ".to_string();
        let result = ReviewResultSubmission {
            report_id: "r1".to_string(),
            outcomes: vec![refuted],
            ..Default::default()
        }
        .into_artifact(&report, result_ingest());
        assert_eq!(result.status, ArtifactStatus::Draft);
        assert!(result.defects.iter().any(|d| d.contains("evidence")));
    }

    #[test]
    fn a_result_does_not_mutate_the_report_it_answers() {
        let report = submission(vec![good_finding()]).into_artifact(ingest("r1", 10));
        let before = report.clone();
        let _ = ReviewResultSubmission {
            report_id: "r1".to_string(),
            outcomes: vec![outcome("f1", "fixed")],
            ..Default::default()
        }
        .into_artifact(&report, result_ingest());
        assert_eq!(report, before);
    }

    #[test]
    fn a_zero_finding_report_accepts_an_empty_result() {
        let report = submission(vec![]).into_artifact(ingest("r1", 10));
        let result = ReviewResultSubmission {
            report_id: "r1".to_string(),
            ..Default::default()
        }
        .into_artifact(&report, result_ingest());
        assert_eq!(result.status, ArtifactStatus::Completed);
        assert!(result.outcomes.is_empty());
    }

    /// Retention can delete the report a fixer bound hours earlier. Refusing
    /// the result destroyed every outcome at the last step; it is recorded as
    /// a draft naming the reason instead.
    #[test]
    fn a_result_whose_report_was_pruned_is_kept_not_discarded() {
        let result = ReviewResultSubmission {
            report_id: "r5".to_string(),
            outcomes: vec![outcome("f1", "fixed"), outcome("f2", "refuted")],
            checks: vec!["make test".to_string()],
            notes: "green".to_string(),
        }
        .into_orphan_artifact(
            "gh:acme/repo#7".to_string(),
            "r5".to_string(),
            result_ingest(),
        );
        assert_eq!(result.status, ArtifactStatus::Draft);
        assert_eq!(result.outcomes.len(), 2, "the fixer's work survives");
        assert_eq!(result.report_id, "r5");
        assert_eq!(result.checks, vec!["make test".to_string()]);
        assert!(
            result
                .defects
                .iter()
                .any(|d| d.contains("no longer retained")),
            "{:?}",
            result.defects
        );
    }

    /// A result against a draft means the fixer bound something no selection
    /// would have handed it. Record it, but say so.
    #[test]
    fn a_result_against_an_unbindable_report_is_flagged() {
        let mut finding = good_finding();
        finding.anchors.clear();
        let draft = submission(vec![finding]).into_artifact(ingest("r1", 10));
        assert!(!draft.is_bindable());
        let result = ReviewResultSubmission {
            report_id: "r1".to_string(),
            ..Default::default()
        }
        .into_artifact(&draft, result_ingest());
        assert_eq!(result.status, ArtifactStatus::Draft);
        assert!(
            result.defects.iter().any(|d| d.contains("not bindable")),
            "{:?}",
            result.defects
        );
    }

    #[test]
    fn an_anchor_keeps_no_dangling_separator() {
        let anchor = FileAnchor::parse("src/main.rs:").expect("anchor");
        assert_eq!(anchor.file, "src/main.rs");
        assert_eq!(anchor.line, None);
        assert_eq!(anchor.render(), "src/main.rs");
        // A colon inside a real name is still not a separator.
        assert_eq!(
            FileAnchor::parse("weird:name.rs").expect("anchor").file,
            "weird:name.rs"
        );
        // Nothing but separators anchors nothing.
        assert_eq!(FileAnchor::parse(":::"), None);
    }

    #[test]
    fn results_round_trip_through_json() {
        let report = submission(vec![good_finding()]).into_artifact(ingest("r1", 10));
        let result = ReviewResultSubmission {
            report_id: "r1".to_string(),
            outcomes: vec![outcome("f1", "fixed")],
            ..Default::default()
        }
        .into_artifact(&report, result_ingest());
        let json = serde_json::to_string(&result).expect("encode");
        let back: ReviewResult = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, result);
    }
}
