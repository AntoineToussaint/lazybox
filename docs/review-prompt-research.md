# Toughening the review / fix / security snippets

Research notes and rationale behind the #935 rewrite of the built-in
review, fix, and security prompts in
[`crates/config/src/snippets.rs`](../crates/config/src/snippets.rs).

## Why this was needed

`fixall` and the review snippets were too soft. In practice they would
**skip the hard finding and over-justify the skip** instead of doing the
work — deferring to "prior behavior," "degrades safely," "not worth the
complexity," "not a change I believe in," "out of scope." It reads as
thoroughness; it is avoidance.

This is not cosmetic. One of those exact rationalizations —
*"degrades safely to prior behavior (delete), never data loss"* — is the
reasoning behind **#924**, the reconcile-sweep logic that force-deleted 11
worktrees and their agent sessions. A prompt that rewards "I carefully
decided not to act" manufactures precisely that failure: the
scope-cautious, "the safe default is fine" reflex is not harmless, it
ships bugs. #924 is the cautionary tale the rewritten prompts are built to
prevent.

## What best-in-class review prompts do

A cited pass over the strongest review/security/fix prompts in the wild.
Quotes are verbatim or close paraphrase from the primary sources listed at
the end.

### 1. Severity discipline — force a ranking, suppress nits

The strongest prompts name severity tiers explicitly and pair them with an
*asymmetric loss function* that biases toward silence over noise.

- Anthropic's `/security-review` prompt hard-codes tiers and a suppression
  rule: **"Focus on HIGH and MEDIUM findings only. Better to miss some
  theoretical issues than flood the report with false positives,"** plus an
  explicit *exclusion list* (DoS, rate-limiting, "If there isn't a proven
  problem… don't report it"). [1]
- The Claude Code code-review plugin: **"Flag only significant bugs; ignore
  nitpicks and likely false positives"** with a dedicated false-positive
  list including **"Pedantic nitpicks that a senior engineer would not
  flag."** [2]
- Cursor Bugbot focuses **"exclusively on catching real bugs … while
  intentionally ignoring formatting, style, and low-severity concerns,"**
  because **"the cost of a false positive (eroded trust, wasted time)
  vastly exceeds the cost of a false negative."** [3]

**Why it works:** a named tier list plus a "do-not-report" catalog turns a
fuzzy "be thorough" into a per-finding decision rule, and the stated loss
asymmetry tells the model which way to round when unsure.

### 2. Refute-don't-confirm — adversarial by construction

Treat each candidate finding as a hypothesis that must survive a disproof
attempt; treat the safe-looking default as the thing to disprove.

- The code-review plugin encodes a kill-gate: findings must be confirmed
  **"truly real with high confidence before inclusion,"** and **"If you are
  not certain an issue is real, do not flag it."** [2]
- Greptile v3 raises **"an increased threshold for 'sureness' … since v3 can
  challenge its own hypothesis more strongly,"** so lower-confidence
  comments can be eliminated for higher signal. [4]
- awesome-cursorrules' anti-sycophancy rule is adversarial toward
  *reassurance itself*: **"If the user pushes back on a technically sound
  recommendation, hold the position. Update only on new evidence,"** and
  authority claims ("legal said it's fine") are **"not technical
  justifications."** [5]

**Why it works:** the default LLM behavior is to rationalize a plausible
finding (or a plausible *skip*). Making disproof the job removes the
anchoring that manufactures confident hand-waving in both directions.

### 3. Concrete-repro requirement — a falsifiable input/state

Ban vague worries; demand a specific path from input to wrong result.

- Anthropic security-review: only flag issues you're **">80% confident of
  actual exploitability,"** every finding carries an `exploit_scenario`
  (a literal payload, e.g. `'1; DROP TABLE users--'`), and confidence
  <0.7 → **"Don't report (too speculative)."** [1]
- The code-review plugin flags only code that **"will definitely produce
  wrong results regardless of inputs (clear logic errors)"** and asks for a
  concrete trigger otherwise. [2]
- awesome-prompts' security reviewer requires a `RISK:` line stating
  **"what an attacker can achieve if this is exploited."** [6]

**Why it works:** requiring a concrete trigger makes the claim testable, so
the model self-filters anything it can't instantiate — and, symmetrically,
can't dismiss a finding without naming the input that makes it a non-issue.

### 4. "Do the fix" framing — implement, don't curate reasons to skip

- OpenAI's GPT-4.1 guide: the highest-impact lever is a *persistence*
  reminder — **"keep going until the user's query is completely resolved,
  before ending your turn"** — which made the model **"eager"** rather than
  chatbot-like and lifted their internal SWE-bench score. [7]
- Anthropic's prompt-engineering guide names the exact failure mode of soft
  fix prompts: **"If you say 'can you suggest some changes,' Claude will
  sometimes provide suggestions rather than implementing them … For Claude
  to take action, be more explicit."** [8]
- The code-review plugin makes fixes *complete*: **"Never post suggestions
  unless committing them fully resolves the issue."** [2]

**Why it works:** instruction-following models default to the lower-risk
"describe" action unless the imperative to *edit and finish* is explicit
and the yield condition is "problem solved," not "options presented."

### 5. Anti-hedging — no reassurance without evidence

- awesome-cursorrules: **"Never reply 'looks good' or 'this is correct'
  without by-eye verification against a spec or test execution."** [5]
- Anthropic security-review sets a calibration bar that kills hedged
  findings: **"Each finding should be something a security engineer would
  confidently raise in a PR review."** [1]
- awesome-cursorrules PR review: **"Be specific with evidence, not vague
  assertions,"** ending each angle in a hard verdict (safe to merge / needs
  changes / reject). [9]

