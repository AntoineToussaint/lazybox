# Context hygiene: one policy, two enforcement points

A large share of an agent's input-token bill is mechanical. A 900-line file read
on turn 3 rides in every request for the rest of the session; command output
skimmed once is re-sent forty times. The premium model re-ingests all of it,
every turn, at full price.

The fix is to route that material through a cheap model so the expensive one
never ingests the raw bytes. The part worth stating plainly, because it is the
whole reason this is code and not a prompt: **instructing a model to keep its own
context clean does not work.** Written rules are a suggestion; a block is not.
(Epic [#1611](https://github.com/AntoineToussaint/lazybox/issues/1611); the
prior art is Spotify's Claude Code setup, secondhand and with its headline
percentage unverified — which is exactly why [#1606](https://github.com/AntoineToussaint/lazybox/issues/1606)
measures our own fleet before anything rewrites anything.)

## Why lazybox can do this generically

The published version of this idea is a hook, in a repo, for one agent. lazybox
already sits between *every* agent and its model API through the metering proxy,
and already writes Claude's `PreToolUse` hook at spawn. So there are two places
to enforce, both repo-free:

```
                 ┌──────────────────────────────┐
  agent ──req──▶ │ metering proxy (all agents)  │──▶ upstream
                 │  • instrument       (#1606)  │
                 │  • compact old tool results  │
                 │                     (#1609)  │
                 └──────────────┬───────────────┘
                                │ uses
                 ┌──────────────▼───────────────┐
                 │ summarizer service   (#1608) │  cheap model, content-addressed,
                 │ byte-stable, cached in store │  survives restart
                 └──────────────▲───────────────┘
                                │ uses
                 ┌──────────────┴───────────────┐
  Claude ──hook▶ │ PreToolUse deny/redirect     │  block the read before it happens
                 │                     (#1610)  │  (sharper, per-agent)
                 └──────────────────────────────┘
```

The proxy is the cross-agent baseline — Claude, Cursor and Codex all reach their
upstream through it. The hook is strictly better where it applies, because the
raw bytes never enter the transcript at all, but it only exists for agents with a
pre-tool decision surface. Codex is not even on the MCP bus
(`supports_mcp_config()` is false), which is why the voluntary "read the
condensed version" path is folded into the hook's redirect rather than shipped as
an MCP tool nobody but Claude could call.

## Why the policy is shared code, not a convention

Both enforcement points can act on the same file in the same session. If they
disagree, the model sees that file condensed one way through the hook and another
way through the proxy — and every divergence is a prompt-cache miss, paid twice.
Two enforcement points that disagree are worse than one enforcement point.

So `crates/core/src/context_hygiene.rs` owns the three things that must not
drift, and nothing else:

| | |
|---|---|
| `ContextHygiene::eligibility(&ToolResultFacts) -> Eligibility` | the verdict |
| `cache_key(input, kind, model, prompt_version)` | the cache identity |
| `render_condensed(kind, original_lines, summary)` | the bytes |

Each enforcement point owns its own mechanics — the proxy owns the request-body
parser and the rewrite, the hook owns the decision round-trip, the summarizer
owns the model call and the store cache.

## The knobs

All under `agent.context_hygiene`. One block, because three separate `agent.*`
keys read by three different slices is precisely the drift this design exists to
prevent.

```yaml
agent:
  context_hygiene:
    mode: shadow                    # off | shadow | on
    keep_recent: 4
    min_lines: 350
    hook_intercept: true
    condense_model: claude-haiku-4-5
    prompt_version: 1
    condense_timeout_ms: 15000
    condense_input_cap_bytes: 262144
```

- **`mode`** — `shadow` (the default) decides everything `on` decides, logs what
  it *would* rewrite and what that would save, and sends the original bytes.
  Rewriting an agent's context is load-bearing for correctness, not just
  accounting, so it earns its way from evidence rather than starting on. Note
  `on` is a YAML 1.1 boolean and must be quoted: `mode: 'on'`.
- **`keep_recent`** — tool results this close to the newest are never touched,
  whatever their size. The model is likely mid-task on them, and edits need real
  content.
- **`min_lines`** — the eligibility floor. Below it a rewrite cannot repay the
  one deliberate cache miss it costs on the turn it happens. #1606's "blocks over
  N lines" counter reads the same number, so what is measured and what is
  rewritten cannot diverge.
- **`hook_intercept`** — let agents with a pre-tool decision hook block the read
  before it happens. Ignored by agents without one.
- **`condense_model`** — fallback when the serving agent declares no cheap tier.
  Model selection normally reuses the agent's own ladder
  (`AgentModels::alias_for_priority(PriorityTier::Low)`), so Claude sessions
  condense with Haiku and Codex sessions with their own cheap tier.
- **`prompt_version`** — bumped by hand when the condense prompt changes. It is
  part of the cache key, so a new prompt yields new keys rather than silently
  different output under old ones.

Only sessions actually routed through the proxy are affected: `metering_proxy`
plus the per-workspace / Space / `meter_all` routing. On an unmetered fleet this
is inert — which also means a `0%` on the stats screen means "not measured
here", not "no waste here".

## The constraints that shape every slice

**Prompt caching is make-or-break.** Claude Code re-sends the whole conversation
each turn with cache breakpoints. A rewrite that is not byte-stable across turns
invalidates the cached prefix and *costs more than it saves*. Three properties
hold the line:

- *Monotone.* A block is condensed on turn *k* and byte-identical on every turn
  after. It is never un-condensed, and never re-condensed — `is_condensed()`
  makes our own output ineligible, so the rewrite is idempotent.
- *Byte-stable rendering.* `render_condensed` is a pure function of its
  arguments, and the cached summary behind it is content-addressed, so the same
  block renders identically from either enforcement point forever.
- *Measured, with a kill switch.* Turn *k* itself is one deliberate cache miss
  per block. The proxy already parses `cache_read_input_tokens` and
  `core/pricing.rs` prices it, so the miss is visible in dollars — and #1609
  disables compaction for a session whose cache-read share drops and does not
  recover. This never silently loses money.

**Edits need real content.** The recency window is absolute — size never buys
past it — and every condensed block names its way back to the real bytes. The
re-read affordance in the header is load-bearing, not decoration: condensation is
only safe because an explicit, always-permitted path back exists for the moment
the model needs the real thing. At the hook that path is narrower (a `Read` with
an explicit `offset`/`limit` is the only thing allowed through), so the hook
appends its own instruction *after* the shared header rather than rewording it —
keeping the cached prefix byte-identical to the compactor's.

**Never stall an agent.** Summarizer failure, timeout, upstream 5xx, an
unrecognized wire shape, a parse failure — every one of them forwards the
original bytes. The hook's failure mode is exit 0 with nothing printed. Pass
through on doubt, always.

## Cache identity

`cache_key` is SHA-256, hex, over length-prefixed fields: `prompt_version`, the
model, the kind discriminant, the kind's label (the path, the command), and the
input. It is *not* the FNV-1a/64 that `Snippet::content_hash` uses. A cache hit
is served straight back into an agent's context, so a collision would put one
file's summary under another file's header — and the inputs here are unbounded
tool output, not the small closed set of snippet bodies. Length prefixes mean no
field's content can impersonate a boundary and forge another entry's key.

Entries live in the store kv under `condense:`, not in memory, because agent
processes survive a daemon restart and keep sending the same blocks. Both
enforcement points read and write that one space, so a file condensed by the hook
is never re-condensed by the proxy.

## Slices

| | | |
|---|---|---|
| [#1606](https://github.com/AntoineToussaint/lazybox/issues/1606) | Instrument | Tool-result share and re-send ratio per agent. No behavior change — it answers whether there is a 90% here or a 15% for *our* fleet before anything is built on the assumption. |
| [#1608](https://github.com/AntoineToussaint/lazybox/issues/1608) | Summarizer | One cheap-model condense call behind the content-addressed, restart-surviving cache. Shared by both enforcement points. |
| [#1609](https://github.com/AntoineToussaint/lazybox/issues/1609) | Proxy compactor | The generic block: rewrite old, large tool results in the buffered request body, for every agent the proxy fronts. |
| [#1610](https://github.com/AntoineToussaint/lazybox/issues/1610) | `PreToolUse` | The sharper blade where the agent supports it: deny the read, return the condensed text as the reason. |

Out of scope for now: the second helper in the Spotify write-up, a cheap model
that *generates* boilerplate from a neighbouring example and writes it to disk.
That is a delegation pattern, not a context-hygiene one — it fits a snippet or a
skill better than a proxy rewrite, and can be its own issue once these four land.
