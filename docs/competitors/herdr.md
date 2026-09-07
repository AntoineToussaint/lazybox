# Competitor brief: herdr (herdr.dev)

Status: **field notes** from a docs + source read (2026-09). Not exhaustive.
Sibling brief: [`agent-orchestrator.md`](agent-orchestrator.md). Strategy:
[`../positioning.md`](../positioning.md).

## What it is

"The runtime your coding agents live on." An **agent-native terminal
multiplexer**: a background server owns the PTYs that Claude Code / Codex /
Cursor / OpenCode / Grok run in, so agents survive a closed lid, a dropped
network, or a restart, and you reattach from any terminal or over SSH. Single
Rust binary, no Electron, macOS/Linux/Windows, Apache-2.0. By Ogulcan Celik,
YC-backed; release ~v0.8.2. Think **tmux that understands agents**.

Star/install counts are inconsistent across sources (site markets ~35k stars /
~707k installs / ~961 plugins; secondary trackers cite ~22–32k stars) — treat
the big numbers as soft. Regardless of the exact figure, it is the highest-
mindshare tool in this category and the one users will ask us about.

## Architecture (from docs + AGENTS.md)

- **Client/daemon split over a Unix socket** — a background session server holds
  the terminals; a thin TUI renders. Same shape as lazybox. **Convergent, not a
  differentiator.**
- **Worktree per task is core** — `worktree.create/open/remove`,
  `herdr worktree create --branch/--base`.
- **Socket API** over newline-delimited JSON: `pane.*` (split/run/read/
  send_text/send_keys), `agent.*`, `workspace.*`/`tab.*`, `events.subscribe`.
- **State detection is screen-snapshot / UI-pattern based**, *not* process names
  or scrollback regex — "the detector reads a screen snapshot… matching visible
  invariant controls." Four states: **idle / working / blocked / done** (+
  `unknown`), per-agent reloadable manifests, inspectable via `agent.explain`.
- **Plugin marketplace** — the provider/PR/inbox/auto-merge surface lives here,
  not in core.

## Their thesis vs ours

| | herdr | lazybox |
| --- | --- | --- |
| Center of gravity | The **runtime** — where agents live, which one is stuck | The **inbox** — what to work on, did anything happen |
| Core metaphor | Workspace → tab → pane (a better tmux) | Provider task → session (a reactive inbox) |
| Provider layer (GitHub/Linear/Slack) | **Plugin-only** | **Core** |
| Auto-merge-on-green / auto-fix CI | **Plugin-only** | **Core** |
| Agent↔agent messaging | ✓ `agent.prompt` (push) + `agent.wait` (observe) | ✓ `x s` send-to-session, `notify_session`, notes blackboard |
| Task lease / anti-double-spawn | Not documented | ✓ heartbeat `working`-label lease + SpawnCoordinator |
| State detection | Screen-snapshot UI-pattern matcher | asking / working / CI-failing badges from provider + PTY |
| Platforms | macOS · Linux · Windows | macOS · Linux |

## The one correction we had gotten wrong

herdr **does** have a genuine agent-to-agent **push** primitive — `agent.prompt`
(CLI `herdr agent prompt <name> "<text>"`) injects text + Enter into another
pane, even one already working. Earlier internal notes claiming herdr is
"observation-only" were wrong. It is the *only* researched competitor with real
cross-agent messaging (Conductor / Claude Squad / Vibe Kanban / Crystal are
isolation-only). Our edge here is **not** "we have messaging and they don't" —
it's that our messaging is wired into an inbox and provider layer that they
leave to plugins.

## What they do genuinely well (respect / borrow)

- **Persistence + state awareness as the headline** — the "never lose the stuck
  agent" story is crisp and is the #1 thing users praise. Our badges do this too
  but we don't *market* it as cleanly.
- **The socket API is a real product surface** — documented, JSON, with
  `agent.explain` for debugging detection. A good model for hardening our own
  JSON API / MCP story.
- **Screen-snapshot detection** decoupled from the VT parser is a clean design
  worth studying against our detection path.
- **Distribution + mindshare** — brew/mise/one-liner installers, Windows, a
  plugin marketplace, and the category's biggest star count.

## Where we're honestly stronger

1. **The provider inbox is core, not a plugin.** GitHub + Linear read/unread,
   CI/review events, label-to-spawn — shipped in the binary. On herdr this is a
   third-party plugin (`herdr-agent-inbox`) you assemble yourself.
2. **The landing toil is automated.** Auto-merge-on-green + auto-fix-CI are core.
   The loudest documented pain across *all* these tools is the merge-conflict /
   CI tax at scale; herdr leaves it to plugins.
3. **Fleet safety.** The heartbeat task-lease + SpawnCoordinator stop two agents
   grabbing the same issue — no equivalent found in herdr core.
4. **The control layer.** Multi-select + broadcast, snippets with memory,
   send-to-session, model-tier dispatch — the ~hundred affordances in
   [`../positioning.md`](../positioning.md) that make *many* agents tractable.

## Known weaknesses users report

- **Rendering lag with many panes open** (HN: text-render delay past ~10 panes)
  — directly relevant to the 20–30-agent workload; worth a head-to-head on our
  VT throughput.
- **State-detection reliability is unquantified** — vendor design claims only, no
  published accuracy numbers; a snapshot/UI-pattern matcher can drift as agent
  CLIs restyle their prompts.
- **Everything past the runtime is assembly-required** — provider inbox, merge
  automation, PR tracking are all plugins of varying maturity.

## So what (implications for lazybox)

1. **Don't fight herdr on "runtime."** Persistence + a socket API + state
   awareness is table stakes now and they do it well. Fighting there is a
   feature race we don't need.
2. **Lead with the layer above the runtime.** Provider inbox + automation
   policies + fleet safety, all core. That's the sentence: *"herdr is where
   agents live; lazybox is what tells you which one needs you and lands the
   work."*
3. **The two are complementary, not exclusive.** A herdr-as-runtime /
   lazybox-as-brain integration (drive its socket API instead of, or beside, our
   own daemon) is worth a spike — it neutralizes the mindshare gap by meeting
   their users where they are.
4. **Borrow the marketing crispness**, not the scope. Their "never lose the
   stuck agent" line converts; our equivalent capability is buried.

## Sources

- [herdr.dev](https://herdr.dev/) ·
  [docs](https://herdr.dev/docs/) ·
  [agent automation](https://herdr.dev/docs/agent-automation/) ·
  [socket API](https://herdr.dev/docs/socket-api/)
- [github.com/herdrdev/herdr](https://github.com/herdrdev/herdr) (AGENTS.md, SKILL.md)
- [awesome-herdr plugin catalog](https://github.com/yigitkonur/awesome-herdr)
  (confirms provider/PR/auto-merge are plugin-driven, not core)
</content>