**Why it works:** replacing hedge-friendly output slots with a forced
verdict plus a required evidence citation removes the linguistic room where
"may," "could," "out of scope," and "degrades safely" live.

## How the rewrite adopts these

| Pattern | Applied to our snippets |
| --- | --- |
| Adversarial / refute-don't-confirm (§2) | `rev`, `deepreview`, `audit`, `sec` now read *adversarially*: each finding is real until refuted, and a safe-looking default (early return, fallback, delete-on-missing) is a claim to **disprove**, not a resting place. `arch` rejects "it matches the existing pattern" / "it degrades safely" as defenses. |
| Falsifiable trigger (§3) | `rev`, `deepreview`, `audit`, `selfrev`, `hotpath` require the concrete input or state that produces the wrong result. `fixall` inverts it: a finding stays unfixed **only** with a specific, falsifiable reason a concrete input proves. |
| "Do the fix" (§4) | `fixall` now says the deliverable is a **clean, tested diff, not a curated list of reasons not to act** — "default to fixing," and "when unsure, fix it." `bug` isn't done without a red-then-green test. |
| Anti-hedging ban (§5) | `fixall` names and bans the weasel phrases — "not worth the complexity," "degrades safely to the prior behavior," "out of scope," "not a change I believe in" — as **"the reflex that ships bugs."** `hotpath` bans "probably negligible" without a number; `ci` bans "flake / unrelated" without a re-run; `test` bans "flaky / wrong test" without proof; `deps`/`leaks` ban "not exploitable" / "probably a fixture" without a traced reason. |
| #924 as the anti-pattern | `deepreview` and `fixall` bake the lesson in directly: *"it degrades safely to the old behavior" is the exact reasoning that ships silent data loss.* The prompts carry the principle, not the (repo-specific) issue number, so they read the same in any checkout. |

The rewrite is regression-guarded by
`review_and_fix_bodies_are_adversarial` and
`fixall_defaults_to_implementing_not_skipping` in `snippets.rs`, so the
toughening can't silently soften back to "degrades safely / not worth the
complexity."

## Deliberately deferred: external snippet library (design)

