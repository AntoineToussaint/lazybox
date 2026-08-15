# Snippets vs Agent Skills: when to use which

lazybox [snippets](snippets.md) and Agent **Skills** (the `SKILL.md`
skills that Claude Code loads natively) look almost identical from a
distance — both are named, described, reusable bundles of agent
instruction. They are **complementary layers, not competitors.** One
axis separates them cleanly, and it should drive which one you reach
for.

(Skills are a Claude Code feature. Of the agents lazybox spawns —
Claude, Codex, Cursor — only Claude Code loads `SKILL.md` today; where
this page says "skill" it means that capability, and the bridging work
below starts there.)

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
| Scope / layering | built-in → global → launch-dir (`~/.lazybox/snippets.yaml`) | `.claude/skills/` per-repo + `~/.claude/skills/` user-level |
| Authoring | YAML + "Ask Lazybox" confirm-and-write | Author a `SKILL.md` folder by hand or via the agent |
| lazybox memory | MRU **Recent**, per-workspace `]N` badge, broadcast rollout | None — skill invocation is agent-internal |
| Portability | lazybox-only | Runs in Claude Code / API with or without lazybox |
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
every Claude surface. Their cost is that they fire on the model's
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
  *discovers* the skills a repo or user already ships (`.claude/skills/`,
  `~/.claude/skills/`); it does not vendor one. So there is exactly one
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

See the issues linked from
[#793](https://github.com/AntoineToussaint/lazybox/issues/793) for the
scoped versions of that work.
