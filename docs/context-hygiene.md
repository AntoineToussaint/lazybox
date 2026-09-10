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
| `ContextHygiene::cache_key(input, kind, model)` | the cache identity |
| `render_condensed(kind, original_lines, summary, tag) -> Option<String>` | the bytes |

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
  accounting, so it earns its way from evidence rather than starting on.

  There is a trap here worth stating once. `eligibility()` returns `Condense` in
  shadow mode — deliberately, because #1606's instrumentation needs the real
  verdict on every block while nothing on the wire changes. So the verdict
  answers *"is this block eligible"*, never *"may I act"*. **Permission is
  `mode.rewrites()`**, which is true only for `on`. An enforcement point that
  branches on the verdict alone enforces in the shipped default configuration —
  for the hook, that would be a denied read on a fresh install.
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

Zero is refused at load for `keep_recent`, `min_lines`, `condense_input_cap_bytes`
and `condense_timeout_ms`. Each of those zeros silently disarms something rather
than failing loudly: `keep_recent: 0` makes the *newest* tool result eligible —
the block the model is mid-edit on; `min_lines: 0` sends every one-line command
result to a model; `condense_input_cap_bytes: 0` truncates every input to
nothing; `condense_timeout_ms: 0` expires every call, disabling compaction while
the config still reads `mode: on`. `Config::parse` rejects them the same way it
rejects an out-of-range `server.ring_buffer_bytes`.

Only sessions actually routed through the proxy are affected: `metering_proxy`
plus the per-workspace / Space / `meter_all` routing. On an unmetered fleet this
is inert — which also means a `0%` on the stats screen means "not measured
here", not "no waste here".

### The dial is per workspace (#1622)

`mode` alone is fleet-wide, which is the wrong granularity for a pass that
rewrites what a model sees: the saving has to be proven on one row before every
row carries it. So `on` composes the same way metering's canary does — a
per-workspace flag, a Space, or the global mode, OR'd:

```yaml
agent:
  compacted_spaces: [obin-ai]       # Space tier, resolved through ui.spaces
```

- `Workspace::compact_context` — the per-row canary, toggled with `x h` on a
  workspace row. Off until chosen: unlike the meter, which is on by default
  because counting is harmless, this one is opted into a row at a time.
- `agent.compacted_spaces` — the Space tier, `x h` on a Space header, so a
  rollout widens from one workspace to one repo group without touching `mode`.
- `mode: 'on'` — the whole fleet.

`mode: off` is not promotable. It is the configured kill switch, the one setting
that means "not on my traffic", so a flag left on a row cannot resurrect the
rewrite under it — the same way no per-workspace meter overrides
`metering_proxy: false`.

The proxy resolves this **per request**, not at spawn: the session segment of the
proxy path names the workspace, so flipping the canary lands on that workspace's
next turn rather than its next agent. Membership (the flag, the Space) is
resolved by the daemon; promotion is `ContextHygiene::mode_for`, shared with the
hook enforcement point so the two cannot disagree about which sessions are
rewriting. It is resolved exactly once per request and carried on that request's
session pass, so the response's kill-switch accounting judges the turn under the
mode the turn actually ran under.

**Only `mode` is re-read live.** The rest of the block — `keep_recent`,
`min_lines`, the condense knobs — is snapshotted when the proxy starts, as it
always was; changing those still needs a daemon restart. And when the config
cannot be parsed at all, the per-request read falls back to the mode that was in
force at startup rather than to the built-in default, so a half-saved
`config.yaml` cannot quietly lift a configured `mode: off`.

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
  block renders identically from either enforcement point forever. It returns
  `Option`, and `None` is a guard against silent data loss rather than a
  formatting nicety: an empty or whitespace-only summary arrives through the
  summarizer's *success* path, so pass-through-on-error never fires. Rendering it
  would replace a real file with a header and nothing — permanently, since
  condensation is monotone and content-addressed. On `None`, send the original.
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

The cache holds the **summary**, not the rendered block, because the rendered
block carries a per-session tag (below) while the summary does not — which is
what lets two sessions share an entry.

## The marker is keyed, because it is a trust boundary

Recognizing our own output is what makes rewriting monotone. But tool results are
exactly the material an attacker controls — a repo file, a command's output, a
diff — so an unkeyed `[condensed by lazybox: …]` prefix would let any file whose
first line mimics it both evade condensation and present arbitrary text under
lazybox's provenance.

So the marker carries a `CondenseTag`: a token the daemon generates per session
and untrusted content cannot guess. Within a session it is constant, so rendered
bytes stay stable across turns; across sessions it differs, which is why it is
not part of the cache key. This closes the structural hole — lazybox no longer
acts on forged markers — but it cannot stop a model from believing a
plausible-looking line it reads in a file, which no marker scheme can.

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
