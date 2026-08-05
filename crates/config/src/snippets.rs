//! Snippet system: short keystroke shortcuts that expand into
//! pre-defined prompts and are auto-submitted to the active agent.
//!
//! Two files contribute, merged with the launch-directory one winning on
//! key conflict:
//!
//! - **Global** — `<lazybox_home>/snippets.yaml` (defaults to
//!   `~/.lazybox/snippets.yaml`). Lives at the profile root so a
//!   schema bump in `v2/` doesn't orphan the user's library.
//! - **Launch-directory** — `.lazybox/snippets.yaml` below the directory
//!   where this lazybox client was started. The resulting catalog is
//!   client-wide; it does not change with the selected workspace.
//!
//! Both files have the same shape:
//!
//! ```yaml
//! snippets:
//!   rev:
//!     description: Review current diff
//!     body: |
//!       Review the current diff for correctness bugs. Report findings
//!       ranked by severity, each with a file:line anchor. Look only at
//!       the changed lines; if it's clean, say so.
//!   pr:
//!     description: Open a PR with summary + test plan
//!     body: |
//!       Open a PR for the current branch with gh. Concise title; body
//!       with a Summary (1-3 bullets on why) and a Test plan checklist
//!       of what you verified. Print the PR URL when done.
//! ```
//!
//! The shipped built-in bodies ([`Snippets::builtin`]) follow one
//! deliberate house style: imperative and addressed to the agent, a
//! checkable deliverable stated up front, best-practice discipline
//! encoded inline (root-cause before fixing, no behavior change on a
//! refactor), and an escape hatch ("if it's clean, say so"). See
//! `docs/snippets.md` for the full house style.
//!
//! At runtime the TUI loads both and feeds the merged set into the
//! snippet picker mounted by the terminal pane on `]<key>`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Serialize `value` and write it to `path` atomically: a sibling
/// `.tmp` file, then a rename, so a crash mid-write can never leave a
/// half-written (and unparseable) snippets file behind. Creates the
/// parent directory if needed. Mirrors `Config::save_to`.
///
/// The tmp sibling carries a per-process, per-call suffix so a second
/// writer — another lazybox instance, or a concurrent call — can't
/// clash on a fixed name and clobber the other's in-flight write.
fn write_yaml_atomically(path: &Path, value: &serde_yaml::Value) -> Result<(), SnippetsError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let yaml = serde_yaml::to_string(value)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("yaml.tmp.{}.{seq}", std::process::id()));
    std::fs::write(&tmp, &yaml)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Single snippet definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snippet {
    /// Human-readable label shown in the picker. Short — one line.
    #[serde(default)]
    pub description: String,
    /// Optional grouping label ("Review", "Git & PR", …). Drives the
    /// category headers + colored tags in the picker. Defaults to
    /// empty; empty-category snippets fall under a trailing "Other"
    /// group. Free-form so user files can invent their own groups.
    #[serde(default)]
    pub category: String,
    /// Snippet body. Composer delivery removes a leading blank-line prefix
    /// and its padding; a body that begins directly with indented content
    /// preserves that indentation. Submission follows the target terminal's
    /// PTY protocol.
    pub body: String,
    /// Optional native agent skill this snippet dispatches. When set, the
    /// snippet is a *bridge* (#798): instead of pasting `body` verbatim, the
    /// delivered instruction explicitly tells the agent to invoke the named
    /// `SKILL.md` skill, with `body` supplying the task context. Everything
    /// else — picker, Recent, `]N`, broadcast — is unchanged, since the
    /// skill-invocation text flows through the same delivery path as a plain
    /// body (see [`Snippet::dispatch_body`]).
    ///
    /// Invariant: `Some` always names a real, trimmed, non-empty skill. A
    /// blank `skill:` in a user file is normalized to `None` at load
    /// ([`Snippet::normalize_skill`]), so consumers never have to guard
    /// against an empty dispatch target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// Provenance — which file the entry came from. Hand-set by the
    /// loader; serde ignores it on the way in / out. Used by the
    /// picker to show "global" vs "repo" hints alongside each row,
    /// and by the merge step to render conflict diagnostics.
    #[serde(skip)]
    pub origin: SnippetOrigin,
}

/// Where a snippet was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnippetOrigin {
    #[default]
    Unknown,
    /// Shipped with lazybox, merged beneath the user's files.
    BuiltIn,
    /// `<lazybox_home>/snippets.yaml`.
    Global,
    /// `<repo>/.lazybox/snippets.yaml`.
    Repo,
}

impl SnippetOrigin {
    /// Short label for the picker — `"built-in"` / `"global"` /
    /// `"repo"` / `""`.
    pub fn label(self) -> &'static str {
        match self {
            SnippetOrigin::BuiltIn => "built-in",
            SnippetOrigin::Global => "global",
            SnippetOrigin::Repo => "repo",
            SnippetOrigin::Unknown => "",
        }
    }
}

impl Snippet {
    /// Normalize `skill` to uphold its invariant: trim surrounding
    /// whitespace, and collapse a blank value to `None`. A user file with
    /// `skill: ""` (or `skill: "  "`) names no real dispatch target, so it
    /// must not survive as `Some("")` and produce an empty `` Use the ``
    /// `` `` skill.`` invocation. Applied at every load boundary.
    pub fn normalize_skill(&mut self) {
        self.skill = self.skill.take().and_then(|name| {
            let trimmed = name.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        });
    }

    /// The text actually delivered to the focused agent.
    ///
    /// For a plain snippet this is just `body`. For a *skill-dispatching*
    /// snippet (`skill:` set, #798) it is an explicit instruction to invoke
    /// the named native skill, with `body` — when present — appended as the
    /// task context. The invocation is a self-contained sentence so it lands
    /// cleanly no matter how `body` is phrased (or if it is empty), and it
    /// flows through the same delivery path as any other body, so Recent,
    /// `]N`, and `Shift-B` broadcast track it unchanged.
    pub fn dispatch_body(&self) -> String {
        match &self.skill {
            None => self.body.clone(),
            Some(skill) => {
                let body = self.body.trim();
                if body.is_empty() {
                    format!("Use the `{skill}` skill.")
                } else {
                    format!("Use the `{skill}` skill to complete this task:\n\n{body}")
                }
            }
        }
    }
}

/// Top-level wire shape of a snippets file. The outer key in
/// `snippets:` is the shortcut key (`rev`, `pr`, …); the inner
/// fields populate [`Snippet`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct SnippetsFile {
    snippets: BTreeMap<String, Snippet>,
}

/// Loaded + merged snippet collection.
#[derive(Debug, Clone, Default)]
pub struct Snippets {
    /// Snippets keyed by shortcut. `BTreeMap` for deterministic
    /// iteration in the picker.
    by_key: BTreeMap<String, Snippet>,
}

#[derive(Debug, thiserror::Error)]
pub enum SnippetsError {
    #[error("failed to read snippets: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse snippets: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("{0} is not a snippets mapping — refusing to overwrite it")]
    NotAMapping(PathBuf),
}