Issue #935 also asks for an **external snippet library** — pulling snippet
packs from a URL / git repo rather than only built-ins + local YAML. That
is a network-and-trust feature with a materially different blast radius
from a prompt rewrite (it fetches remote text that is then auto-submitted
to an agent with `git`/`gh` in a worktree), so per one-logical-change-per-PR
it is **not** implemented here. This is the design it should follow.

**Config surface** — a new list on the snippets config, lowest precedence,
below built-in:

```yaml
snippets:
  sources:
    - https://example.com/packs/review.yaml   # raw YAML over https
    - github:owner/repo//snippets.yaml@v1.2.0  # pinned git ref
```

**Load order** (extends today's built-in → global → launch dir): built-in →
**sources (in listed order)** → global → launch dir. External packs are a
*starting library* a user can still override locally, never an override of
the user's own files.

**Trust model** — the load-bearing constraint, because a snippet body is
executed as an agent instruction:

- **Pinned + cached.** A `github:` source resolves to an immutable commit;
  the resolved SHA and fetched bytes are cached under
  `<lazybox_home>/snippet-cache/` and reused offline. Refresh is
  *on demand* (`lazybox snippets refresh`), never silently at startup, so a
  moved upstream can't change your prompts between launches.
- **Review-on-change.** A refresh that changes a cached pack surfaces a
  diff the user confirms before it takes effect — the same confirm-with-
  preview posture Ask Lazybox already uses for `upsert_global_snippet`.
- **https/pinned-git only**, size-capped, parsed through the existing
  `SnippetsFile` path (a malformed pack is skipped with a log warning, as a
  malformed local file already is), and origin-labeled (a new
  `SnippetOrigin::External`) so the picker shows provenance.

**MVP scope** for the follow-up PR: the `sources` field, the fetch+cache
for `https` and pinned `github:` refs, `lazybox snippets refresh` with the
confirm-on-change diff, and the `External` origin label. A hosted
marketplace is explicitly out of scope for the MVP.

## Sources

1. Anthropic security-review prompt —
   <https://github.com/anthropics/claude-code-security-review/blob/main/claudecode/prompts.py>
2. Claude Code code-review plugin —
   <https://github.com/anthropics/claude-code/blob/main/plugins/code-review/commands/code-review.md>
3. Cursor, "Building Bugbot" — <https://cursor.com/blog/building-bugbot>
4. Greptile v3 agentic code review —
   <https://www.greptile.com/blog/greptile-v3-agentic-code-review>
5. awesome-cursorrules, anti-sycophancy code discipline —
   <https://github.com/PatrickJS/awesome-cursorrules/blob/main/rules/anti-sycophancy-code-discipline-cursorrules-prompt-file.mdc>
6. ai-boost/awesome-prompts, security reviewer —
   <https://github.com/ai-boost/awesome-prompts/blob/main/prompts/code_reviewer_security.txt>
7. OpenAI GPT-4.1 Prompting Guide —
   <https://developers.openai.com/cookbook/examples/gpt4-1_prompting_guide>
8. Anthropic prompt-engineering guide (be clear & direct; take action) —
   <https://platform.claude.com/docs/en/docs/build-with-claude/prompt-engineering/be-clear-and-direct>
9. awesome-cursorrules, PR review —
   <https://github.com/PatrickJS/awesome-cursorrules/blob/main/rules/pr-review-cursorrules-prompt-file.mdc>

Supporting reading on severity/noise-suppression philosophy: CodeRabbit
(<https://www.coderabbit.ai/blog/how-coderabbit-delivers-accurate-ai-code-reviews-on-massive-codebases>),
Semgrep Assistant
(<https://semgrep.dev/blog/2025/announcing-ai-noise-filtering-and-triage-memories/>),
Graphite Diamond (<https://graphite.com/features/ai-reviews>).

*Verification note:* the Anthropic prompt files, the OpenAI/Anthropic
guides, and the vendor blogs above were confirmed firsthand during
research. One future-dated arXiv preprint surfaced during the pass but
could not be independently verified, so it is deliberately omitted rather
than cited.
