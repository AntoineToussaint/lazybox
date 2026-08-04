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