impl Snippets {
    /// Load from a specific file. Missing file → empty collection
    /// (no error — snippet files are optional). Origin is stamped
    /// on every entry so the picker can show provenance.
    ///
    /// We match on `ErrorKind::NotFound` from the read itself
    /// rather than gating on a prior `path.exists()` check — that
    /// would race a file removal between the two calls and surface
    /// as a misleading `Io` error.
    pub fn load_from(path: &Path, origin: SnippetOrigin) -> Result<Self, SnippetsError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => return Err(e.into()),
        };
        let file: SnippetsFile = serde_yaml::from_str(&raw)?;
        let by_key = file
            .snippets
            .into_iter()
            .map(|(k, mut s)| {
                s.origin = origin;
                s.normalize_skill();
                (k, s)
            })
            .collect();
        Ok(Self { by_key })
    }

    /// Snippets shipped with lazybox. Merged *beneath* the user's
    /// global + launch-directory files (see
    /// [`Snippets::load_for_launch_dir`]), so any
    /// user entry with the same key transparently overrides one of
    /// these — they're a starting library, not a locked-in set.
    pub fn builtin() -> Self {
        let entry = |category: &str, description: &str, body: &str| Snippet {
            description: description.to_string(),
            category: category.to_string(),
            body: body.to_string(),
            skill: None,
            origin: SnippetOrigin::BuiltIn,
        };
        let by_key = BTreeMap::from([
            // ── Review ──────────────────────────────────────────────
            (
                "rev".to_string(),
                entry(
                    "Review",
                    "Review the current diff",
                    "Review the current diff (`git diff` against the base branch) for \
                     correctness bugs: logic errors, off-by-one mistakes, missing error \
                     handling, broken edge cases, and anything that wouldn't survive a \
                     careful review. Report findings as a list ranked by severity, each \
                     with a `file:line` anchor and a one-line explanation of what breaks \
                     and when. Look only at the changed lines and the code they directly \
                     touch, not the whole file. If the diff is clean, say so plainly \
                     rather than inventing nits.",
                ),
            ),
            (
                // Not `revdeep`: a built-in key must never be a strict
                // prefix of another (`rev`), or the `]]rev` exact-key
                // auto-submit stops firing (two keys share the prefix).
                "deepreview".to_string(),
                entry(
                    "Review",
                    "Deep review: design, edge cases, failure modes",
                    "Review the current diff deeply, past surface bugs. First evaluate \
                     the design: are the abstractions and boundaries right, is there a \
                     simpler shape, and does it fit the surrounding code? Then stress the \
                     change — enumerate the edge cases, error paths, and concurrency / \
                     partial-failure / bad-input scenarios that aren't handled. Report \
                     findings as a list ranked by severity with `file:line` anchors, \
                     separating \"will break\" from \"worth reconsidering\". If the design \
                     is sound, say so and name the one thing you'd still keep an eye on.",
                ),
            ),
            (
                "nit".to_string(),
                entry(
                    "Review",
                    "Nitpick pass: naming, comments, style",
                    "Do a nitpick pass over the current diff: naming, comment quality, \
                     dead code, inconsistent style, and anything that would slow a \
                     reviewer down. Keep every suggestion small, mechanical, and \
                     behavior-preserving, each with a `file:line` anchor. This is polish \
                     only — if you spot an actual correctness or design problem, flag it \
                     separately rather than burying it as a nit.",
                ),
            ),
            (
                "selfrev".to_string(),
                entry(
                    "Review",
                    "Self-review before pushing",
                    "Self-review this branch as a skeptical reviewer seeing it for the \
                     first time. Read the full diff against the base branch, then call \
                     out anything that isn't obviously correct, any missing or weak \
                     tests, any leftover debug code or stray TODOs, and anything you'd be \
                     asked to change in review. List concrete items with `file:line` \
                     anchors so I can fix them before pushing. If it's genuinely ready, \
                     say so.",
                ),
            ),
            (
                "audit".to_string(),
                entry(
                    "Review",
                    "Full pre-ship review: correctness, design, security, perf, tests",
                    "Give the current diff (`git diff` against the base branch) the full \
                     pre-ship review, one lens at a time. Correctness: logic errors, \
                     off-by-one, unhandled errors and edge cases. Design: are the \
                     boundaries and abstractions right, does it fit the surrounding code, \
                     is there a simpler shape. Security: untrusted input crossing a trust \
                     boundary, authz, injection, secret handling. Performance: \
                     allocations or I/O added to a hot path, N+1s, accidental \
                     quadratics. Tests: do they actually exercise the new behavior and \
                     its failure paths. For each finding, trace the real code path and \
                     give a concrete failure scenario — the input or state that produces \
                     the wrong result — with a `file:line` anchor, ranked by severity. If \
                     a lens is clean, say so in one line rather than padding the list.",
                ),
            ),
            (
                "arch".to_string(),
                entry(
                    "Review",
                    "Design & architecture review (staff-engineer lens)",
                    "Review this change the way a staff engineer would — past line-level \
                     bugs, at the design. Does the change belong in this layer, or is it \
                     in the wrong place? Are the module boundaries, data flow, and \
                     ownership right, or does it add coupling that will bite later? Is \
                     there a materially simpler shape that does the same job? What's the \
                     blast radius, and the rollback story if it's wrong? Judge it against \
                     the patterns already in this codebase, not an ideal in the abstract. \
                     End with a one-word verdict — ship, reshape, or rethink — and the \
                     two or three highest-leverage changes, each with a `file:line` \
                     anchor and the reasoning. If the design is sound, say so and name \
                     the one risk you'd keep watching.",
                ),
            ),
            (
                // Not `perfrev`: `perf` is already a built-in key, and a
                // key with `perf` as a strict prefix breaks `]]perf`'s
                // exact-key auto-submit (see `deepreview`).
                "hotpath".to_string(),
                entry(
                    "Review",
                    "Performance review of the diff",
                    "Review the current diff for performance regressions, not micro-nits. \
                     Walk the changed code on its hot path and look for allocations or \
                     clones added inside a loop, N+1 queries or repeated I/O, an \
                     accidental O(n²) from a nested scan, eager work that could be \
                     deferred, and blocking calls or locks added to a latency-sensitive \
                     path. For each, give the `file:line`, the input scale at which it \
                     starts to hurt, and the concrete cost — measured or estimated, not \
                     hand-waved — ranked by impact. Suggest a fix only where the win is \
                     real and the code stays readable. If the diff has no hot-path \
                     impact, say so plainly instead of inventing concerns.",
                ),
            ),
            (
                "fixall".to_string(),
                entry(
                    "Review",
                    "Apply the review findings you just produced",
                    "Take the findings from the review you just produced and work through \
                     them end to end. Go in severity order — highest first — and for each \
                     finding fix the *real cause* at the `file:line` it names, not the \
                     surface symptom. If a finding is wrong, already handled, or not worth \
                     acting on, skip it and say so in one line rather than forcing a change \
                     you don't believe in. Whenever a fix changes behavior, add or adjust \
                     the test that would have caught the original problem. When you're \
                     done, re-run the build, tests, and linter to confirm the tree is \
                     green, and don't drag unrelated edits into the diff. Commit the fixes \
                     with a clear message that says what was wrong and why the change is \
                     the real fix, staging only the files you touched. Finish with a short \
                     summary: what you fixed, what you deliberately left and why, and \
                     anything you noticed in the code that the review missed.",
                ),
            ),
            // ── Git & PR ────────────────────────────────────────────
            (
                "pr".to_string(),
                entry(
                    "Git & PR",
                    "Open a PR (summary + test plan)",
                    "Open a PR for the current branch with `gh`. Push first if the branch \
                     isn't up to date, then write a concise, specific title and a body \
                     with a `## Summary` section (1-3 bullets on *why*, not a diff recap) \
                     and a `## Test plan` checklist of what you actually verified. Base \
                     all of it on the real commits and diff, not a guess. Print the PR \
                     URL when it's open.",
                ),
            ),
            (
                "ready".to_string(),
                entry(
                    "Git & PR",
                    "Mark the PR ready for review",
                    "Mark the current pull request as ready for review with \
                     `gh pr ready`. First confirm it actually is ready: the diff is \
                     clean, tests and CI pass, and the description matches what changed. \
                     If anything's off, tell me instead of flipping it.",
                ),
            ),
            (
                "commit".to_string(),
                entry(
                    "Git & PR",
                    "Commit staged changes with a good message",
                    "Commit the staged changes only. Write an imperative subject \
                     (<=50 chars) and, unless the change is trivial, a body explaining \
                     the *why* rather than restating the *what*. Don't stage or commit \
                     unrelated edits — if the working tree has changes outside this \
                     change, leave them alone and say so.",
                ),
            ),
            (
                "amend".to_string(),
                entry(
                    "Git & PR",
                    "Amend the last commit",
                    "Fold the current working changes into the previous commit with \
                     `git commit --amend`, keeping the existing message unless it no \
                     longer describes the result. Only do this when the previous commit \
                     hasn't landed on a shared branch (an unmerged PR branch is fine) — \
                     flag it if that isn't the case rather than rewriting shared history.",
                ),
            ),
            (
                "rebase".to_string(),
                entry(
                    "Git & PR",
                    "Rebase onto the latest main",
                    "Rebase this branch onto the latest `main` (or whatever base it \
                     targets). Fetch just main first (`git fetch origin main`) — leaving \
                     your branch's own remote-tracking ref untouched so the \
                     `--force-with-lease` below still guards it — then rebase onto \
                     `origin/main`, replaying cleanly and resolving any conflicts \
                     conservatively: preserve the intent of both sides, never drop a \
                     change just to clear markers, and flag anything ambiguous for me. \
                     When it's done, confirm the branch still builds and tests pass, then \
                     push the rewritten history with `--force-with-lease` (never a bare \
                     `--force`).",
                ),
            ),
            (
                "sync".to_string(),
                entry(
                    "Git & PR",
                    "Update the branch from main (merge, no rewrite)",
                    "Bring this branch up to date with the latest `main` by *merging*, not \
                     rebasing — use this when the branch is already pushed or shared and \
                     you'd rather not rewrite history. Fetch main first \
                     (`git fetch origin main`), merge `origin/main` in, and resolve any \
                     conflicts by honoring the intent of both sides (never drop a change \
                     just to clear markers), flagging anything ambiguous for me. Confirm \
                     the branch builds and tests pass afterward. Since this doesn't \
                     rewrite history, a plain `git push` is enough — no force needed. If \
                     you'd rather keep a linear history and the branch is unshared, rebase \
                     onto `origin/main` instead of merging.",
                ),
            ),
            (
                "resume".to_string(),
                entry(
                    "Git & PR",
                    "Resume an in-progress rebase through conflicts",
                    "A rebase is in progress and has stopped on a conflict. For each \
                     conflicted file, work out what both the replayed commit and the \
                     upstream changes intended and keep a result that honors both — don't \
                     blindly take one side just to clear the markers. Stage the resolved \
                     files, run `git rebase --continue`, and repeat for each commit the \
                     rebase stops on until it finishes. If it reaches a state you can't \
                     safely resolve, run `git rebase --abort` and tell me rather than \
                     forcing a bad merge. When it completes, confirm the tree builds and \
                     tests pass, then push with `--force-with-lease`.",
                ),
            ),
            (
                "squash".to_string(),
                entry(
                    "Git & PR",
                    "Squash the branch into clean commits",
                    "Squash this branch's commits into a small number of logically \
                     coherent commits with clear messages. Group by concept, not by \
                     chronology, and don't collapse genuinely independent changes into \
                     one. The final tree must be identical to the current one — verify \
                     with `git diff` before and after.",
                ),
            ),
            (
                "conflicts".to_string(),
                entry(
                    "Git & PR",
                    "Resolve merge conflicts",
                    "Resolve the current merge conflicts. For each hunk, work out what \
                     both sides intended and keep a result that honors both, not just \
                     whichever is easier to paste. Explain per file which side you kept \
                     and why, then confirm the merged result actually builds and passes \
                     tests rather than just clearing the markers.",
                ),
            ),
            (
                "push".to_string(),
                entry(
                    "Git & PR",
                    "Push the branch (force-with-lease after a rewrite)",
                    "Push the current branch to its remote. If its history was rewritten — \
                     after a rebase, amend, or squash — push with `--force-with-lease` so \
                     you overwrite only your own commits and never clobber work someone \
                     else pushed; never use a bare `--force`, and don't run a fresh \
                     `git fetch` of this branch's own upstream right before the push or \
                     the lease check can't protect you. Otherwise a plain `git push` (add \
                     `-u` to set the upstream on the first push). If `--force-with-lease` \
                     is rejected because the remote moved, stop and tell me what changed \
                     rather than escalating to `--force`. Confirm the push landed and the \
                     branch is in sync when done.",
                ),
            ),
            // ── Testing ─────────────────────────────────────────────
            (
                "test".to_string(),
                entry(
                    "Testing",
                    "Run the test suite and fix failures",
                    "Run the project's test suite. If anything fails, fix the root cause \
                     in the code — not the test, and not by loosening an assertion — then \
                     re-run until green. Report what failed, why, and what you changed. \
                     If a test is genuinely wrong, say so explicitly and explain before \
                     you touch it.",
                ),
            ),
            (
                "tdd".to_string(),
                entry(
                    "Testing",
                    "TDD: failing test first, then implement",
                    "Work test-first. Write a failing test that captures the desired \
                     behavior, run it, and confirm it fails for the *right* reason (not a \
                     typo or missing import). Then implement the minimal change to make \
                     it pass, re-run to confirm green, and refactor with the test as your \
                     guard. Show me the test before the implementation.",
                ),
            ),
            (
                "cover".to_string(),
                entry(
                    "Testing",
                    "Add tests for uncovered branches",
                    "Find the important untested branches in the code I just touched — \
                     error paths, edge cases, and boundary conditions first, not just the \
                     happy path. Add focused tests that would actually fail if the \
                     behavior regressed, run them to confirm they pass, and tell me which \
                     branches you deliberately left uncovered and why.",
                ),
            ),
            (
                "repro".to_string(),
                entry(
                    "Testing",
                    "Write a failing test that reproduces the bug",
                    "Write a minimal automated test that reproduces the bug I'm about to \
                     describe. Trace the real code path first so the test exercises the \
                     actual failure, then confirm it fails on the current code for the \
                     same reason the bug occurs — that red test is the regression guard. \
                     Don't fix the bug yet; just prove it with a failing test and show me \
                     the failure output.",
                ),
            ),
            // ── Debugging ───────────────────────────────────────────
            (
                "bug".to_string(),
                entry(
                    "Debugging",
                    "Root-cause the failure",
                    "Investigate the failure I'm about to describe. Find the root cause \
                     before proposing any fix — trace the actual code path and confirm \
                     the mechanism, don't guess or pattern-match. Once you can explain \
                     exactly why it happens, write a failing regression test, then fix \
                     the underlying cause (never the symptom) and confirm the test goes \
                     green. Report the mechanism, the fix, and why it's the real cause.",
                ),
            ),
            (
                "bisect".to_string(),
                entry(
                    "Debugging",
                    "git bisect to find the offending commit",
                    "Use `git bisect` to find the commit that introduced the regression. \
                     Establish a known-good and known-bad revision, script the check as a \
                     one-liner where you can so the bisect runs automatically, and report \
                     the first bad commit with its diff and an explanation of how it \
                     caused the failure. Reset the bisect state when you're done.",
                ),
            ),
            (
                "trace".to_string(),
                entry(
                    "Debugging",
                    "Add logging to narrow it down",
                    "Add targeted logging around the suspect code path to narrow where \
                     behavior diverges from expectation — log the inputs, the branch \
                     taken, and the key values at each step, not everything. Run it, read \
                     what it reveals, and report where reality first differs from what \
                     you expected. Keep the instrumentation easy to remove once we've \
                     found it.",
                ),
            ),
            (
                "explain".to_string(),
                entry(
                    "Debugging",
                    "Explain this error / stack trace",
                    "Explain the error or stack trace I'm about to paste: what it \
                     actually means, the most likely cause given *this* codebase (trace \
                     it to the real line, don't speak in generalities), and the single \
                     concrete next step to confirm and fix it. If more than one cause is \
                     plausible, rank them and say how to tell them apart.",
                ),
            ),
            // ── Refactor ────────────────────────────────────────────
            (
                "refac".to_string(),
                entry(
                    "Refactor",
                    "Refactor for clarity, no behavior change",
                    "Refactor the code I point you at for clarity and simplicity with no \
                     behavior change. Keep the diff small and reviewable — one coherent \
                     transformation, not a rewrite. Prove behavior is unchanged by \
                     running the existing tests before and after; if coverage there is \
                     thin, add a characterization test first. Don't fix bugs or change \
                     APIs along the way — flag those separately instead.",
                ),
            ),
            (
                "rename".to_string(),
                entry(
                    "Refactor",
                    "Rename a symbol across the repo",
                    "Rename the symbol I specify consistently across the whole repo — \
                     code, tests, docs, and comments. Lean on the compiler or language \
                     tooling to catch call sites rather than a blind find-replace, verify \
                     nothing unrelated matched the same string, and confirm it still \
                     builds and tests pass. No behavior change beyond the rename.",
                ),
            ),
            (
                "extract".to_string(),
                entry(
                    "Refactor",
                    "Extract a function / module",
                    "Extract the logic I point you at into a well-named function or \
                     module with a clear signature and no hidden coupling to its old \
                     context. Update every call site, keep behavior identical, and \
                     confirm with the existing tests. Keep the diff reviewable — this is \
                     a move, not a rewrite; flag any behavior change you're tempted to \
                     make instead of quietly doing it.",
                ),
            ),
            (
                "dedupe".to_string(),
                entry(
                    "Refactor",
                    "Remove duplication",
                    "Unify the near-duplicate logic I point you at behind a single \
                     implementation — but only where it's genuinely the same concept, not \
                     coincidentally similar code that will diverge later. Preserve \
                     behavior exactly, update all call sites, and confirm with tests. If \
                     some copies differ in ways that matter, say so and leave them alone.",
                ),
            ),
            // ── Performance ─────────────────────────────────────────
            (
                "perf".to_string(),
                entry(
                    "Performance",
                    "Profile and optimize the hot path",
                    "Profile the hot path I describe and find where the time *actually* \
                     goes — measure, don't assume. Optimize the biggest win first, \
                     confirm the improvement with a before/after measurement, and stop \
                     when the gains stop mattering. Don't trade correctness or \
                     readability for micro-gains, and keep the existing tests green.",
                ),
            ),
            (
                "bench".to_string(),
                entry(
                    "Performance",
                    "Add a benchmark",
                    "Add a benchmark that captures the performance characteristic we care \
                     about here, using the project's existing benchmarking setup if there \
                     is one. Make it representative and repeatable, run it, and report the \
                     current baseline numbers so future changes can be measured against \
                     it.",
                ),
            ),
            (
                "alloc".to_string(),
                entry(
                    "Performance",
                    "Reduce allocations in the hot path",
                    "Find avoidable allocations and copies in the hot path I point you at \
                     — reuse buffers, borrow instead of clone, drop intermediate \
                     collections — and remove them only where a measurement shows it \
                     helps and the code stays readable. Confirm the win with a \
                     before/after benchmark and keep the tests green.",
                ),
            ),
            // ── Security ────────────────────────────────────────────
            (
                "sec".to_string(),
                entry(
                    "Security",
                    "Security review of the diff",
                    "Review the current diff for security issues: injection, missing \
                     authz/authn checks, unsafe deserialization, path traversal, secret \
                     handling, SSRF, and unchecked input crossing a trust boundary. For \
                     each finding give the `file:line`, the concrete exploit path, and \
                     the fix, ranked by exploitability. If the diff introduces no \
                     security-relevant change, say so rather than padding the list.",
                ),
            ),
            (
                "deps".to_string(),
                entry(
                    "Security",
                    "Audit and update dependencies",
                    "Audit the project's dependencies for known vulnerabilities and \
                     unmaintained packages using the ecosystem's audit tool. Propose safe \
                     upgrades, call out breaking changes from each changelog, and don't \
                     bump anything without checking what changed. Report findings by \
                     severity with the fixed version for each.",
                ),
            ),
            (
                // Not `secrets`: would make `sec` a strict prefix of it
                // and break `sec`'s exact-key auto-submit (see `deepreview`).
                "leaks".to_string(),
                entry(
                    "Security",
                    "Scan for leaked secrets",
                    "Scan the diff — and recent history if relevant — for accidentally \
                     committed secrets: API keys, tokens, private keys, passwords, \
                     connection strings. For anything found, flag it clearly with \
                     `file:line`, treat it as already compromised, and advise on rotation \
                     and scrubbing it from history. Don't echo the full secret value \
                     back.",
                ),
            ),
            // ── Docs ────────────────────────────────────────────────
            (
                "doc".to_string(),
                entry(
                    "Docs",
                    "Document public APIs",
                    "Document the public APIs I touched: what each does, its parameters \
                     and return, invariants and failure modes, and a short usage example \
                     where it earns its place. Match the surrounding doc style and \
                     tooling exactly. Skip the trivial and self-evident — document the \
                     *why*, not the obvious *what*.",
                ),
            ),
            (
                "readme".to_string(),
                entry(
                    "Docs",
                    "Update the README",
                    "Update the README to reflect the change I just made — usage, flags, \
                     examples, and anything now stale or wrong. Verify each command or \
                     example actually works rather than assuming, keep it accurate and \
                     concise, and don't rewrite sections that are still correct.",
                ),
            ),
            (
                "adr".to_string(),
                entry(
                    "Docs",
                    "Write an ADR for this decision",
                    "Write a short Architecture Decision Record for the decision we just \
                     made: the context and forces, the options considered with their \
                     trade-offs, the decision, and its consequences (good and bad). \
                     Follow any existing ADR format and numbering in the repo. Be honest \
                     about what we're giving up, not just why we're right.",
                ),
            ),
            // ── Chores ──────────────────────────────────────────────
            (
                "lint".to_string(),
                entry(
                    "Chores",
                    "Fix lint and formatting",
                    "Run the project's linter and formatter, then fix every warning and \
                     formatting issue in the code I touched — address the underlying \
                     cause, never suppress or `allow` it away without a clear reason. \
                     Re-run to confirm clean, and don't drag unrelated reformatting into \
                     the diff.",
                ),
            ),
            (
                "ci".to_string(),
                entry(
                    "Chores",
                    "Diagnose and fix failing CI",
                    "CI is failing. Pull the failing job's logs (`gh run view \
                     --log-failed`), find the real cause rather than the surface error, \
                     and fix it locally. Re-run the equivalent check here to confirm it \
                     passes before pushing, and report what was actually broken.",
                ),
            ),
            (
                "clean".to_string(),
                entry(
                    "Chores",
                    "Remove dead code and unused deps",
                    "Find and remove dead code, unused imports, and unused dependencies \
                     in the area I point you at. Before deleting each one, verify it's \
                     truly unreferenced — check for reflection, macros, feature gates, \
                     and dynamic dispatch that a static search misses. Confirm it still \
                     builds and tests pass, and keep the deletions in a reviewable diff.",
                ),
            ),
        ]);
        Self { by_key }
    }

    /// Commented starter file written by the "Edit snippets" settings
    /// action when no global file exists yet. Mirrors the built-in
    /// `rev` entry so a user has a working example to copy.
    pub fn starter_template() -> &'static str {
        "# Snippets — keystroke shortcuts that expand into a prompt and\n\
         # auto-submit to the focused agent. Trigger with `]]<key>` in a\n\
         # session terminal. See docs/snippets.md for the full reference.\n\
         #\n\
         # lazybox ships a broad, categorized built-in library (rev, pr,\n\
         # test, refac, …); anything you define here with the same key\n\
         # overrides the built-in one. `category:` is optional.\n\
         snippets:\n\
         \x20 rev:\n\
         \x20   description: Review current diff\n\
         \x20   category: Review\n\
         \x20   body: |\n\
         \x20     Review the current diff for correctness bugs — logic errors,\n\
         \x20     off-by-one mistakes, missing error handling, broken edge cases.\n\
         \x20     Report findings ranked by severity, each with a file:line anchor\n\
         \x20     and a one-line note on what breaks and when. Look only at the\n\
         \x20     changed lines; if it's clean, say so instead of inventing nits.\n"
    }

    /// Default global path: `<lazybox_home>/snippets.yaml`.
    pub fn default_global_path() -> PathBuf {
        lazybox_core::paths::home().join("snippets.yaml")
    }

    /// Default repo-local path: `<repo_root>/.lazybox/snippets.yaml`.
    pub fn default_repo_path(repo_root: &Path) -> PathBuf {
        repo_root.join(".lazybox").join("snippets.yaml")
    }

    /// Convenience: load global file from the standard path. Missing
    /// → empty, never errors on absence.
    pub fn load_global() -> Self {
        match Self::load_from(&Self::default_global_path(), SnippetOrigin::Global) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to load global snippets: {e}");
                Self::default()
            }
        }
    }

    /// Convenience: load repo-local file from `<repo_root>/.lazybox/
    /// snippets.yaml`. Missing → empty.
    pub fn load_repo(repo_root: &Path) -> Self {
        match Self::load_from(&Self::default_repo_path(repo_root), SnippetOrigin::Repo) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "failed to load repo snippets at {}: {e}",
                    repo_root.display()
                );
                Self::default()
            }
        }
    }

    /// Upsert one snippet into the global `<lazybox_home>/snippets.yaml`
    /// and return the path written. Missing file/dir are created; an
    /// existing `key` is replaced in place, every other entry (and the
    /// mapping's order) is preserved. The write round-trips through
    /// `serde_yaml::Value` rather than editing text, so YAML comments in
    /// the file are not preserved — the trade-off for letting lazybox
    /// own the mutation safely.
    ///
    /// Used by the Ask Lazybox help agent's `add_snippet` action (#353):
    /// the agent proposes a snippet, the TUI confirms it, and this
    /// applies it natively before hot-reloading the picker.
    pub fn upsert_global_snippet(key: &str, snippet: &Snippet) -> Result<PathBuf, SnippetsError> {
        Self::upsert_snippet_at(&Self::default_global_path(), key, snippet)
    }

    /// The path-parameterized core of [`Self::upsert_global_snippet`],
    /// split out so tests exercise the read-modify-write without
    /// touching the real `LAZYBOX_HOME`.
    fn upsert_snippet_at(
        path: &Path,
        key: &str,
        snippet: &Snippet,
    ) -> Result<PathBuf, SnippetsError> {
        let existing = match std::fs::read_to_string(path) {
            Ok(raw) => Some(raw),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let mut root = match existing.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => serde_yaml::from_str(raw)?,
            _ => serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        };
        // A file that parsed to a non-mapping (a bare scalar or list) is
        // not a snippets file we can safely extend — refuse rather than
        // clobber whatever the user actually has there.
        let Some(map) = root.as_mapping_mut() else {
            return Err(SnippetsError::NotAMapping(path.to_path_buf()));
        };
        let snippets = map
            .entry(serde_yaml::Value::from("snippets"))
            .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        // Same guard one level down: a `snippets:` that isn't a mapping
        // (e.g. `snippets: []`) can't take a keyed entry.
        let Some(snippets) = snippets.as_mapping_mut() else {
            return Err(SnippetsError::NotAMapping(path.to_path_buf()));
        };
        snippets.insert(serde_yaml::Value::from(key), serde_yaml::to_value(snippet)?);

        write_yaml_atomically(path, &root)?;
        Ok(path.to_path_buf())
    }

    /// Merge two snippet sets. Entries in `overlay` win on key
    /// conflict. Used to stack repo-local on top of global so a
    /// project can override a shared shortcut without touching the
    /// user's library.
    pub fn merged(base: Self, overlay: Self) -> Self {
        let mut by_key = base.by_key;
        by_key.extend(overlay.by_key);
        Self { by_key }
    }

    /// Build the client-wide catalog from the built-in and global layers,
    /// plus `.lazybox/snippets.yaml` below `launch_dir` when present.
    /// Precedence, lowest to highest: built-in → global → launch directory.
    /// Changing workspace does not select a different directory layer.
    pub fn load_for_launch_dir(launch_dir: Option<&Path>) -> Self {
        let global = Self::load_global();
        let repo = launch_dir.map(Self::load_repo).unwrap_or_default();
        Self::merged(Self::merged(Self::builtin(), global), repo)
    }

    /// Exact lookup by shortcut key.
    pub fn get(&self, key: &str) -> Option<&Snippet> {
        self.by_key.get(key)
    }

    /// All snippets, in key order. Lazy — no allocation. Caller
    /// decides whether to collect or stream.
    pub fn all(&self) -> impl Iterator<Item = (&str, &Snippet)> {
        self.by_key.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Helper: write a snippets YAML into a unique tmp dir and
    /// return the path. We don't pull in the `tempfile` crate just
    /// for tests — `std::env::temp_dir` + a unique name is enough.
    fn write_tmp(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lazybox-snippets-test-{}-{}",
            std::process::id(),
            name,
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snippets.yaml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn missing_file_yields_empty_collection() {
        let s = Snippets::load_from(
            &PathBuf::from("/nonexistent/snippets-does-not-exist.yaml"),
            SnippetOrigin::Global,
        )
        .expect("missing file is not an error");
        assert!(s.is_empty());
    }

    #[test]
    fn loads_basic_snippets() {
        let path = write_tmp(
            "basic",
            r#"
snippets:
  rev:
    description: Review current diff
    body: |
      Please review the current diff.
  pr:
    description: Open a PR
    body: Please open a PR for the current branch.
"#,
        );
        let s = Snippets::load_from(&path, SnippetOrigin::Global).unwrap();
        assert_eq!(s.len(), 2);
        let rev = s.get("rev").unwrap();
        assert_eq!(rev.description, "Review current diff");
        assert!(rev.body.contains("Please review"));
        assert_eq!(rev.origin, SnippetOrigin::Global);
    }

    /// Repo-local entries override globals on key conflict — this
    /// is the headline acceptance criterion.
    #[test]
    fn repo_overrides_global_on_key_conflict() {
        let global_path = write_tmp(
            "conflict-global",
            r#"
snippets:
  rev:
    description: Global review
    body: global body
  shared:
    description: Only in global
    body: global only
"#,
        );
        let repo_path = write_tmp(
            "conflict-repo",
            r#"
snippets:
  rev:
    description: Repo-specific review
    body: repo body
  deploy:
    description: Only in repo
    body: repo only
"#,
        );
        let global = Snippets::load_from(&global_path, SnippetOrigin::Global).unwrap();
        let repo = Snippets::load_from(&repo_path, SnippetOrigin::Repo).unwrap();
        let merged = Snippets::merged(global, repo);
        assert_eq!(merged.len(), 3);
        let rev = merged.get("rev").unwrap();
        assert_eq!(rev.description, "Repo-specific review");
        assert_eq!(rev.body, "repo body");
        assert_eq!(rev.origin, SnippetOrigin::Repo);
        assert_eq!(merged.get("deploy").unwrap().origin, SnippetOrigin::Repo);
        assert_eq!(merged.get("shared").unwrap().origin, SnippetOrigin::Global);
    }

    #[test]
    fn all_yields_entries_in_key_order() {
        let path = write_tmp(
            "all-order",
            r#"
snippets:
  zeta:
    description: last
    body: z
  alpha:
    description: first
    body: a
  mid:
    description: middle
    body: m
"#,
        );
        let s = Snippets::load_from(&path, SnippetOrigin::Global).unwrap();
        let keys: Vec<&str> = s.all().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["alpha", "mid", "zeta"]);
    }

    /// Lookup by exact key — the auto-submit path uses this.
    #[test]
    fn get_returns_exact_match() {
        let path = write_tmp(
            "get",
            r#"
snippets:
  rev:
    description: Review
    body: review body
"#,
        );
        let s = Snippets::load_from(&path, SnippetOrigin::Global).unwrap();
        assert!(s.get("rev").is_some());
        assert!(s.get("re").is_none());
        assert!(s.get("REV").is_none(), "keys are case-sensitive");
    }

    /// `category` parses from YAML and round-trips; a snippet that
    /// omits it defaults to the empty string (backwards compatible).
    #[test]
    fn category_parses_and_defaults_to_empty() {
        let path = write_tmp(
            "category",
            r#"
snippets:
  rev:
    description: Review
    category: Review
    body: review body
  bare:
    body: no category here
"#,
        );
        let s = Snippets::load_from(&path, SnippetOrigin::Global).unwrap();
        assert_eq!(s.get("rev").unwrap().category, "Review");
        assert_eq!(s.get("bare").unwrap().category, "");
    }

    /// The built-in library is large and every entry is categorized —
    /// the picker relies on that to group.
    #[test]
    fn builtin_is_large_and_categorized() {
        let b = Snippets::builtin();
        assert!(
            b.len() >= 30,
            "built-in library should be broad: {}",
            b.len()
        );
        assert!(
            b.all().all(|(_, s)| !s.category.is_empty()),
            "every built-in snippet carries a category",
        );
    }

    /// Built-in bodies encode real instructions, not one-line labels
    /// (#247). Every body clears a floor length so the set can't
    /// regress back to "please review the diff" one-liners.
    #[test]
    fn builtin_bodies_are_substantial() {
        for (key, s) in Snippets::builtin().all() {
            assert!(
                s.body.len() >= 150,
                "built-in `{key}` body is too thin ({} chars) — snippet bodies \
                 should encode real, structured instructions",
                s.body.len(),
            );
        }
    }

    /// The high-traffic review/security bodies ask for structured,
    /// checkable output — `file:line` anchors — per the house style.
    #[test]
    fn flagship_bodies_request_anchored_findings() {
        let b = Snippets::builtin();
        for key in [
            "rev",
            "deepreview",
            "sec",
            "selfrev",
            "audit",
            "arch",
            "hotpath",
        ] {
            let body = &b.get(key).expect("flagship snippet ships built-in").body;
            assert!(
                body.contains("file:line"),
                "built-in `{key}` should ask for `file:line`-anchored findings",
            );
        }
    }

    /// The dedicated review suite (#252) ships as distinct built-in
    /// lenses in the Review category — a comprehensive `audit`, a
    /// staff-engineer `arch` design pass, and a `hotpath` performance
    /// review — so a user can reach each without hand-writing the prompt.
    #[test]
    fn builtin_ships_review_suite() {
        let b = Snippets::builtin();
        for key in ["audit", "arch", "hotpath"] {
            let s = b
                .get(key)
                .unwrap_or_else(|| panic!("`{key}` ships built-in"));
            assert_eq!(s.category, "Review", "`{key}` is a Review-category lens");
            assert_eq!(s.origin, SnippetOrigin::BuiltIn);
        }
    }

    /// `fixall` is the apply step paired with the review lenses (#838):
    /// it turns a findings list into a clean, tested diff. It ships as a
    /// Review-category built-in with a real body.
    #[test]
    fn builtin_ships_fixall() {
        let b = Snippets::builtin();
        let s = b.get("fixall").expect("`fixall` ships built-in");
        assert_eq!(s.category, "Review");
        assert_eq!(s.origin, SnippetOrigin::BuiltIn);
        assert!(!s.body.is_empty());
        assert!(
            s.body.contains("file:line"),
            "`fixall` fixes findings at their `file:line` anchor",
        );
    }

    /// No built-in key may be a strict prefix of another. The picker's
    /// `]]rev`-style fast path auto-submits only when the typed key is
    /// the *sole* key with that prefix, so a colliding pair (e.g.
    /// `rev` + `revdeep`) would silently kill the shorter key's
    /// auto-submit — the headline acceptance criterion.
    #[test]
    fn no_builtin_key_is_a_prefix_of_another() {
        let b = Snippets::builtin();
        let keys: Vec<&str> = b.all().map(|(k, _)| k).collect();
        for &a in &keys {
            for &c in &keys {
                assert!(
                    a == c || !c.starts_with(a),
                    "built-in key `{a}` is a prefix of `{c}` — breaks `]]{a}` auto-submit",
                );
            }
        }
    }

    /// The Git & PR category covers the day-to-day rebase/update/push
    /// loop (#276): rebasing onto main, merging main in without a
    /// rewrite, continuing a rebase through conflicts, and pushing
    /// rewritten history safely. Every one is a Git & PR built-in.
    #[test]
    fn builtin_covers_rebase_and_push_workflow() {
        let b = Snippets::builtin();
        for key in ["rebase", "sync", "resume", "push", "conflicts"] {
            let s = b
                .get(key)
                .unwrap_or_else(|| panic!("`{key}` ships built-in"));
            assert_eq!(s.category, "Git & PR", "`{key}` is a Git & PR lens");
            assert_eq!(s.origin, SnippetOrigin::BuiltIn);
        }
        // `push` and `resume` teach the safe force-push after a rewrite.
        assert!(b.get("push").unwrap().body.contains("--force-with-lease"));
        assert!(
            b.get("resume")
                .unwrap()
                .body
                .contains("git rebase --continue")
        );
        // `sync` is the merge (no-rewrite) alternative to `rebase`.
        assert!(b.get("sync").unwrap().body.contains("merg"));
        assert!(b.get("rebase").unwrap().body.contains("--force-with-lease"));
        // A narrowed fetch keeps the branch's remote-tracking ref intact so
        // the post-rebase `--force-with-lease` still guards a teammate's push.
        assert!(
            b.get("rebase")
                .unwrap()
                .body
                .contains("git fetch origin main")
        );
    }

    #[test]
    fn missing_description_defaults_to_empty_string() {
        let path = write_tmp(
            "no-desc",
            r#"
snippets:
  bare:
    body: just a body
"#,
        );
        let s = Snippets::load_from(&path, SnippetOrigin::Global).unwrap();
        let entry = s.get("bare").unwrap();
        assert_eq!(entry.description, "");
        assert_eq!(entry.body, "just a body");
    }

    /// Empty `snippets:` block parses to zero entries instead of
    /// erroring. Mirrors how every other section in `Config`
    /// behaves when omitted.
    #[test]
    fn empty_snippets_block_parses() {
        let path = write_tmp(
            "empty-block",
            r#"
snippets: {}
"#,
        );
        let s = Snippets::load_from(&path, SnippetOrigin::Global).unwrap();
        assert!(s.is_empty());
    }

    /// The shipped built-in set is non-empty and carries the
    /// `ready` shortcut (the headline default).
    #[test]
    fn builtin_includes_ready_snippet() {
        let b = Snippets::builtin();
        assert!(!b.is_empty());
        let ready = b.get("ready").expect("ready snippet ships built-in");
        assert_eq!(ready.origin, SnippetOrigin::BuiltIn);
        assert!(ready.body.contains("gh pr ready"));
        assert!(b.get("rev").is_some());
        assert!(b.get("pr").is_some());
    }

    /// A global entry with a built-in key wins; built-ins fill the
    /// gaps. Mirrors how `load_for_launch_dir` layers built-in < global.
    #[test]
    fn user_entry_overrides_builtin_on_key_conflict() {
        let user = Snippets::load_from(
            &write_tmp(
                "override-builtin",
                r#"
snippets:
  rev:
    description: My review
    body: my custom review body
"#,
            ),
            SnippetOrigin::Global,
        )
        .unwrap();
        let merged = Snippets::merged(Snippets::builtin(), user);
        let rev = merged.get("rev").unwrap();
        assert_eq!(rev.body, "my custom review body");
        assert_eq!(rev.origin, SnippetOrigin::Global);
        // Untouched built-ins remain.
        assert_eq!(merged.get("ready").unwrap().origin, SnippetOrigin::BuiltIn);
    }

    #[test]
    fn load_for_launch_dir_builds_one_catalog_from_that_directory() {
        let launch_dir =
            std::env::temp_dir().join(format!("lazybox-snippets-launch-{}", std::process::id()));
        let snippets_dir = launch_dir.join(".lazybox");
        std::fs::create_dir_all(&snippets_dir).unwrap();
        std::fs::write(
            snippets_dir.join("snippets.yaml"),
            "snippets:\n  launch-contract:\n    body: from launch directory\n",
        )
        .unwrap();

        let catalog = Snippets::load_for_launch_dir(Some(&launch_dir));
        let snippet = catalog
            .get("launch-contract")
            .expect("launch-directory entry");
        assert_eq!(snippet.body, "from launch directory");
        assert_eq!(snippet.origin, SnippetOrigin::Repo);
    }

    /// The starter template parses as a valid snippets file.
    #[test]
    fn starter_template_is_valid_yaml() {
        let file: SnippetsFile =
            serde_yaml::from_str(Snippets::starter_template()).expect("template parses");
        assert!(file.snippets.contains_key("rev"));
    }

    /// Malformed YAML surfaces as `SnippetsError::Parse`, not a
    /// silent empty load — users will want to know they typo'd.
    #[test]
    fn malformed_yaml_surfaces_parse_error() {
        let path = write_tmp(
            "bad-yaml",
            r#"
snippets:
  rev:
    body: [this is not a valid mapping value
"#,
        );
        let err = Snippets::load_from(&path, SnippetOrigin::Global)
            .expect_err("malformed YAML should error");
        assert!(matches!(err, SnippetsError::Parse(_)));
    }

    fn snippet(category: &str, description: &str, body: &str) -> Snippet {
        Snippet {
            description: description.to_string(),
            category: category.to_string(),
            body: body.to_string(),
            skill: None,
            origin: SnippetOrigin::Unknown,
        }
    }

    /// Writing into a directory that doesn't exist yet creates the file
    /// (and its parent) and the snippet reads back through `load_from`.
    #[test]
    fn upsert_creates_missing_file() {
        let dir = std::env::temp_dir().join(format!(
            "lazybox-snippets-upsert-create-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("snippets.yaml");
        Snippets::upsert_snippet_at(
            &path,
            "integrate",
            &snippet("Review", "Integrate feedback", "Do the thing"),
        )
        .expect("write succeeds");

        let loaded = Snippets::load_from(&path, SnippetOrigin::Global).expect("reloads");
        let s = loaded.get("integrate").expect("key present");
        assert_eq!(s.category, "Review");
        assert_eq!(s.body, "Do the thing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Upserting a new key preserves every existing entry — the write
    /// is a merge, not a replace.
    #[test]
    fn upsert_preserves_existing_entries() {
        let path = write_tmp(
            "upsert-preserve",
            "snippets:\n  rev:\n    description: Review\n    body: review it\n",
        );
        Snippets::upsert_snippet_at(
            &path,
            "integrate",
            &snippet("Review", "Integrate", "integrate it"),
        )
        .expect("write succeeds");

        let loaded = Snippets::load_from(&path, SnippetOrigin::Global).expect("reloads");
        assert!(loaded.get("rev").is_some(), "existing snippet must survive");
        assert_eq!(
            loaded.get("integrate").expect("new key").body,
            "integrate it"
        );
    }

    /// Upserting an existing key replaces its value in place.
    #[test]
    fn upsert_replaces_existing_key() {
        let path = write_tmp(
            "upsert-replace",
            "snippets:\n  rev:\n    description: Old\n    body: old body\n",
        );
        Snippets::upsert_snippet_at(&path, "rev", &snippet("Review", "New", "new body"))
            .expect("write succeeds");

        let loaded = Snippets::load_from(&path, SnippetOrigin::Global).expect("reloads");
        assert_eq!(loaded.len(), 1, "replace, not append");
        assert_eq!(loaded.get("rev").expect("rev").body, "new body");
    }

    /// A file that exists but has no `snippets:` key still gets the new
    /// entry (and keeps whatever other top-level keys it had).
    #[test]
    fn upsert_adds_snippets_key_when_absent() {
        let path = write_tmp("upsert-no-key", "some_other_top_level: 1\n");
        Snippets::upsert_snippet_at(&path, "rev", &snippet("", "", "body"))
            .expect("write succeeds");

        let loaded = Snippets::load_from(&path, SnippetOrigin::Global).expect("reloads");
        assert!(loaded.get("rev").is_some());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("some_other_top_level"),
            "sibling key preserved"
        );
    }

    /// A valid-YAML-but-non-mapping file (e.g. a bare list) is refused,
    /// not silently overwritten — the user's content is never destroyed.
    #[test]
    fn upsert_refuses_to_clobber_a_non_mapping_file() {
        let path = write_tmp("upsert-non-mapping", "- one\n- two\n");
        let err = Snippets::upsert_snippet_at(&path, "rev", &snippet("", "", "body"))
            .expect_err("must refuse");
        assert!(matches!(err, SnippetsError::NotAMapping(_)));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("- one"), "original content intact");
    }

    /// A `snippets:` value that isn't a mapping is likewise refused
    /// rather than replaced.
    #[test]
    fn upsert_refuses_when_snippets_key_is_not_a_mapping() {
        let path = write_tmp("upsert-snippets-list", "snippets: []\n");
        let err = Snippets::upsert_snippet_at(&path, "rev", &snippet("", "", "body"))
            .expect_err("must refuse");
        assert!(matches!(err, SnippetsError::NotAMapping(_)));
    }

    /// The atomic write leaves no stray `.tmp` sibling behind — the
    /// rename consumes whatever unique tmp name the write picked.
    #[test]
    fn upsert_cleans_up_its_temp_file() {
        let path = write_tmp("upsert-tmp", "snippets:\n  rev:\n    body: x\n");
        Snippets::upsert_snippet_at(&path, "pr", &snippet("", "", "y")).expect("write");
        let dir = path.parent().expect("has parent");
        let leftovers: Vec<_> = std::fs::read_dir(dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "tmp siblings left behind: {leftovers:?}"
        );
    }

    /// A plain snippet dispatches its body verbatim — no `skill:` wrapping.
    #[test]
    fn plain_snippet_dispatch_body_is_the_raw_body() {
        let s = snippet("Review", "Review", "review the diff");
        assert_eq!(s.dispatch_body(), "review the diff");
    }

    /// A `skill:` snippet resolves to an explicit skill invocation with the
    /// body supplied as the task context (#798).
    #[test]
    fn skill_snippet_dispatch_body_invokes_the_skill_with_context() {
        let mut s = snippet("Review", "Deep review", "review the diff for correctness");
        s.skill = Some("code-review".into());
        assert_eq!(
            s.dispatch_body(),
            "Use the `code-review` skill to complete this task:\n\nreview the diff for correctness"
        );
    }

    /// A `skill:` snippet with an empty body still dispatches the skill —
    /// the invocation alone is the instruction.
    #[test]
    fn skill_snippet_with_empty_body_just_invokes_the_skill() {
        let mut s = snippet("Review", "Review", "   \n  ");
        s.skill = Some("code-review".into());
        assert_eq!(s.dispatch_body(), "Use the `code-review` skill.");
    }

    /// The `skill:` field parses from YAML and defaults to `None` when
    /// absent, so existing snippet files are unaffected.
    #[test]
    fn skill_field_parses_and_defaults_to_none() {
        let path = write_tmp(
            "skill-parse",
            "snippets:\n  \
             plain:\n    body: just text\n  \
             bridge:\n    skill: code-review\n    body: review the diff\n",
        );
        let loaded = Snippets::load_from(&path, SnippetOrigin::Global).expect("loads");
        assert_eq!(loaded.get("plain").expect("plain").skill, None);
        assert_eq!(
            loaded.get("bridge").expect("bridge").skill.as_deref(),
            Some("code-review")
        );
    }

    /// A blank `skill:` names no real dispatch target, so loading collapses
    /// it to `None` — the snippet stays a plain text snippet instead of
    /// delivering a degenerate empty-backtick invocation. Surrounding
    /// whitespace on a real name is trimmed. Verified through the delivered
    /// text (`dispatch_body`), the observable behavior.
    #[test]
    fn blank_skill_normalizes_to_none_and_names_are_trimmed() {
        let path = write_tmp(
            "skill-normalize",
            "snippets:\n  \
             empty:\n    skill: \"\"\n    body: just text\n  \
             spaces:\n    skill: \"   \"\n    body: more text\n  \
             padded:\n    skill: \"  code-review  \"\n    body: review the diff\n",
        );
        let loaded = Snippets::load_from(&path, SnippetOrigin::Global).expect("loads");

        let empty = loaded.get("empty").expect("empty");
        assert_eq!(empty.skill, None, "blank skill collapses to None");
        assert_eq!(
            empty.dispatch_body(),
            "just text",
            "delivered as plain text"
        );

        let spaces = loaded.get("spaces").expect("spaces");
        assert_eq!(
            spaces.skill, None,
            "whitespace-only skill collapses to None"
        );
        assert_eq!(spaces.dispatch_body(), "more text");

        let padded = loaded.get("padded").expect("padded");
        assert_eq!(padded.skill.as_deref(), Some("code-review"), "name trimmed");
        assert_eq!(
            padded.dispatch_body(),
            "Use the `code-review` skill to complete this task:\n\nreview the diff"
        );
    }
}
