---
title: How lazybox compares
description: Where lazybox fits among parallel-agent tools — and why it's built for running many agents across many repos.
---

A growing set of tools run several coding agents in parallel, isolating their
work in checkouts or containers. That part is table stakes now, and some of
these tools are genuinely good at it. Where lazybox is different is **what
surrounds** the agents: it's a reactive, terminal-native inbox designed for
driving *many* agents across *many* repositories, not only a launcher or run
dashboard.

Three capabilities set it apart from the tools below:

- **A reactive multi-provider inbox.** GitHub PRs and issues, plus Linear
  tickets, flow into one read/unread event feed — new comments, CI failures,
  and review requests surface as they land. Slack mirrors workspace activity
  into channels and can route replies back to an existing agent; it is not a
  third task source. Some tools below accept external triggers or show agent-run
  history; none ships the same read/unread inbox across GitHub **and** Linear
  in its core binary.
- **Tag-to-spawn from a labeled issue.** Drop a `lazybox:` label on GitHub
  issues you authored or are assigned to — one, or a whole eligible backlog
  across repos — and lazybox opens each worktree and starts an agent, no TUI
  required. See
  [Trigger agents with @lazybox mentions](/docs/how-to/lazybox-mentions/).
  Among the documented products below, no other one documents this exact
  GitHub-label trigger.
- **Automation policies as core, not plugins.** Auto-merge-on-green lands clean
  PRs without you; auto-fix spawns an agent at failing CI. These ship in the
  binary — see [Manage automation policies](/docs/how-to/manage-automation-policies/).
  Every tool below either lacks them or leaves them to a plugin ecosystem.

Add a terminal-native TUI you can [forward over SSH](/docs/how-to/remote-over-ssh/)
and cross-platform (macOS **and** Linux) support, and lazybox is built to be the
one surface you live in when you're orchestrating a fleet.

## At a glance

