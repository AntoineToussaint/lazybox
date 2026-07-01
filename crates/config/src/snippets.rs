//! Snippet system: short keystroke shortcuts that expand into
//! pre-defined prompts and are auto-submitted to the active agent.
//!
//! Two files contribute, merged with the repo-local one winning on
//! key conflict:
//!
//! - **Global** — `<lazybox_home>/snippets.yaml` (defaults to
//!   `~/.lazybox/snippets.yaml`). Lives at the profile root so a
//!   schema bump in `v2/` doesn't orphan the user's library.
//! - **Repo-local** — `.lazybox/snippets.yaml` at the repository root.
//!   Checked into source control so a project can ship its own
//!   review prompts, deploy checks, etc.
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
    /// Snippet body. Sent verbatim to the active agent's PTY,
    /// followed by a single carriage-return that triggers submit
    /// on every agent we ship (Claude Code, Codex, Cursor, shell).
    pub body: String,
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
                (k, s)
            })
            .collect();
        Ok(Self { by_key })
    }

    /// Snippets shipped with lazybox. Merged *beneath* the user's
    /// global + repo files (see [`Snippets::load_merged`]), so any
    /// user entry with the same key transparently overrides one of
    /// these — they're a starting library, not a locked-in set.
    pub fn builtin() -> Self {
        let entry = |category: &str, description: &str, body: &str| Snippet {
            description: description.to_string(),
            category: category.to_string(),
            body: body.to_string(),
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
                    "Rebase onto the base branch",
                    "Rebase this branch onto the latest base branch. Fetch first \
                     (`git fetch`), replay cleanly, and resolve any conflicts \
                     conservatively — preserve the intent of both sides, never drop a \
                     change just to clear markers, and flag anything ambiguous for me. \
                     When done, confirm the branch still builds and tests pass.",
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
         \x20     Please review the current diff for correctness bugs and\n\
         \x20     obvious cleanups. Focus on the changes only.\n"
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

    /// Merge two snippet sets. Entries in `overlay` win on key
    /// conflict. Used to stack repo-local on top of global so a
    /// project can override a shared shortcut without touching the
    /// user's library.
    pub fn merged(base: Self, overlay: Self) -> Self {
        let mut by_key = base.by_key;
        by_key.extend(overlay.by_key);
        Self { by_key }
    }

    /// Load both global + repo files and merge them over the
    /// built-in set. Precedence, lowest to highest: built-in →
    /// global → repo. This is the one-shot entry point most callers
    /// want.
    pub fn load_merged(repo_root: Option<&Path>) -> Self {
        let global = Self::load_global();
        let repo = repo_root.map(Self::load_repo).unwrap_or_default();
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
        for key in ["rev", "deepreview", "sec", "selfrev"] {
            let body = &b.get(key).expect("flagship snippet ships built-in").body;
            assert!(
                body.contains("file:line"),
                "built-in `{key}` should ask for `file:line`-anchored findings",
            );
        }
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
    /// gaps. Mirrors how `load_merged` layers built-in < global.
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
}
