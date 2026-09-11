# Snippets vs Agent Skills: when to use which

lazybox [snippets](snippets.md) and Agent **Skills** (the `SKILL.md`
folders defined by the [Agent Skills standard](https://agentskills.io))
look almost identical from a distance — both are named, described,
reusable bundles of agent instruction. They are **complementary layers,
not competitors.** One axis separates them cleanly, and it should drive
which one you reach for.

(Skills started as a Claude Code feature and became an open format in
December 2025. All three agents lazybox spawns read the same `SKILL.md`
— Claude Code, Codex, and Cursor — but **not from the same roots**, and
that distinction is load-bearing: `.agents/skills` is the standard's
shared root, while `.claude/skills` and `~/.codex/skills` belong to one
agent each. `]]l` therefore scans the roots *the focused agent reads* —
`<repo>/.claude`, `<repo>/.agents`, then `~/.claude`, `~/.agents`,
`~/.codex`, filtered by agent — first root to claim a name winning it.
Listing a root its agent never reads would offer a skill that cannot
load, and describe a folder with no bearing on the session.)

**A skill is a trust surface, and lazybox vets nothing.** It loads as a
system-prompt fragment carrying the agent's full permissions — inside a
`skip_permissions` fleet and on the coordination bus — and the 2026
supply-chain record on published skills is poor. lazybox's `]]l` picker
therefore shows what it can *see* and nothing more: the skill's scope,
its path on disk, whether the folder bundles a `scripts/` directory
(a `⚠ runs code` tag — a directory-exists test, not a scan), and a
standing `not vetted by lazybox` note. A skill whose frontmatter marks it
as a lazybox export (#1672) says so too — as a claim, since the marker is
frontmatter anyone can write, and alongside the standing note rather than
in place of it; `lazybox snippet export --check` is what verifies it.
Read the `SKILL.md` before you invoke one.

Two details keep that disclosure honest rather than merely present. The
tag **leads** the row instead of trailing the description, because the
row truncates at the pane edge and a skill's description is long by
construction — a trailing tag is clipped away exactly when the row is
crowded. And where two roots the agent reads claim the same name,
lazybox picks a winner by *its* precedence while the agent resolves the
name by its own: so the preview names every candidate folder, and the
`⚠ runs code` tag is ORed across them. A name that might resolve to a
scripts-bundling copy is never presented as instructions-only. lazybox deliberately offers no install path: discovering
what is already on disk is a different job from being the channel that
puts it there.

The two bridges are built. A snippet can *dispatch* a skill (`skill:`),
and a snippet can *become* one (`lazybox snippet export <key>`, or `x` in
the `]` browser) — see [Export a workflow as a
skill](snippets.md#export-a-workflow-as-a-skill).

## The axis that matters: who *can* trigger it

Not "human vs model" — you can invoke either by hand. The real
difference is that a skill *adds* autonomous triggering on top of manual
invocation, while a snippet is human-only:

- **A snippet is human-only.** *You* open `]]s`, pick `rev`, and lazybox
  pastes and submits the body to the focused agent. There is no path by
  which it fires on its own — it is deterministic and in-the-loop, and
  you see the body in the preview before it goes.
- **A skill can also fire itself.** You can still invoke one by hand
  ("use the `code-review` skill", or a `/`-command if it's exposed as
  one), but its distinguishing power is that the agent reads each skill's
  `description` and can decide *itself* to invoke it mid-task, then
  progressively loads the `SKILL.md` body and bundled scripts on demand.

So the useful distinction is the *ceiling*, not the only mode: a snippet
can only ever be your deliberate act; a skill can be that **or**
autonomous. (Giving skills a first-class, previewable hand-trigger
inside lazybox is exactly the follow-up in the recommendation below.)

## Side by side

| Dimension | lazybox snippet | Agent skill (`SKILL.md`) |
| --- | --- | --- |
| Trigger | Human-only (`]]s<key>`, `Shift-B` broadcast) | Human **or** model — by hand, or autonomously on `description` |
| Payload | Single verbatim `body` (text only) | `SKILL.md` **plus** bundled scripts / files / resources |
| Progressive disclosure | No — the whole body is sent at once | Yes — name + description first, body then files on demand |
| Parameters / variables | No ([not yet supported](snippets.md#house-style-for-bodies)) | Effectively yes — the agent fills context from the task |
| Multi-step / tools | No — one prompt | Yes — can drive tools and run bundled code |
| Scope / layering | built-in → global → launch-dir (`~/.lazybox/snippets.yaml`) | per-repo `.claude/skills/` + `.agents/skills/`, shadowing the user-level `~/.claude`, `~/.agents`, `~/.codex` roots |
| Authoring | YAML + "Ask Lazybox" confirm-and-write | Author a `SKILL.md` folder by hand or via the agent |
| lazybox memory | MRU **Recent**, per-workspace `]N` badge, broadcast rollout | None — skill invocation is agent-internal |
| Portability | lazybox-only — until exported as a `SKILL.md` (`lazybox snippet export`), which any agent reading the standard then loads | An open standard — the same folder runs in Claude Code, Codex, Cursor and 40+ other tools, with or without lazybox |
| Determinism | High — you know exactly what fires | Lower — depends on the model's read of `description` |

## When to use which

**Reach for a snippet when you want to be in the loop.** You know the
process, you want it to run *now*, and you want to see exactly what the
agent is told. Snippets are deterministic and auditable (the body shows
in the preview before it fires), carry zero execution trust surface (it
is just a prompt), scale to a fleet through the `Shift-B` broadcast, and
work for any agent — Claude, Codex, Cursor, even a plain shell. Their
cost is that they are text-only, single-shot, unparameterized, and live
only inside lazybox.

**Rely on a skill when you want the agent to self-select the right
capability** without you thinking about it, especially when the job
needs bundled scripts, reference files, or genuine multi-step
orchestration. Skills carry a far richer payload and keep context lean
through progressive disclosure, and they travel with the repo across
every agent that reads the standard. Their cost is that they fire on the model's
judgment, are opaque to lazybox (no Recent, no `]N`, no preview,
no broadcast), and are a code-execution trust surface.

A rough rule: **if you would type the same instruction yourself and
want it to fire on your command, it is a snippet. If you want the agent
to notice the situation and apply a bundled, possibly multi-step
capability on its own, it is a skill.** The two rarely overlap in
practice, and where they do (a lazybox `rev` snippet vs. a Claude
`code-review` skill), pick by whether you or the model should be the
one pulling the trigger.

## One standard, one home: the review discipline lives in the snippet

The `rev` / `deepreview` / `fixall` overlap with a hypothetical Claude
`code-review` skill is the sharpest place these two layers could **drift** —
two copies of a long, carefully-tuned review prompt that soften apart over
time. #1145 settles it: the strict prompt text is the **single source of
truth, and it lives in the snippet body.**

- **Today there is no built-in `code-review` skill body** — lazybox only
  *discovers* the skills a repo or user already ships; it does not vendor
  or install one. So there is exactly one
  copy of the toughened review standard, in
  [`crates/config/src/snippets.rs`](../crates/config/src/snippets.rs), and
  a regression test (`no_soft_body_offers_a_banned_dismissal` and friends)
  keeps it from regressing.
- **The snippet is the right home** for it: the review discipline is a
  deterministic, in-the-loop, agent-agnostic instruction you want to fire
  *on your command* and see in the preview first — exactly a snippet's
  properties, not a skill's autonomous-trigger ceiling. It also has to work
  from a phone with one tap, where a snippet is the primary driver.
- **If a `code-review` skill is ever added** (the bridging work below), it
  must not fork the prompt: it should embed or reference the same standard
  the snippet encodes, so the banned-phrase / bias-to-action / falsifiable-
  skip rules have one authored source and one test guarding them. A skill
  that quietly relaxes the wording would reintroduce precisely the drift
  this decision exists to prevent.
- **Export (#1672) is that "embed", made mechanical.** `lazybox snippet
  export rev` writes a `SKILL.md` whose body is the snippet's, byte for
  byte, and records a byte-exact hash of it; `--check` (and a startup
  notice) reports an exported skill that has come apart from its snippet,
  whether because the snippet moved or because the file was edited in
  place. Nothing is ever read back from a skill into a snippet. So the
  portable copy is a *build artifact* of the one authored standard, not a
  second voice on it — and the #1145 tests keep guarding the single copy
  they always did.

## Recommendation and where this is headed

Today the two are invisible to each other: lazybox does not know what
skills the focused agent has, and skills do not know lazybox snippets
exist. The near-term stance is to **keep them as explicit complementary
layers** — this document is that framing — rather than converge them,
which would cost snippets the deterministic, agent-agnostic, broadcast
properties that make them good.

Bridging the two is scoped as follow-up work rather than built blind:

- **Surface the focused agent's skills in lazybox** so a skill can be
  triggered *explicitly* from the `]]` leader, gaining the snippet
  picker's preview + Recent + `]N` UX for a capability the agent
  otherwise only self-selects.
- **Let a snippet dispatch a skill**, keeping lazybox's picker / Recent
  / broadcast UX while a real skill does the heavy lifting.
- **Let "Ask Lazybox" scaffold a skill** (not just a snippet) when a
  request is genuinely multi-step or needs bundled code.
- **Export a curated snippet as a skill**, so lazybox's vetted library is
  portable rather than lazybox-only — skills as an export format, not an
  import one. Notably *not* an install pipeline: lazybox publishes what it
  authored and vouches for, and does not become the delivery path for
  skills it did not write. (Shipped as `lazybox snippet export`, #1672.)

See the issues linked from
[#793](https://github.com/AntoineToussaint/lazybox/issues/793) for the
scoped versions of that work.