| Tool | Interface | Isolation | Reactive inbox¹ | Tag-to-spawn² | Remote / headless | Platforms | License |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **lazybox** | Terminal TUI | Worktree | ✓ GitHub · Linear | ✓ GitHub label | ✓ SSH daemon | macOS · Linux | MIT |
| [herdr](https://herdr.dev/docs/) | Terminal TUI | Worktree | —⁸ | — | ✓ SSH / daemon | macOS · Linux · Windows | Apache-2.0 |
| [Conductor](https://www.conductor.build/docs/) | Native GUI | Worktree | — | — | ✓ Cloud / API⁹ | macOS · cloud | Proprietary |
| [Warp](https://docs.warp.dev/agent-platform) | Terminal + web | Checkout / container | — | —² | ✓ cloud / self-hosted | macOS · Linux · Windows | AGPL-3.0 client · proprietary service |
| [Claude Squad](https://github.com/smtg-ai/claude-squad) | Terminal (tmux) | Worktree | — | — | Host-dependent¹⁰ | macOS · Linux | AGPL-3.0 |
| [Crystal / Nimbalyst](https://github.com/stravu/crystal)³ | Desktop GUI | Worktree | — | — | — | macOS · Win/Linux source | MIT |
| [Vibe Kanban](https://www.vibekanban.com/docs/)⁴ | Web (local / cloud) | Worktree | — | — | ✓ paired host / cloud⁵ | Cross-platform | Apache-2.0 |
| [Sculptor](https://github.com/imbue-ai/sculptor) | Desktop GUI | Worktree (containers optional) | — | — | Partial⁶ | macOS · Linux | MIT |
| [container-use](https://container-use.com/) | CLI + MCP | Container + branch | — | — | Host-dependent | Cross-platform | Apache-2.0 |
| [Amp](https://ampcode.com/manual) | CLI + editors + web | Checkout / remote Orb⁷ | — | — | ✓ runners / Orbs | Cross-platform | Proprietary |

<div style="font-size:0.85em">

¹ A read/unread **event feed** across task providers (new comments, CI, review
requests) — not an agent-run dashboard, a one-off "open this issue" picker, or
the optional Slack mirror.
² Auto-spawn an agent from a label on a **GitHub issue**. Warp can spawn a cloud
agent when you tag `@Oz` in **Linear or Slack**, and also supports schedules,
APIs, and GitHub Actions; GitHub-label auto-spawn is not a documented Warp
feature.
³ Crystal was deprecated in early 2026 and continues as **Nimbalyst**; its final
Crystal docs cover macOS binaries plus Windows and Linux source builds.
⁴ Vibe Kanban is sunsetting: its parent (Bloop AI) is winding down, cloud/server
features are being removed, and the project continues open-source and
community-maintained with **local workspaces** intact.
⁵ Vibe Kanban documents cloud pairing to a running host and editor access over
Remote-SSH; it is not a standalone SSH daemon.
⁶ Sculptor's custom backend can run in Docker or on a remote host, marked
experimental.
⁷ Amp supports parallel subagents, remote Orbs, and runner-only mode, but does
not document automatic git-worktree-per-thread isolation.
⁸ herdr is an agent-native terminal multiplexer; a read/unread PR inbox exists
only as a **third-party plugin** (`herdr-agent-inbox`), not in core. Its
worktree, per-pane agent-state detection, and agent-to-agent push messaging
*are* core.
⁹ Conductor is local-first (runs Claude Code / Codex / Cursor / OpenCode on your
Mac); a paid **Conductor Cloud** tier and mobile/API access are add-ons.
¹⁰ Claude Squad runs agents in tmux sessions, so it works over SSH on a headless
box, but it is not itself a forwarding daemon.

</div>

## Four families, and where lazybox sits

**Agent-native terminal runtimes** — herdr. The closest architectural cousin to
lazybox: a background server owns the agents' terminals, so work survives a
closed lid, a dropped network, or a restart, and reattaches over SSH. Every pane
is classified **idle / working / blocked / done** from a screen-snapshot
detector, and a newline-delimited-JSON socket API is rich enough that agents
`agent.prompt` each other and `agent.wait` on each other's state — the only tool
here with a genuine agent-to-agent **push** primitive. What it deliberately
leaves out of core is the provider layer: PR/issue inbox, GitHub tracking,
auto-merge and auto-fix live in a **plugin ecosystem**, not the binary. herdr is
excellent at the runtime question ("where do my agents live, and which one is
stuck?") and pairs naturally *underneath* an inbox; lazybox answers the inbox
question ("what should I work on, and did anything happen?") with those provider
features shipped first-class.

**Workspace managers** — Conductor, Crystal/Nimbalyst, Vibe Kanban, Sculptor —
create isolated workspaces and put several agents within reach of a visual
interface. Their interfaces, collaboration models, and isolation options differ
substantially; none documents a multi-provider read/unread inbox or the
GitHub-label trigger defined above.

**Automation and execution platforms** — Warp's Oz platform can launch agents
from Slack or Linear mentions, schedules, APIs, and CI, on managed or
self-hosted container infrastructure. container-use gives MCP-compatible agents
fresh container-and-branch environments. These are strong choices when
automation surfaces or container isolation matter more than an inbox.

**Agent clients and remote runners** — Amp combines a CLI, editor integrations,
a web feed, subagents, and remote Orbs/runners. Claude Squad is the leanest of
the terminal managers — tmux sessions plus a worktree per agent, great over SSH
on a headless box. They manage parallel agent work, but neither documents a
provider inbox or automation policies.

lazybox is the cockpit: a keyboard-driven inbox where every PR, issue, and
ticket across every connected repo is a row you can turn into an isolated agent
workspace — and where a label on an issue starts that work without you opening
anything at all.

## The shared blind spot: the merge-conflict tax

Across every tool here — herdr included — isolation is worktree- or
container-deep, which *defers* conflicts to merge time rather than preventing
them. The most consistent thing people report when running many agents at once
is the resulting merge-and-CI toil: the agents finish in parallel, then you
serialize on landing them. None of the tools above ships collision detection or
a task lease; parallelism is safe at the filesystem, and manual at the finish
line.

lazybox doesn't escape worktree isolation either, but it's built to attack the
*toil* around it. [Automation
policies](/docs/how-to/manage-automation-policies/) let auto-merge-on-green land
clean PRs without you and auto-fix put an agent on failing CI, and the
[@lazybox-mention / label trigger](/docs/how-to/lazybox-mentions/) dispatches a
whole backlog without double-assigning the same issue to two agents. The finish
line is where a fleet actually bottlenecks, and it's the part most of these
tools leave entirely to you.

## When another tool is the better fit

Honesty helps you trust the rest of this page:

- You want a **persistent, agent-aware terminal runtime** — panes that survive a
  closed lid, per-pane idle/working/blocked/done, agents that prompt each other
  over a socket API — and you'll assemble the PR/inbox/merge side from plugins →
  herdr is purpose-built for that runtime layer (and pairs well *under* an inbox
  like lazybox).
- You want a **polished native GUI** and work on a Mac, one task at a time →
  Conductor is a strong, focused choice.
- You want the **leanest terminal multiplexer** for a handful of agents over SSH
  → Claude Squad's tmux-plus-worktree model is hard to beat for simplicity.
- You want a **web kanban board** your whole team opens in a browser →
  Vibe Kanban's board model fits that shape better than a TUI (note its sunset).
- You want **managed or self-hosted cloud agents** launched from schedules,
  APIs, CI, Slack, or Linear → Warp's Oz platform is built for that.
- You need **container isolation through MCP** around an existing agent →
  container-use is built for exactly that.
- You want an **agent client with subagents and remote runners** across CLI,
  editors, and web → Amp is designed for that.

If instead you're running **many agents across many repositories**, want work to
**flow to you** instead of hunting for it, want to **start a whole backlog of
eligible issues with a label**, and want the **landing toil automated** — from a
terminal you can forward over SSH — that's the workload lazybox is built for, and
nothing above matches the combination.

---

<sub>Compiled from each tool's public documentation as of September 2026. A `—`
means a capability is not a documented core feature — not proof it's impossible,
and several of these tools have rich plugin ecosystems; they also move fast. Spot
something out of date? [Open an
issue](https://github.com/AntoineToussaint/lazybox/issues).</sub>

## Sources

- [herdr docs](https://herdr.dev/docs/),
  [agent automation](https://herdr.dev/docs/agent-automation/),
  [socket API](https://herdr.dev/docs/socket-api/), and
  [the repository](https://github.com/herdrdev/herdr) (Apache-2.0)
- [Conductor: worktrees](https://www.conductor.build/docs/concepts/git-worktrees),
  [cloud-workspace API](https://www.conductor.build/docs/api), and
  [pricing](https://www.conductor.build/pricing)
- [Warp: Oz overview](https://docs.warp.dev/agent-platform),
  [integrations](https://docs.warp.dev/reference/cli/integration-setup), and
  [environments](https://docs.warp.dev/agent-platform/cloud-agents/environments);
  [the client is AGPL-3.0](https://github.com/warpdotdev/warp)
- [Claude Squad repository](https://github.com/smtg-ai/claude-squad) (AGPL-3.0)
- [Crystal repository and final documentation](https://github.com/stravu/crystal)
- [Vibe Kanban: worktrees](https://www.vibekanban.com/docs/workspaces/repositories),
  [remote access](https://www.vibekanban.com/docs/remote-access), and
  [sunset notice](https://www.vibekanban.com/blog/shutdown)
- [Sculptor: workspaces](https://github.com/imbue-ai/sculptor/blob/main/docs/help/workspaces.md)
  and [experimental container / remote backend](https://github.com/imbue-ai/sculptor/blob/main/docs/help/experimental/container_backend.md)
- [container-use repository and documentation](https://github.com/dagger/container-use)
- [Amp Owner's Manual](https://ampcode.com/manual)
</content>
</invoke>
