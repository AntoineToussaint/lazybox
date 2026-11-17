# Lazybox V2 and herdr: the GUI, side by side — 2026-11-16

The [audit README](README.md) compares the two at a feature level. This
note compares the **interfaces**: what a person sees and touches in herdr's
desktop and phone apps, against the Lazybox V2 desktop/web UI on
`feat/v2-command-output` at `9c234e28` (missions slice 1 included). It ends
with a prioritized list of UI changes.

Method. herdr: its site, the `herdrdev/herdr-app` repository (README,
`CHANGELOG.md`, the shipped `defaults/` folder: `AGENTS.md`, `keys.json`,
layouts and the default mods' source), the TUI's README and keyboard docs,
the mobile page, a heise article, a Bitdoze gallery tour and the HN launch
thread. Every claim links its source. Lazybox: code on this branch, with
screenshots taken against the fake Runner (`scripts/fixtures/runner-ui.mjs`
plus `apps/web` through Vite, as the `scripts/test-*.mjs` suites do). The
screenshots are in `.cache/compare-*.png`, which is ignored and not
committed. I did not run the herdr app. Its side comes from documents and
its own source, so anything that depends on how it feels to use is marked
as reported, not observed.

## What herdr is (as of 2026-11-16)

herdr is three products. Only the second one is a GUI.

| Product | What it is | Version / date | Source |
|---|---|---|---|
| **herdr** (TUI) | Rust/ratatui terminal multiplexer with its own server. Uses a tmux-style `ctrl+b` prefix, a sidebar of spaces and agents, and per-pane agent state. | v0.9.5, 2026-10-14; 51.4k stars | [README](https://github.com/herdrdev/herdr), [releases](https://github.com/herdrdev/herdr/releases), [keyboard](https://herdr.dev/docs/keyboard/) |
| **Herdr** (desktop app) | A native GPU app on Zed's **GPUI**, with a Ghostty-based terminal (`crates/ghostty-sys`). It is a client of **herdr-core** (`hc`), which owns every terminal. The whole UI is JSON layouts plus JavaScript "mods" that hot-reload. It does **not** share sessions with the TUI. | 0.1.16, 2026-11-14 (repo created 2026-10-16; 13 releases since 0.1.4 on 10-23); 3.0k stars, 644 open issues | [herdr-app README](https://github.com/herdrdev/herdr-app), [CHANGELOG](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md), [heise, 2026-11-02](https://www.heise.de/en/news/Herdr-desktop-app-A-hub-for-AI-coding-agents-11508175.html) |
| **Herdr mobile** | iPhone, iPad and Android app over SSH to herdr-core. It has an urgency inbox, the real terminal, a key bar, compose and dictation. It does not work with the TUI. | no version given | [herdr.dev/mobile](https://herdr.dev/mobile) |

Company context: Y Combinator ([2026-08-06](https://herdr.dev/blog/herdr-is-joining-y-combinator/)),
then a $6M seed led by Bessemer ([2026-09-08](https://herdr.dev/blog/herdr-raised-a-seed/)).

The one design fact that explains everything below: **herdr never renders an
agent; it renders the agent's terminal.** Claude Code, Codex and the other 20
or so CLIs run unmodified in panes. herdr adds the frame around them:
spaces, tabs, a status per pane, the agents list, notifications and
remote machines. Agent state comes from an open escape sequence, OSC 7501
(`working` / `blocked` / `idle` / `done`). An agent that doesn't emit it
"still runs but shows no status" ([herdr.dev](https://herdr.dev),
[defaults/AGENTS.md § The model](https://github.com/herdrdev/herdr-app/blob/master/defaults/AGENTS.md)).
Lazybox is the opposite: it **owns the conversation** and re-renders it as
chat, approvals, review and evidence. So herdr wins on frame, density and
fidelity, and Lazybox wins wherever understanding what the agent did matters.

## Verdict

> **Evening update.** The [re-audit](#re-audit-evening) re-checked all 44
> rows against the code and against screenshots. It finds **21 rows won by
> Lazybox, 11 by herdr and 12 tied**, and gives a fix brief for every row
> Lazybox does not win. The coordination design is in
> [sota-agents.md](sota-agents.md).

As a *window manager for agents*, herdr's GUI is ahead of Lazybox V2 by a
clear margin. These are the points that matter:

1. **The agents list is always on screen and sorted by urgency.** Lazybox
   puts the same information behind an Inbox button that opens a modal.
2. **Every action is one command.** Keys, the palette, the macOS menu bar,
   mods and the CLI all share a single list of commands. Lazybox has a
   six-entry `]]` menu, and file-only Mod+P.
3. **Panes are fully direct-manipulated:** drag to swap, drag to an edge,
   zoom, keyboard swap and resize. Lazybox can dock and collapse panels, but
   it has no zoom and no tabbed stacks.
4. **The terminal is first-class in herdr.** Not a gap for Lazybox: V2 is
   deliberately **not a terminal emulator** (see "Decision: no terminal
   emulator" below).
5. **It ships weekly** with release notes in the app, a welcome screen,
   themes, zoom and fonts. Lazybox is dark-only, with no onboarding and no
   release notes in the app.

Lazybox is ahead on everything **after** the agent stops: reading what it
did, review comments, the PR loop, checkpoints, the editor, Run/Preview,
cost, native OS notifications, missions with evidence, and approvals as UI.
herdr's Changes panel is one list of uncommitted files with a commit box
(its `changes` mod), and it has no approvals, PRs or cost at all.

The blunt part: **Lazybox's chat must be at least as good as the agent's
own TUI, because it replaces it.** It isn't yet. Question options are plain
text above a free-text field (`session-parts.tsx:36`). Tool calls are hidden
in an Activity dialog. Approval offers only "Allow once". herdr users
lose none of this, because they see Claude Code itself. Until the chat
reaches that bar, every herdr user who tries Lazybox will feel a downgrade
in the one surface they use most.

## Side by side

These tables are the morning comparison, kept as written. **The evening
re-audit below supersedes their Ahead column:** see
[Re-audit (evening)](#re-audit-evening) for today's winner of every row.

**Ahead** names who leads on that row. "=" means parity. Lazybox paths are
relative to `packages/surfaces/src/` unless they say otherwise.

### Shell, layout, panes

| Aspect | herdr app | Lazybox V2 | Ahead | Evidence |
|---|---|---|---|---|
| Model | A space is one repo on one machine. A tab is one task. Panes split right or down and hold terminals or mods ([AGENTS.md § The model](https://github.com/herdrdev/herdr-app/blob/master/defaults/AGENTS.md)). | A workspace holds several repositories. It has up to 8 pages, each a split tree of up to 12 panels (Workspaces, Chat, Command output, Terminal, Changes, Files, Editor, Processes, Actions, Preview, Usage, Missions, Ask Lazybox). | Lazybox for scope (multi-repo); herdr for simplicity | `workspace-model.ts:22-48`, `PanelRegistry.tsx:70-84` |
| Moving panes | Drag a pane by its header dots onto another pane to swap them, or onto an edge to dock it beside, with a landing highlight ([0.1.5](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)). `pane.swap` and `pane.go` work by keyboard (`cmd-alt-[shift]-arrows`). | Drag a panel header to a left/right/top/bottom dock zone, or use the ⋯ menu "Dock left/right/top/bottom". There is no swap by drag and no keyboard move. "Customize page" mode offers ←/→ swap. | herdr | `usePanelDocking.ts`, `WorkspacePanel.tsx:57-99`, `PageEditor.tsx` |
| Zoom a pane | `pane.zoom` (`cmd-shift-enter`); navigation follows the zoom ([0.1.16](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)). | None. A panel can only collapse or close. | **herdr** | grep: no zoom/maximize in `PanelLayout.tsx`, `WorkspacePanel.tsx` |
| Tabs | Every space has tabs (pills with a `+` menu that starts any installed agent). Tabs can be renamed (F2), reordered, selected with `cmd-1..9`, scrolled one at a time, and "settled" (put away). | Panels never stack as tabs. Only the Editor (tabs in 4 groups) and Terminal (one tab per shell) have tabs. Pages are tab-like but can only be created through Customize or JSON. | **herdr** | `WorkspacePanel.tsx`, `EditorArea.tsx`, `TerminalPanel.tsx` |
| Whole-window layouts | `layouts/*.json` is the window itself. It ships Herdr, Terminals and Herdr 98. `cmd-shift-l` cycles layouts. The site's gallery claims 312 community layouts (a 2026-11-08 count said 215) ([guide](https://herdr.dev/guide/), [Bitdoze](https://www.bitdoze.com/herdr-app-layouts-mods/)). | One built-in arrangement per page plus a raw JSON "Layout configuration" import. There are no presets to switch between. | herdr | `PageEditor.tsx`, `Workspace.tsx:360` |
| Escape hatch | `ctrl-alt-shift-escape` always restores the default layout and keys, and cannot be rebound. | "Reset retained layout" appears only when a restore fails. | herdr | `Workspace.tsx:360` |
| Secondary windows | One window; a peek over the tab for sub-agents. | A chat or a whole workspace can be detached into its own native window (desktop) or `window.open` (web), and reattached. | **Lazybox** | `apps/desktop/src-tauri/src/host.rs`, `placement.rs` |

### Sidebar, agent list, agent state

| Aspect | herdr app | Lazybox V2 | Ahead | Evidence |
|---|---|---|---|---|
| Structure | Two stacked sidebar blocks. **Spaces** are grouped by machine; worktree spaces nest under their repo; each shows repo, branch, ahead/behind and uncommitted changes. **Agents** is every agent across spaces and machines ([main.json](https://github.com/herdrdev/herdr-app/blob/master/defaults/layouts/main.json), [AGENTS.md § Mods](https://github.com/herdrdev/herdr-app/blob/master/defaults/AGENTS.md)). | One Workspaces list with 3-line rows: name, repositories, and an agent line showing provider · model · status, which truncates to "C." at default width (screenshot `compare-workspace.png`). Conversations are listed **only under the selected workspace, and only if it has more than one**. Rows show no branch and no dirty or ahead state. | **herdr** | `WorkspaceRow.tsx:205-215`, `CollectionPanel.tsx` |
| Sorting | A toggle between **Priority** (needs you > just done > working) and **Grouped by space**. Hovering shows a **pin** that keeps an agent on top in both views. Section headers **fold** and keep a count ([0.1.4](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)). Pins are shared with the phone ([0.1.12](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)). | Workspaces stay in Runner order and conversations in creation order. There is no pin and no fold. Urgency sorting exists only inside the Inbox dialog. | **herdr** | `conversation-model.ts:47`, `Inbox.tsx` |
| States | `working` / `blocked` / `idle` / `done`. `done` means finished while you weren't looking and becomes `idle` once seen. A space or tab shows its most urgent agent (blocked > done > working > idle). | Five tones: busy (spinning ring), waiting (breathing amber dot), failed (red), idle, saved. Unread "review" (lilac) and "failed" (red) dots replace the indicator. Live text ages: "Running a command · 12s", "Running · 2 sub-agents". | = (Lazybox's wording is richer; herdr rolls state up to tab and space) | `live-status.ts`, `LiveIndicator.tsx`, `attention.ts` |
| Coverage | Any program that emits OSC 7501 counts. 22 agent CLIs are detected, and Goose, Cursor, Kimi, Grok and others get their own icons. | Only Claude and Codex, which Lazybox runs itself. | herdr | [herdr.dev/tui](https://herdr.dev/tui/), `NewSessionForm.tsx` |
| Next/prev agent | `cmd-]` / `cmd-[` follow the agents list's own order (`setAgentOrder`). `ctrl-1..9` select a space. | j/k inside the focused sidebar only. There is no global "next agent that needs me". | **herdr** | [keys.macos.json](https://github.com/herdrdev/herdr-app/blob/master/defaults/keys.macos.json), `keyboard.ts:4-52` |

### Attention and notifications

| Aspect | herdr app | Lazybox V2 | Ahead | Evidence |
|---|---|---|---|---|
| Inbox | On the desktop, the Priority view of the agents list *is* the inbox, and it is always visible. The phone has a real Inbox (needs you, done, working, idle; a blue unread dot; swipe to pin or mark read; swipe left sends Esc) ([mobile](https://herdr.dev/mobile)). | A header button with a count (amber when urgent) opens a modal dialog grouped as Awaiting acceptance / Needs input / Failed / Ready for review / Working / Done, with j/k/Enter/m and "Mark all seen". | Lazybox for content, herdr for visibility | `Inbox.tsx`, `MissionInbox.tsx`, `compare-inbox.png` |
| OS notifications | A **sound** plus an **in-app toast** (unless you're looking) when an agent is blocked or done. System notifications and the Dock badge come **only from mods** (`notify-send` example; `setBadge`, "Herdr sets none by default"). heise says it "sends a system notification", which the shipped docs contradict ([AGENTS.md § settings](https://github.com/herdrdev/herdr-app/blob/master/defaults/AGENTS.md), [0.1.16](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)). | Native notifications through `tauri-plugin-notification`; clicking one routes to the conversation. Dock badge count, a "(N)" tab title on web, and bursts of more than 3 collapsed into one. Each kind is toggled in Settings → Notifications. Nothing fires for the conversation in a focused window. | **Lazybox** | `apps/desktop/src-tauri/src/attention.rs`, `apps/web/src/browser-host.ts:64-86`, `useAttention.ts:113-159`, `NotificationSettings.tsx` |
| Sound | Bundled `done.mp3` and `request.mp3`, replaceable, with Play buttons in Settings → Alerts. | None (grep: no audio anywhere). | **herdr** | [assets/sounds](https://github.com/herdrdev/herdr-app/tree/master/assets/sounds) |

### Talking to an agent

| Aspect | herdr app | Lazybox V2 | Ahead | Evidence |
|---|---|---|---|---|
| Surface | The agent's own TUI in a terminal pane, so nothing is lost. Paste or drag screenshots and files in, and Cmd+V pastes images ([0.1.10](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md), [0.1.15](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)). | A re-rendered chat: markdown, mermaid, copyable code, clickable file paths, images and attachments with lightbox, prompt navigator, queued follow-ups, Interrupt. | Lazybox for reading; herdr for fidelity | `MessageList.tsx`, `Composer.tsx`, `PromptNavigator.tsx` |
| Questions / approvals | Whatever the agent's TUI does (Claude Code's option pickers, "always allow"). | Question options are listed **as text** above a required free-text input. Approval offers **Decline / Allow once** only. | **herdr (by default)** | `session-parts.tsx:12-41` |
| Tool calls | Inline, as the agent prints them. | Hidden from the transcript, in an "Activity" dialog with search and filters. | herdr | `ToolActivity.tsx`, `transcript-model.ts` |
| Sub-agents | Helpers started with `hc run` show **under their parent** in the agents list. **Peek** (`ctrl/cmd-click` an address like `t7`) opens one over your tab, with "keep as tab" ([0.1.5](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)). | A collapsible "Background work · N running" line. Live sub-agent cards, a tray, a panel and stop are **in flight** (sub-agents branch `8606528d`). | herdr today; Lazybox after that branch | `SessionNotices.tsx:36-56` |
| Telemetry | None. | Context meter by category, compaction reserve, $ estimate, a Usage & cost panel, and subscription limit cards. | **Lazybox** | `TelemetryPanel.tsx`, `compare-settings.png` |

### Review, git, files, terminal, runtime

| Aspect | herdr app | Lazybox V2 | Ahead | Evidence |
|---|---|---|---|---|
| Changes | A right-hand panel: uncommitted files, a unified diff, a "Commit message" box and Commit. That is the whole mod. | Compare against uncommitted / workspace start / branch / any agent turn; diffs per repository; line and range **review comments** with freshness, sent to the agent as one message; commit (agent-drafted), push, create PR, checks, "send failing checks", merge. | **Lazybox, by far** | [changes/main.js](https://github.com/herdrdev/herdr-app/blob/master/defaults/mods/changes/main.js), `ChangesPanel.tsx`, `ReviewDiff.tsx`, `PullRequestFlow.tsx` |
| Diff view | Unified only. | Unified only (custom renderer; no side-by-side). | = | `ReviewDiff.tsx` |
| Checkpoints | None. | Per-turn markers with "Show changes in this turn" / "Revert files" and undo. | **Lazybox** | `TurnCheckpoints.tsx` |
| Files / editor | A Files panel (tree and preview) remembered per space. Drag files into a terminal to type their paths. No editor (a community herdr-nvim mod exists). | File tree with Go-to-file (Mod+P), and **Monaco** with tabs, 4 split groups, breadcrumbs, save/conflict handling, rendered markdown and opt-in vim. | **Lazybox** | `FileExplorer.tsx`, `EditorArea.tsx` |
| Terminal | Ghostty-derived: find (`cmd-f`), scroll thumb, Kitty graphics, ligatures, font weight, line height and cell width, IME, OSC 52 clipboard, copy-on-select, dropping files or screenshots, links. Terminals split freely. | xterm.js with **only the fit addon**: no search, no link handling, no splits, hard-coded dark theme. One tab per shell; shells outlive the Runner. | **herdr, by far** | `TerminalPanel.tsx:3,20`, [settings](https://github.com/herdrdev/herdr-app/blob/master/defaults/AGENTS.md) |
| Run / preview | No run or preview concept, and no browser (an HN commenter asks for one). | ▶ Run control with name · port · elapsed, Actions panel, sandboxed Preview iframe, script approval. | **Lazybox** | `RunControl.tsx`, `ActionsPanel.tsx`, `PreviewPanel.tsx` |
| Processes | `herdr-app ls` and the core keep terminals; no panel. | A Processes panel across workspaces: tmux attach, Stop, "Clean up orphans". | Lazybox | `ProcessesPanel.tsx` |
| Missions / board | A **Board** layout with columns Needs you / Working / Done / Idle (a demo layout; no trust model). | A Missions panel with columns Ready / Working / Needs you / Done, trust badges (claimed / observed / verified / accepted), evidence chips and gates. Acceptance happens in the inbox. | **Lazybox** | `MissionsPanel.tsx`, [guide](https://herdr.dev/guide/) |

### Keyboard, settings, look, lifecycle, remote

| Aspect | herdr app | Lazybox V2 | Ahead | Evidence |
|---|---|---|---|---|
| Command palette | `cmd-shift-p` covers every command. The same registry drives keys, menus, mods and `herdr-app do <command>`. | **None.** Mod+P opens files only. The `]]` which-key has 6 built-in entries plus user snippets. | **herdr** | [docs/commands.md](https://github.com/herdrdev/herdr-app/blob/master/docs/commands.md), `LeaderMenu.tsx`, `compare-which-key.png` |
| Bindings | `keys.json` with `global` / `layout:` / `terminal` / `mod:` sections. Applies live. Invalid keys produce an error toast. The **macOS menu bar** mirrors the bindings ([0.1.16](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)). There is a shortcuts overlay (`cmd-/`). | vim / arrows / tmux presets, fully rebindable with conflict checks (Settings → Shortcuts). The native menu has 3 items. There is no overlay listing all keys. | herdr (Lazybox's presets are a nice touch) | `keyboard.ts:55-81`, `KeyboardSettings.tsx`, `apps/desktop/src-tauri/src/host.rs:229-259` |
| Context menus | Right-click menus on terminals, tabs, spaces and agents. The terminal menu is a replaceable mod. | Only on workspace rows. A shared `ContextMenu.tsx` is **in flight** on the artifacts branch (`0ebd9356`). | herdr today | `WorkspaceRow.tsx:68` |
| Theme / fonts / zoom | Light/dark following the system, a theme picker, `themes/`, color overrides, UI and terminal fonts, app zoom (`cmd-=`/`cmd--`/`cmd-0`), minimum contrast. | **Dark only.** About 1,600 hard-coded hex values across the surfaces CSS and hardly any variables; Monaco, xterm and mermaid are pinned dark. No font or zoom setting. | **herdr** | `style.css:2`, `TerminalPanel.tsx:20` |
| Settings | A Settings window (Look, Machines, Alerts, Updates, Privacy, Keyboard) **and** the same plain files an agent can edit. | A Settings page with 10 tabs. Ask Lazybox can *propose* settings changes for approval. | = (different philosophies) | `SettingsPage.tsx`, `compare-settings.png` |
| Extensibility | Everything is a mod: JavaScript on GPUI, hot-reloaded, **unsandboxed**. The site claims 576 mods. Agents can edit the UI and check it with `herdr-app shot`. | None for users beyond layout JSON and `]]` entries. | herdr (at a security cost Lazybox should not copy) | [AGENTS.md § Mods](https://github.com/herdrdev/herdr-app/blob/master/defaults/AGENTS.md), [security](https://github.com/herdrdev/herdr-app/blob/master/docs/security.md) |
| Onboarding / updates | A welcome screen (including migration from the TUI), What's new after updates with media, a feedback form with screenshots, self-update. | No first-run flow. The empty states are decent: "What are we building?" and Start / Open terminal / Show changes. | **herdr** | `WorkspaceHome.tsx`, `MessageArea.tsx:62` |
| Persistence | Agents outlive the window, updates and crashes. After a reboot, panes come back with a shell in the same folder and **agents are not resumed** ([herdr.dev](https://herdr.dev), heise). HN has reports of lost sessions and empty panes ([HN](https://news.ycombinator.com/item?id=50298436)). | Agents and commands outlive Runner restarts (session hosts). Layout, pages, editor tabs, drafts and console scroll are saved per workspace (desktop `views-<scope>.json`). Behavior after a reboot was not verified here. | = (Lazybox also resumes the *conversation*; herdr resumes the terminal) | `apps/desktop/src-tauri/src/viewstate.rs`, `Workspace.tsx:65-104` |
| Remote machines | Add SSH machines in Settings or the sidebar. Their spaces and agents join the same lists, grouped by machine. | One Runner per window. | **herdr** | [README](https://github.com/herdrdev/herdr-app) |
| Phone | A native app: urgency inbox, swipe actions, the **real terminal** with a configurable key bar, compose with screenshots and photos, **on-device dictation**, a live mini-map of each tab's panes. Connects over SSH or Tailscale with no account. No push notifications are documented. | A web `RemoteApp` paired by **QR** over Tailscale: inbox, conversation, approval cards, composer, Interrupt. | herdr for richness; Lazybox for pairing ease | [mobile](https://herdr.dev/mobile), `remote/RemoteApp.tsx`, `DevicesSettings.tsx` |

## What herdr does notably better

- **The agents list is the product.** It is always visible, Priority-sorted,
  pinnable and foldable, and spans spaces and machines. It works with any
  CLI. On HN, the "agentic inbox" and the "visual indication of waiting
  agents" were the most cited reasons people switched. Lazybox hides the
  equivalent behind a modal and shows each workspace's agent as
  "Claude · cla…".
- **One command list for everything.** The palette, keys, macOS menu,
  right-click menus, mods and `herdr-app do` cannot drift apart because they
  are one list. Lazybox's actions live in buttons, a 6-key `]]` menu and a
  3-item native menu.
- **Panes.** Drag-swap, edge docking with a highlight, zoom, keyboard
  swap/go/resize, tabs per task, and settle. Lazybox has docking but none of
  the rest.
- **Polish cadence.** 13 releases in about 3.5 weeks. The changelogs are
  mostly small UX fixes: tab-bar scrolling one tab at a time so you don't
  type to the wrong agent; a selection that doesn't bleed; IME on every
  platform. Release notes appear in the app.
- **Look and comfort.** Light and dark, themes, zoom and fonts. Lazybox is
  dark-only with hard-coded colors, which blocks theming and accessibility.
- **Agents can see and change the UI.** `herdr-app shot` gives a PNG of the
  window, and every file is documented for agents in a generated
  `AGENTS.md`. The Lazybox equivalent (Ask Lazybox proposing settings) is
  narrower.

## What Lazybox does notably better

- **Everything after "done":** the Changes compare modes, review comments
  with freshness, the PR loop with checks and merge, per-turn checkpoints
  with revert, and Monaco. herdr's whole review story is uncommitted files
  plus a commit box. On HN, "none of these tools offer code-change
  capability properly".
- **Trust:** missions with claimed/observed/verified/accepted, evidence
  chips, and acceptance in the inbox. herdr's Board is a status display.
- **Real notifications by default:** native OS notifications, a dock badge,
  a title count, per-kind toggles and click-to-route. herdr needs a
  community mod for any of that.
- **Approvals, permissions and cost as UI:** live permission changes,
  subscription limits and rotation, a context meter, and $ per turn. herdr
  has none, because it can't see inside the agent.
- **Run and Preview with ports and script approval.** herdr has no runtime
  concept.
- **Multi-repo workspaces and detached windows.**
- **Safety:** no unsandboxed UI code, and no default-on telemetry. Both are
  points of HN criticism of herdr.

## What not to copy

- **Unsandboxed JavaScript mods.** The power is real, but it is trusted code
  with the user's permissions (herdr's own
  [security.md](https://github.com/herdrdev/herdr-app/blob/master/docs/security.md)).
  If Lazybox wants this, it should be declarative layout and menu files
  plus the settings MCP, not arbitrary code. That is now
  [POWER.md](../../POWER.md): actions and hooks in one declarative file,
  with hooks holding no credential unless granted a phone-sized one.
- **Joke layouts** (Herdr 98, Pasture). They are fun marketing and have no
  place on the roadmap.
- **Replacing the chat with raw terminals.** Lazybox's thesis is
  understanding and verifying work, which a terminal grid can't do. Offer the
  terminal as an *escape hatch* (see P1-6), not as the main surface.

## Prioritized gaps

Effort: **S** ≤ 1 day, **M** 2–4 days, **L** 1–2 weeks. "Owner" is the area
or in-flight agent the change belongs to. Where a branch already covers a
change, it is noted as an overlap, not a duplicate.

### P0 — the daily loop with more than three agents

| # | Change | Why (herdr evidence) | Effort | Owner / overlap |
|---|---|---|---|---|
| P0-1 | **An always-visible Agents section in the sidebar**, across workspaces. Toggle *Priority* (needs input > awaiting acceptance > failed > review > working > idle) or *By workspace*. Each row: task title, workspace, live status and age, and an unread dot. Hover pin; fold headers with counts. The Inbox dialog stays as the detailed view. | herdr's agents mod ([0.1.4](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)) is the most praised feature on [HN](https://news.ycombinator.com/item?id=50298436). Lazybox's attention model (`attention.ts`) already computes it all. | M | Layout upgrade / sidebar; reuses `useAttention`. Coordinate with **missions** (awaiting acceptance) and **sub-agent cards** (children nest under parent). |
| P0-2 | **Command palette** (`Mod+Shift+P`) over a single action registry, plus a **shortcuts overlay** (`Mod+/`) generated from it. Every header button, ⋯ menu item, `]]` entry and native-menu item becomes a registered action, so they can't drift. | [commands.md](https://github.com/herdrdev/herdr-app/blob/master/docs/commands.md), [keys.macos.json](https://github.com/herdrdev/herdr-app/blob/master/defaults/keys.macos.json). Lazybox has no palette (grep). | M | Keyboard (`keyboard.ts`). **Context menus** (artifacts branch `ContextMenu.tsx`) should consume the same registry: land the registry first, or together. |
| P0-3 | **Chat at TUI parity for decisions.** Question options become buttons (single or multi-select, with "Other" as free text). Approval gets "Always allow this command/tool" when the provider supports it. Tool calls appear inline as one collapsed line each (name · target · duration · status), with the Activity dialog kept for depth. | herdr users see Claude Code's own pickers. Lazybox: `session-parts.tsx:36` renders options as text, and `ToolActivity.tsx` hides tools from the transcript. | M | Chat. Overlaps **sub-agent cards** (their timelines show tool lines), so use one tool-line component for both. |
| P0-4 | **Global "next agent that needs me"** (`Mod+]` / `Mod+[`, following P0-1's order) and **workspace select** `Ctrl+1..9`. | `agent.next`/`agent.prev` and `space.select:N` ([keys.macos.json](https://github.com/herdrdev/herdr-app/blob/master/defaults/keys.macos.json)). Lazybox's j/k only work inside the focused sidebar. | S | Keyboard |

### P1 — comfort and density

| # | Change | Why | Effort | Owner / overlap |
|---|---|---|---|---|
| P1-1 | **Zoom a panel** (`Mod+Shift+Enter`, a header button, and double-click the header). Navigation keeps the zoom. | `pane.zoom` ([0.1.16](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)). Lazybox has none. | S | Layout upgrade |
| P1-2 | **Tabbed panel stacks:** drop a panel onto another panel's center to stack them as tabs (two chats, or Changes + Files). Also **drag-to-swap**, and keyboard focus-move and swap (`Mod+Alt+arrows` / `+Shift`). | herdr pane drag and swap ([0.1.5](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)). Lazybox panels never stack (`workspace-model.ts`). | L | Layout upgrade (no commit found yet; this is its core) |
| P1-3 | ~~Terminal upgrade~~ | **Dropped by decision:** V2 is not a terminal emulator (below). | — | — |
| P1-4 | **Sounds** for needs input / failed / ready, on by default but quiet, with choice and preview in Settings → Notifications; plus an **in-app toast** for an attention change in another workspace when the window is focused. | `sounds` setting, Settings → Alerts ([AGENTS.md](https://github.com/herdrdev/herdr-app/blob/master/defaults/AGENTS.md)). Lazybox has no audio. | S | Notifications (`NotificationSettings.tsx`) |
| P1-5 | **Workspace rows carry git state:** branch (or "N repos"), ahead/behind, dirty count, PR/check state. Fix the agent line truncating to "C." by showing the task title, not provider · model. | The spaces mod shows "repo, branch, ahead/behind and uncommitted changes". Lazybox: `compare-workspace.png`. | S | Sidebar; data already in the PR loop's ship state |
| P1-6 | **Peek:** Alt-click a conversation, sub-agent or process anywhere (inbox row, Background work, Processes, a mention in chat) to open it in an overlay over the current page, with "Open here" or "Open beside". This also gives an "open the raw terminal" escape hatch for a session-host agent. | herdr peek ([0.1.5](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)). | M | **Sub-agent cards** branch (`8606528d`) owns sub-agent peek; generalize it, don't duplicate it. |
| P1-7 | **Light theme and theme tokens:** move the hard-coded hex values in `style.css` to CSS variables; `prefers-color-scheme` plus a Settings → Appearance with Light / Dark / System, UI zoom (`Mod+=` / `Mod+-` / `Mod+0`) and terminal and editor font size. | herdr themes, appearance and zoom ([AGENTS.md § settings](https://github.com/herdrdev/herdr-app/blob/master/defaults/AGENTS.md)). Lazybox: `style.css:2` `color-scheme:dark`. | L (token sweep), M for zoom/fonts alone | **Settings polish** (`b89cd8bb`, WIP); sequence after it, to avoid conflicts in `style.css`. **Update:** implemented, beyond this scope (theme picker, user theme files, accent, overrides, fonts, chat text, density, persistent desktop zoom, minimum contrast): [Appearance](../../APPEARANCE.md). |
| P1-8 | **Context menus on every object** (conversation, workspace, panel header, file, diff file, terminal tab, inbox row), fed by the P0-2 action registry. | herdr has right-click menus everywhere, with the terminal menu as a mod. | M | **Artifacts** branch (`0ebd9356`) introduces `ContextMenu.tsx`; extend that. |
| P1-9 | **Native menu bar mirrors the actions and shows the user's bindings** (File / Edit / View / Window / Help), rebuilt live when keys change. | [0.1.16](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md). Lazybox has 3 items (`host.rs:229-259`). | M | Desktop shell; after P0-2 |

### P2 — later or strategic

| # | Change | Why | Effort | Owner / overlap |
|---|---|---|---|---|
| P2-1 | **Welcome and What's new:** a first-run sheet (connect a repo, start a conversation, pair a phone, pick a keyboard preset) and an in-app release-notes view after updates, with Settings → Updates. | herdr `frame/welcome` and `whatsnew` ([0.1.5, 0.1.6](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)). | M | Settings polish / desktop shell |
| P2-2 | **Several Runners in one window**, grouped by machine in the sidebar (Agents and Workspaces). | herdr machines ([README](https://github.com/herdrdev/herdr-app)). The audit lists remote Runners as missing. | L | Remote ([REMOTE.md](../../REMOTE.md)). **Designed** in [MULTI_RUNNER.md](../../MULTI_RUNNER.md); step 1 (`lazybox runners`, `ls --all`) done. |
| P2-3 | **Phone richness:** swipe actions (pin / mark seen / interrupt), **dictation** in the composer, a live view of a command's output, and push notifications when the app is closed (herdr documents only in-app banners, so this would be a lead, not catch-up). | [mobile](https://herdr.dev/mobile) | M–L | Phone (`remote/RemoteApp.tsx`) |
| P2-4 | **Layout presets** ("Focus", "Review", "Monitor: Agents + Missions + Processes") switchable with a key, stored as files the Ask Lazybox agent can edit through the settings MCP. | `layout.switch` and layout files. Declarative, no code mods. | M | Layout upgrade, after P1-2 |
| P2-5 | **`lazybox shot`:** a CLI or MCP tool that returns a PNG of the current window, so UI agents (and Ask Lazybox) can check what they changed. | `herdr-app shot` ([0.1.5](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)). | S–M | **Done** ([POWER.md](../../POWER.md#the-cli-and-the-open-window)): owner-only `lazybox shot [PANEL]`. |
| P2-6 | **Side-by-side diff** toggle in Changes (Monaco diff editor is already a dependency). | Neither has one. This is about staying ahead of Conductor, not herdr. | M | Changes |
| P2-7 | ~~More harnesses via terminals~~ (dropped: no terminal emulator) **More harnesses visible in the sidebar:** adopt OSC 7501 so any CLI in a Lazybox terminal reports `working/blocked/done` and appears in the Agents list as a "terminal agent" (no chat, but attention works). | It is an open protocol; herdr's coverage comes from it. | M | Terminal + attention; complements the audit's "more harnesses" item |

## Top 10, in one line each

1. **P0-1 Agents list always visible, urgency-sorted, pinnable.** herdr's most praised feature; Lazybox hides it in a modal ([0.1.4](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md), [HN](https://news.ycombinator.com/item?id=50298436)).
2. **P0-2 Command palette plus shortcuts overlay over one action registry.** herdr runs keys, menus, mods and CLI from one list; Lazybox has no palette ([commands.md](https://github.com/herdrdev/herdr-app/blob/master/docs/commands.md)).
3. **P0-3 Clickable question options, "always allow", inline tool lines.** herdr users see the agent's own TUI; Lazybox's chat is below it (`session-parts.tsx:36`).
4. **P0-4 Next/prev agent needing you, and `Ctrl+1..9` workspaces.** Two keys herdr ships by default ([keys.macos.json](https://github.com/herdrdev/herdr-app/blob/master/defaults/keys.macos.json)).
5. **P1-1 / P1-2 Panel zoom, tab stacks, drag-to-swap.** herdr's pane handling ([0.1.5](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md), [0.1.16](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md)).
6. ~~P1-3 Terminal upgrade~~ — dropped: V2 is not a terminal emulator.
7. **P1-4 Sounds and in-app toasts.** herdr's bundled done/request sounds; Lazybox is silent.
8. **P1-5 Branch, ahead/behind and dirty state on workspace rows; no more "C." truncation.** herdr's spaces mod.
9. **P1-7 Light theme, tokens, zoom and fonts.** Lazybox is hard-coded dark; herdr follows the system.
10. **P1-6 Peek a conversation or sub-agent without navigating.** herdr peek; extend the sub-agent cards branch rather than duplicating it.

## Re-audit (evening)

This section replaces the "scorecard after round one" that stood here. That
scorecard marked gap items done without asking whether each row was now
*won*. This pass asks exactly that, row by row, against
`feat/v2-command-output` at `88672e29`.

**Method.**

- **Lazybox.** Every claim was re-checked in the code on this branch, with a file:line for each.
- **Screenshots.** They come from the real web client against the fake Runner (`scripts/fixtures/runner-ui.mjs` plus `apps/web` through Vite), in dark and light, at 1440×900 and 1024×680. They are in `.cache/reaudit-<shot>-<scheme>-<width>.png`, which is ignored and not committed. The script that made them is `.cache/reaudit-shots.mjs`: a copy of `scripts/docs-screenshots.mjs` with four broken shots repaired (see D10).
- **herdr facts, re-fetched today:**
  - herdr-app is still at 0.1.16. Its master has unreleased work: `pane.resize` (#1087), `pane.rename` (#1086), and a focused-row marker in the agents rail (#969).
  - herdr-core (`hc`) is at 0.1.21.
  - The iOS app is at 1.0.6 (2026-11-11).
  - The TUI has not changed since 0.9.5.

Sources: [Sources](#sources), plus the
[herdr-core changelog](https://raw.githubusercontent.com/herdrdev/herdr-core/master/CHANGELOG.md),
[herdr-core commands](https://raw.githubusercontent.com/herdrdev/herdr-core/master/docs/commands.md),
the [App Store page](https://apps.apple.com/us/app/herdr/id6828898364) and the
[mobile launch thread](https://news.ycombinator.com/item?id=50442203).

Caveats: this is still the fake Runner, not daily use in the desktop app; herdr was not installed.

### Result: 21 Lazybox, 11 herdr, 12 tie (44 rows)

That count is not acceptable, and it is kinder than the truth underneath it.
Several Lazybox "wins" rest on features a person does not see in the
default window. The worst case: **the Agents list ships hidden.** Every
built-in layout puts Agents in the *same tab group* as Workspaces
(`layout-presets.ts:18-22`, `g('collections', 'agents')`). The first screen
shows Workspaces and a clipped tab that reads "Agent⌄1"
(`reaudit-fleet-default-dark-1440.png`).

herdr's lead is concentrated in three places, and the fix briefs target
them:

1. **An agent can drive agents.** herdr has `hc run --bg`, `send`, `wait --children` and parent notes. In Lazybox only the person can do this; agents have `lazybox ask`, and only between a review's author and its reviewer.
2. **Reach.** Any CLI agent, several machines in one window, a native phone app.
3. **Power-user plumbing.** Every setting is a file, every command runs from the CLI (`herdr-app do`), and panes have keyboard operations.

### Row by row

The **Winner** column is today's winner, and **Fix** points to a brief
below. Lazybox paths are relative to `packages/surfaces/src/` unless they
say otherwise.

| # | Row | Winner | Evidence | Fix |
|---|---|---|---|---|
| 1 | Model | tie | Lazybox has multi-repo scope (`workspace-model.ts`); herdr has one-noun simplicity. Starting a task in Lazybox takes a workspace first, then a conversation (`NewConversationDialog.tsx`, `WorkspaceHome.tsx`). | R11 |
| 2 | Moving panes | tie | Lazybox leads with the mouse: dockview edge/middle drop, float, own window (`DockLayout.tsx`). herdr leads on the keyboard: `pane.go`, `pane.swap` and now `pane.resize`. Lazybox has no directional focus, move or resize from the keyboard; `Workspace.tsx:481-490` is the full list of layout commands. | R6 |
| 3 | Zoom a pane | tie | Lazybox has `layout.maximize` (⌘⇧M, or double-click a tab; `DockLayout.tsx:335,448`). In herdr, navigation follows the zoom and the zoomed pane takes focus (0.1.16); Lazybox has no such rule. | R6 |
| 4 | Tabs | tie | Lazybox has tab groups, but the tab strip is clipped with a scroll bar ("orkspaces", "Agent⌄1"; `reaudit-agents-panel-*-1024.png`). A group has no `+` to start a conversation and no per-group number keys. herdr: `+` starts any agent, F2 renames, ⌘1–9, `tab.move`. | R1, R6 |
| 5 | Whole-window layouts | **herdr** | Lazybox has 5 presets, "Save current layout as…" and Reset (`reaudit-layout-menu-dark-1440.png`). Layouts live in client view state, not files. There is no cycle key (`layout.presets` has no default key) and no way to share one. herdr: layout files, `cmd-shift-l` to cycle, a community gallery. | R7 |
| 6 | Escape hatch | tie | Lazybox: "Reset to default" in the layout menu and `layout.reset`; keys are reset separately in Settings. herdr: one chord, which can't be rebound, restores both layout and keys. | R6 |
| 7 | Secondary windows | **Lazybox** | A conversation, a panel or a whole workspace can open in its own window (`host.rs`, `placement.rs`, `layout.window`). | — |
| 8 | Sidebar structure | **herdr** | In every preset, Agents is a tab behind Workspaces (`layout-presets.ts:18-22`). Workspace rows list conversations only under the selected workspace. Every conversation row repeats "Claude · claude-opus-5" and shows no status or age (`reaudit-fleet-default-dark-1440.png`). herdr stacks Spaces over Agents, both visible. | R1 |
| 9 | Sorting, pin, fold | tie | Priority / By workspace, pin and fold exist (`AgentsPanel.tsx:26-43`). In the screenshot, though, the pin button covers each row's age, and its icon is a crosshair (`AgentsPanel.tsx:80-82`, `agents-panel.css:26-28`; `reaudit-crop-agents.png`). Pins are kept per device and don't reach the phone; herdr shares pins with its phone. | R1 |
| 10 | States | **Lazybox** | Five live tones, ageing live text, unread marks for review and failed (`live-status.ts`, `attention.ts`). A workspace row rolls up its most urgent conversation ("Bump depende… +2 · Waiting for…"). Missions add trust levels. | — |
| 11 | Coverage (agent CLIs) | **herdr** | Lazybox runs only Claude and Codex (`services/runner/code/internal/runner/adapters/{claude,codex}`); herdr detects 22+ CLIs. herdr's terminal route is out of scope. ACP is not a terminal, so it is in scope. | R3 |
| 12 | Next/prev agent | **Lazybox** | `attention.next` (⌘J) goes to the next agent that needs you, which herdr lacks. Also ⌘] / ⌘[ and ⌘1–9 for workspaces (`actions.ts:66`). | — |
| 13 | Inbox | tie | The content is richer than herdr's: Awaiting acceptance / Needs input / Failed / Done, reasons, j/k/m (`reaudit-inbox-dark-1440.png`). But the always-visible form of it (row 8) ships hidden, and the Inbox is a popover over the chat. | R1 |
| 14 | OS notifications | **Lazybox** | Native notifications, a dock badge, a title count, burst folding and per-kind toggles (`attention-alerts.ts`, `NotificationSettings.tsx`). herdr has a sound and a toast; native notifications are its open issue #120. | — |
| 15 | Sound | tie | Two synthesized sounds with Play and on/off (`sounds.ts:1-17`, `NotificationSettings.tsx:16-51`). They can't be replaced and have no volume control. herdr ships replaceable sound files; Conductor added custom sounds in 0.93. | R14 |
| 16 | Chat surface | tie | For reading, it beats a TUI: grouped tool lines, the plan, a turn summary (`reaudit-hero-dark-1440.png`). For fidelity it doesn't: a provider feature Lazybox doesn't map (for example `/mcp`, plugins, the agent's own plan mode) is unreachable, and there is no way to continue in the agent's own CLI. | R12 |
| 17 | Questions / approvals | **Lazybox** | Question cards with options 1–9 and Other (`reaudit-chat-decisions-dark-1440.png`). Approvals offer Allow once, Deny with a reason, and Always when the provider offers it (`request-model.ts:21,112,153`), from the Inbox or `lazybox approve`. In herdr you type into the TUI or use `hc send --force`. | — |
| 18 | Tool calls | **Lazybox** | Inline grouped lines ("Ran tests and explored the codebase · 1 command, 4 files read"), Smart / Quiet / Verbose, and an Activity detail view (`ToolLine.tsx`, `activity-detail.ts`). | — |
| 19 | Sub-agents | **Lazybox** | Live cards with tools, tokens and stop, a side panel and a tray (`reaudit-subagents-dark-1440.png`). Weakness: sub-agents don't appear under their parent in the Agents list, as herdr's kept sub-agents do. That is folded into R1. | (R1) |
| 20 | Telemetry | **Lazybox** | Context meter, $ per turn, a Usage panel, subscription limits; herdr has none. Polish: before any data arrives, the "Context" meter is an empty grey bar (in every screenshot). | (R12) |
| 21 | Changes | **Lazybox** | Compare modes, review comments with freshness, the PR loop (`ChangesPanel.tsx`, `reaudit-changes-review-*`). herdr has `git add -A` plus commit. At 1024 px the Review tray leaves the diff about 180 px (`reaudit-changes-review-dark-1024.png`). | (R12) |
| 22 | Diff view | **Lazybox** | Unified or Split, word highlights, folding, n/p, Viewed (`SplitDiff.tsx`, `diff-layout.ts`). | — |
| 23 | Checkpoints | **Lazybox** | Per-turn revert with undo (`TurnCheckpoints.tsx`). | — |
| 24 | Files / editor | **Lazybox** | Monaco, 4 editor groups, opt-in vim (`EditorArea.tsx`). | — |
| 25 | Terminal | **herdr**, *out of scope* | No terminal emulator, by decision. The Lazybox-native answer is R13: open the person's own terminal at the worktree, already set up so `lazybox run` / `log` feed the console. | R13 |
| 26 | Run / preview | **Lazybox** | `RunControl.tsx`, `PreviewPanel.tsx`, port blocks. | — |
| 27 | Processes | **Lazybox** | `ProcessesPanel.tsx`. | — |
| 28 | Missions / board | **Lazybox** (on substance) | Trust levels and evidence chips (`reaudit-missions-dark-1440.png`). The UI is weak: in a side panel the four columns stack into one long list, a *Blocked* task sits under *Ready*, and every card has the same loud "Assign…" button. Fixed inside R2. | (R2) |
| 29 | Command palette | tie | The palette is richer than herdr's: `>` `@` `#` `/` modes, voice, user actions (`reaudit-command-palette-dark-1440.png`). But herdr's registry also drives its CLI (`herdr-app do <command>`). `lazybox actions run` runs only the person's own actions (`PowerLayer.tsx:141-143`), and an action step can't call a built-in one. | R5 |
| 30 | Bindings | tie | Presets, a settings editor and conflict checks (`keyboard.ts`). The keymap lives in view state (`keyboard.ts:51`), not in a file that an agent or a dotfiles repo can edit. herdr has `keys.json` with `global` / `layout` / `terminal` / `mod` sections, applied live. | R4 |
| 31 | Context menus | tie | Menus exist on 28 surfaces. But the most-used object, a conversation row, offers only Open, Delete and Peek, with Delete right under Open (`ConversationList.tsx:62-66`; `reaudit-row-menu-dark-1440.png`). | R1 |
| 32 | Theme / fonts / zoom | **Lazybox** | Light / Dark / System; 4 themes plus theme files that hot-reload; accent; overrides; minimum contrast; zoom ([APPEARANCE.md](../../APPEARANCE.md)). Light verified (`reaudit-*-light-*.png`). | — |
| 33 | Settings | **herdr** (for power users) | Every herdr setting is a plain file an agent can edit. In Lazybox, actions and hooks, themes and the process are files, but keys, layouts, notifications and appearance are view state. | R4 |
| 34 | Extensibility | **herdr**, at a cost | herdr claims 576 mods. Lazybox has declarative actions and hooks only ([POWER.md](../../POWER.md)). Unsandboxed mods are ruled out by decision; the Lazybox-native alternative is R10. | R10 |
| 35 | Onboarding / updates | **herdr** | Lazybox has a welcome and What's new (`Onboarding.tsx`, `release-notes.ts`), but no self-update, no feedback form and no release. herdr updates itself, although its *forced* update is an HN complaint ([#207](https://github.com/herdrdev/herdr-app/issues/207)). | R9 |
| 36 | Persistence | **Lazybox** | Agents and commands outlive the Runner, and the conversation resumes. Lost agents after a reboot are herdr's top complaint ([#406](https://github.com/herdrdev/herdr-app/issues/406), [#429](https://github.com/herdrdev/herdr-app/issues/429)). Lazybox after a reboot is still unverified, so that check is part of R9's acceptance. | — |
| 37 | Remote machines | **herdr** | Lazybox is one Runner per window; [MULTI_RUNNER](../../MULTI_RUNNER.md) steps 2–3 are still open. | R8 |
| 38 | Phone | **herdr** | herdr has a native app (iOS 1.0.6, Android); Lazybox has the web RemoteApp. Its Inbox rows can't approve, there is no swipe and no push (`reaudit-phone-inbox-dark-1440.png`, `remote/RemoteApp.tsx`). herdr has no push either, which leaves room to lead. | R7p |
| 39 | **Agent drives agents** (new row) | **herdr** | herdr: `hc run --bg`, `send --wait`, `wait --children`, `read`, parent notes, templates ([commands](https://raw.githubusercontent.com/herdrdev/herdr-core/master/docs/commands.md)). A Lazybox agent can call only its own console, commands, artifacts, repository request and review verbs (`services/runner/code/internal/transport/auth.go:49-100`), and `lazybox ask` works only between one review's author and its reviewer. | R2 |
| 40 | Browser (new) | **Lazybox** | An agent browser you can watch and take over, plus the Web panel. herdr has none (on HN, users asked for one). | — |
| 41 | PR loop and CI (new) | **Lazybox** | Commit, push, PR, checks, failing checks back to the agent, merge. herdr has none. | — |
| 42 | Cost and limits (new) | **Lazybox** | Metering, account rotation, limits. herdr has none. | — |
| 43 | Privacy (new) | **Lazybox** | No telemetry and no forced update. herdr has telemetry on by default ("7000 pings in 3 days", [HN](https://news.ycombinator.com/item?id=50352494)). | — |
| 44 | Shipping (new) | **herdr** | herdr is public and ships weekly; V2 has no release yet. | R9 |

### What a demanding designer sees in the screenshots

These defects come from the screenshots, separately from the rows. A herdr
user would notice each one in the first minute.

| # | Defect | Where | Fix |
|---|---|---|---|
| D1 | The Agents list is hidden behind a tab. The left tab strip is clipped ("orkspaces"), with a visible scroll bar under it. | `reaudit-fleet-default-*`, `reaudit-agents-panel-*` | R1 |
| D2 | Agent rows: the pin button covers the age ("now", "5m"). The meta line has no separator ("Release 2.4  Approval"). The panel background stops at the last row, so a band ends partway down the column (clear in light). | `reaudit-crop-agents.png`, `reaudit-agents-panel-light-1024.png` | R1 |
| D3 | Conversation rows repeat provider · model but show no state or age. The workspace's own agent line truncates to "Bump depende… +2 · Waiting for…". | `reaudit-fleet-default-dark-1440.png` | R1 |
| D4 | The conversation menu offers Open, Delete… and Peek, with the destructive item second. There is no rename, pin, mark seen, interrupt, open beside, review with… or add to mission. | `reaudit-row-menu-dark-1440.png` | R1 |
| D5 | The chat header shows an empty grey "Context" bar before any usage. The workspace header repeats the chat title, truncated ("Add a response cache · Claude · alp…"). | every workspace shot | R12 |
| D6 | The Inbox is a popover that covers the conversation. | `reaudit-inbox-dark-1440.png` | R1 |
| D7 | Missions in a side panel: the board collapses to one long column, the Blocked task sits under Ready, the mission picker is a native `<select>`, and every card has a full primary "Assign…" button. | `reaudit-missions-*` | R2 |
| D8 | Changes at 1024 px: the Review tray takes half the height, leaving the diff about 180 px. | `reaudit-changes-review-dark-1024.png` | R12 |
| D9 | The phone Inbox shows a request but can't act on it from the row. | `reaudit-phone-inbox-dark-1440.png` | R7p |
| D10 | `scripts/docs-screenshots.mjs` is broken against the dock: `showPanel` opens "Page actions", which no longer lists panels, so `hero`, `missions`, `workspace-runtime` and `dock-panels` fail. The docs images can't be regenerated. | first-run `.cache/reaudit-failed-*` | R15 |

### Fix briefs, by impact

Effort: **S** ≤ 1 day, **M** 2–4 days, **L** 1–2 weeks. Each brief says what
to build, its UX, and an acceptance test that fails today.

**R1: Agents always on screen, and the conversation row as a first-class object (P0, M).**
Covers rows 8, 9, 13, 31 and part of 4, and defects D1–D4 and D6.

- **Left column.** Every preset's left column becomes a *vertical split*: Workspaces on top, Agents below at 45 %. Both stay resizable, closable and movable.
- **Migration.** A saved layout keeps its arrangement but gains the split if Agents was tabbed with Workspaces.
- **Agent rows.**
  - Each row shows the title, a `workspace · reason` meta line with a separator, and the age right-aligned.
  - Pin and seen appear on hover or focus. They *replace* the age in its slot and never overlay it. The pin icon is a pin.
  - Sub-agents that are still running nest one level under their parent, folded.
- **Workspace rows.** Each lists *all* its conversations, one line each: status mark, title, age. Provider · model moves to the tooltip. Folding is per workspace.
- **Tab strip.** No horizontal scroll bar. Overflow goes into the existing chevron menu, with full titles.
- **Inbox.** It opens as the Agents panel, focused on its first row. The popover remains only when the Agents panel is closed.
- **Conversation context menu**, in this order:
  1. Open · Open beside · Open in new window · Peek
  2. Rename (F2) · Pin · Mark seen
  3. Interrupt · Review with Codex/Claude · Add to mission… · Copy link
  4. Delete…
- **Pins** move to Runner preferences, so the phone shows them.

Acceptance, in a new `test-agents-visible.mjs`:

- On a fresh profile at 1440×900 *and* 1024×680, the Workspaces and Agents regions are both visible without a click.
- No `.agent-age` box intersects an `.agent-pin` box while nothing is hovered.
- `.dv-tabs-container` has `scrollWidth <= clientWidth`.
- Right-clicking a conversation lists the items above in order, with Delete last.
- A pin made on the desktop shows in the phone Inbox.

**R2: Orchestrator, an agent that runs a mission's agents under a grant (P0, L).**
Covers row 39 and defect D7. The design is in
[sota-agents.md](sota-agents.md#6-a-coordination-design-for-lazybox).

- **The grant** is scoped to one mission.
  - It may create task workspaces and conversations inside the mission's workspaces.
  - It may drive only the agents it started: send, ask, wait, read, interrupt, stop.
  - It may claim and release path leases.
  - It may never approve, answer a permission request, pass a gate, accept a task, change any permission, or raise a budget or depth.
- **Before any spawn,** the person sees one plan-approval sheet with the fan-out and the $ (Warp's pattern).
- **The Missions board** becomes a real board in a main-area panel, with Messages and Timeline tabs.
- **CLI:** `lazybox agents spawn|send|ask|wait|read|stop|ls`, `lazybox mission plan|approve` and `lazybox lease`.

Acceptance:

- `TestOrchestratorGrantWalksEveryRPC`: the grant reaches only its verbs, only for conversations in its lineage, and never any `Respond*`, gate or accept RPC.
- A Playwright mission run:
  1. The orchestrator proposes three tasks.
  2. The person approves with a $ cap.
  3. Three workers start, each in its own workspace, and the board moves.
  4. An approval request from a worker lands in the *person's* Inbox, not the orchestrator's.

**R3: Any CLI agent through ACP, not terminals (P1, L).** Covers row 11.

- **Adapter.** Add a third Runner adapter, `adapters/acp`, that speaks the Agent Client Protocol over stdio to any registered ACP agent. Native ACP agents include Gemini CLI, Goose, OpenCode, Kimi, Qwen, Cursor CLI, Copilot, Droid, Cline and Kiro ([ACP agents](https://agentclientprotocol.com/overview/agents)).
- **Mapping onto the existing session journal:**

  | ACP message | Lazybox UI |
  |---|---|
  | `session/request_permission` (allow/reject, once/always) | The approval cards |
  | `plan` | The plan block |
  | `tool_call`, with diff or terminal content | Tool lines |
  | `usage_update` | Cost |
  | `available_commands_update` | Slash commands |

- **Files and commands.** Lazybox serves `fs/*` from the workspace with the conversation's permission. Terminals go through `lazybox run`, so every command reaches the console.
- **New conversation** lists the ACP agents it detects, with their icons.
- **Gaps.** Where ACP lacks a feature, Lazybox falls back to what ACP offers, never to a terminal. Claude and Codex keep their native adapters.

Acceptance:

- A fake ACP agent in `scripts/fixtures` runs a turn with a permission request, a plan, an edit diff and a terminal command.
- The conversation shows the approval card, the plan, the diff in Changes and the command in the console.
- `go test ./internal/runner/adapters/acp` covers the protocol.

**R4: Settings as files for keys, layouts, notifications and appearance (P1, M).** Covers rows 30 and 33.

- **Files:** `~/.config/lazybox/{keys,layouts,settings}.yaml`, beside `actions.yaml`.
- **One loader,** generalized from `internal/power`: a schema, hot reload every second, the last good version kept on a bad save, errors reported by line.
- **The Settings UI** reads and writes the files and keeps their comments.
- **Keys** have sections `global`, `navigation`, `agent` and `editor`.
- **CLI:** `lazybox config path|validate|schema`.
- **Ask Lazybox** edits the files through the settings MCP, confirmed per change as today.

Acceptance:

- After an edit to `keys.yaml` in a text editor, the new binding works within 2 s in an open window, with no reload.
- A wrong key name is shown in Settings with its line.
- Settings → Shortcuts writes the file, and the comments survive.

**R5: One command plane: `lazybox do` and `do:` steps (P1, S–M).** Covers row 29.

- `lazybox do <action-id> [--workspace W] [--conversation C]` runs any registry action in the open window, through the existing UI request broker.
- `lazybox do --list` prints the registry with its keys.
- `actions.yaml` gains a `do: <action-id>` step.
- A hook with the `respond` grant may `do` only actions marked safe for devices (no settings, no deletes).

Acceptance:

- `lazybox do layout.maximize` maximizes the active panel.
- An action with the steps `[do: changes.show, run: test]` works.
- `lazybox do settings.open` with a hook's credential is refused.

**R6: Pane keyboard operations and a safe-reset chord (P1, S–M).** Covers rows 2, 3, 4 and 6.

- **New actions:**

  | Keys | Action |
  |---|---|
  | ⌘⌥ arrows | `layout.focus-{left,right,up,down}` |
  | ⌘⌥⇧ arrows | `layout.move-*`: move the active panel to the neighbouring group, or swap with it |
  | ⌃⌘ arrows | `layout.resize-*`, in 5 % steps |
  | ⌃1–9 | Select that tab in the focused group |

- **Tab strip.** It gets a `+` that starts a conversation in that group.
- **While a panel is maximized,** ⌘J, ⌘] and opening a conversation keep the zoom and change what it shows.
- **⌃⌥⇧⎋** restores the default layout *and* the default keymap. It can't be rebound, and it shows an undo toast.

Acceptance, a keyboard-only Playwright run:

- It focuses each neighbour, swaps two panels and resizes one by three steps.
- ⌃2 reaches tab 2.
- The chord resets everything, and Undo restores it.

**R7: Layouts you can share, and a cycle key (P2, M).** Covers row 5.

- Saved layouts are files in `~/.config/lazybox/layouts/*.yaml`, read through R4's loader.
- `layout.cycle` (⌘⇧L) cycles through them.
- "Copy layout" puts the YAML on the clipboard. Pasting layout YAML into the palette offers "Import layout".

Acceptance:

- A file dropped into the folder appears in the Layout menu within 2 s.
- ⌘⇧L cycles through all of them.
- An invalid file is reported by line.

**R7p: A phone that acts, with push (P1, M–L).** Covers row 38 and defect D9.

- **Inbox rows act.**
  - An approval row has Allow and Deny.
  - A question row shows its options as chips.
  - Swipe right to pin, swipe left to mark seen, long-press to interrupt.
- **Push.** Web Push through a service worker, over the Tailscale HTTPS origin.
  - The Runner keeps one VAPID key and each device's subscription.
  - The payload is minimal: title and kind, never transcript text.
- **Pins and seen state** are shared with the desktop (R1).

Acceptance:

- In the fake-Runner phone test, an approval answered from the Inbox row shows as answered on the desktop.
- With the page closed, `PushManager` receives a needs-input push (the `test-remote-push` suite, in a headed Chromium context).

**R8: Several Runners in one window (P2, L).** Covers row 37.

- This is MULTI_RUNNER step 2, then step 3, as designed.
- After R1, the Agents and Workspaces sections are grouped by Runner.
- A remote Runner pairs as a device, so its owner bearer never leaves its machine.

Acceptance: with the Dev and Working fixtures running, one window lists both Runners' workspaces and agents, grouped. An approval from either is answered from the same Inbox.

**R9: Ship, with opt-in self-update, feedback and a reboot test (P1, M).** Covers rows 35, 36 and 44.

- **Updates.** Tauri's updater with signed releases. An update is *offered*, never forced (herdr's HN complaint), with "Install on quit".
- **Feedback.** Help → Send feedback attaches a `lazybox shot` and the version.
- **Reboot test.** A release gate restarts the session host and the Runner as a reboot would, and checks that conversations resume.

Acceptance:

- The release gate passes the reboot test.
- An update offered by a local feed installs only after a click.

**R10: Sandboxed panel extensions, the safe answer to mods (P2, L).** Covers row 34.

- **An extension** is a folder with `panel.yaml` and static HTML, JS and CSS.
- **Sandbox.**
  - It renders in an `<iframe sandbox="allow-scripts">` with no same-origin, under a strict CSP, from a dedicated origin.
  - It talks to Lazybox only through `postMessage`, with hook-style grants (`read`, `respond`, `start`) that the Runner checks like a device credential.
  - It has no network unless it declares hosts the person approves, and no file access.
- **Development.** Hot reload, and `lazybox shot PANEL` so an agent can see its own work.

Acceptance:

- An extension that lists agents by cost renders.
- Its RPC walk test shows it reaches only `read`.
- A `fetch` to an undeclared host is blocked.

**R11: New task in one step (P2, S–M).** Covers row 1.

- ⌘N opens one sheet: repository or repositories, provider and model, an optional mission, and the prompt.
- Enter creates the workspace and the conversation together and sends the prompt.
- The sheet remembers the last set of repositories.

Acceptance: from an empty window, ⌘N, a repository, a prompt and Enter give one workspace, one conversation and a sent prompt.

**R12: Chat-side polish, and "Continue in your CLI" (P2, S).** Covers rows 16, 20 and 21, and defects D5 and D8.

- **Context meter.** It is hidden until there is usage.
- **Workspace header.** It drops the duplicated conversation title when a chat panel is visible.
- **Review tray.** It docks below the diff with at most 40 % of the height. Under 1100 px it becomes a slide-over.
- **Continue in….** The conversation menu gets "Continue in Claude Code/Codex…". It copies the provider's own resume command (`claude --resume <id>`, `codex resume <id>`) with `cd <worktree>`. This covers the moments a provider feature isn't mapped; it is not a terminal.

Acceptance:

- Before any usage, no meter element is visible.
- At 1024×680 with the tray open, the diff region is at least 300 px tall.
- The copied command resumes the same session in the real CLI. This is a manual check and is noted as such.

**R13: Open in your terminal (P2, S; the out-of-scope row 25).**

- A workspace or conversation action, "Open in Terminal", opens the person's chosen terminal at the worktree.
  - The terminal is picked in Settings → Agents & workspaces: Ghostty, iTerm, Terminal or WezTerm.
- The terminal starts with `LAZYBOX_RUNNER` set. `lazybox run` and `lazybox log` from it then publish to that workspace's console, with the terminal-scoped credential.

Acceptance:

- The action starts the configured terminal with the worktree as its current directory.
- `lazybox run -- echo hi` typed there appears in the workspace console.

**R14: Sounds you choose (P3, S).** Covers row 15.

- For each kind: the built-in sound, a macOS system sound, or a file.
- A volume control.
- An option to play only when the window is not focused.

Acceptance: a chosen file plays from Settings, and the choice survives a reload.

**R15: Docs screenshots work again (P3, S).** Covers defect D10.

- Fix `showPanel` in `scripts/docs-screenshots.mjs` so it uses the dock's Panels menu, and drop the separator drags.
- Add `--variants` for dark/light and widths, so this re-audit's screenshots take one command.

Acceptance: `npm run docs:screenshots` exits 0.

### Out of scope, with the Lazybox-native answer

| herdr capability | Why it is out | What beats it for the same need |
|---|---|---|
| Ghostty-grade terminal and splits (row 25) | No terminal emulator (decision below) | R13: the person's own terminal, wired to Lazybox. `lazybox run` / `log` put output in a console that both the agent and the person read. |
| Terminal agents through OSC 7501 (row 11) | Same decision | R3, ACP: any ACP agent gets approvals, the plan, diffs, cost and evidence, none of which a terminal pane can show. |
| Unsandboxed JavaScript mods (row 34) | Security | R10 sandboxed panels with grants, plus R4 settings as files, plus R5 `lazybox do`. |

### Work packages for parallel agents

Each package owns a distinct set of files. The only overlaps are listed with
the order to land them.

| Package | Briefs | Files |
|---|---|---|
| **A. Sidebar and agents** | R1 | `AgentsPanel.tsx`, `agents-panel.css`, `WorkspaceRow.tsx`, `workspace-row-model.ts`, `CollectionPanel.tsx`, `ConversationList.tsx`, `Inbox.tsx`, `inbox.css`, `layout-presets.ts` (left-column split only), `dock.css` (tab strip only), `attention.ts` (pins in Runner preferences), `remote/RemoteApp.tsx` (showing pins only); new `scripts/test-agents-visible.mjs` |
| **B. Layout keyboard** | R6 | `DockLayout.tsx`, `Workspace.tsx` (the layout-command block, lines ~460–510), `actions.ts` (new `layout.*` ids only), `keyboard.ts` (the reset chord), `ShortcutsOverlay.tsx`; new `scripts/test-layout-keyboard.mjs` |
| **C. Config files and command plane** | R4, R5, R7 | `services/runner/code/internal/power/*` (generic loader), `cmd/lazybox/power.go` (`do`, `config`), `PowerLayer.tsx`, `useUiRequests.ts`, `user-actions.ts`, `user-commands.ts`, `KeyboardSettings.tsx`, `keyboard.ts` (load from file), `layout-presets.ts` (user layouts), `NotificationSettings.tsx` (file I/O). **Order:** after A (shared `layout-presets.ts`) and after B (shared `keyboard.ts`). |
| **D. Orchestration** | R2 | `services/runner/code/internal/work/*` (grants, leases, messages, budgets), `internal/transport/auth.go` (grant table), `internal/runner/work.go`, `internal/runner/orchestration.go`, new `cmd/lazybox/{agents,mission,lease}.go`, `MissionsPanel.tsx`, `missions-model.ts`, `missions.css`, `useMissions.ts`, `MissionInbox.tsx`, `ORCHESTRATION.md` |
| **E. ACP adapter** | R3 | new `services/runner/code/internal/runner/adapters/acp/*`, `internal/accounts` (detection), `internal/runner/provider_availability.go`, `useProviderAvailability.ts`, `Icons.tsx`; new `scripts/fixtures/acp-fake.mjs`. Does not edit `NewSessionForm.tsx`. |
| **F. Phone** | R7p | `remote/*` (after A's small pins edit), `apps/web` (service worker, manifest), `services/runner/code/internal/remote/*`, `internal/devices/*`, `internal/transport/device_auth.go`; new `scripts/test-remote-push.mjs` |
| **G. Entry and chat polish** | R11, R12, R13, R14 | `NewConversationDialog.tsx`, `NewSessionForm.tsx`, `WorkspaceHome.tsx`, `ChatHeader.tsx`, `WorkspaceHeader.tsx`, `TelemetryPanel.tsx`, `ReviewTray.tsx`, `review.css`, `SessionMenu.tsx`, `sounds.ts`, `RuntimeSettings.tsx`, `apps/desktop/src-tauri/src/host.rs` (the open-terminal command only) |
| **H. Ship** | R9 | `apps/desktop/src-tauri` (updater, `tauri.conf.json`; not `host.rs`), `scripts/release/*`, `scripts/release-gates.*`, `HelpMenu.tsx`, `Onboarding.tsx`, `release-notes.ts` |
| **I. Several Runners** | R8 | `RunnerContext.tsx`, `runner-locator.ts`, `apps/desktop/src-tauri/src/host.rs` (`runner_bootstrap`), then grouping in `CollectionPanel.tsx` and `AgentsPanel.tsx`. **Order:** after A and G land. |
| **J. Extensions** | R10 | `PanelRegistry.tsx` (one new kind), new `ExtensionPanel.tsx` and `extensions.css`, new `services/runner/code/internal/power/extensions.go`, an extension credential in `internal/transport` (like `hook_auth.go`). **Order:** after C, which owns `internal/power`'s loader. |
| **K. Docs screenshots** | R15 | `scripts/docs-screenshots.mjs` |


## Status (same day, earlier)

Shipped on `feat/v2-command-output` after this comparison was written:

- **P1-5 — done.** Workspace rows have four fixed lines: name, repositories,
  a git line (branch, uncommitted, not pushed, to pull, behind the default
  branch, PR with a checks dot) from the new owner-only
  `ListWorkspaceGitSummaries`, and the conversation's title with one status
  mark and its elapsed time. Provider and model are the tooltip; no more "C.".
  Ahead of the spaces mod: PR and checks state, and behind-default.
- **P1-6 — done, without the terminal escape hatch** (no terminal emulator,
  below). Alt-click, Space (Quick Look) or "Peek" in the context menu on a
  conversation, process, command, artifact, mission task or file opens it
  live over the page; ← → step through the list, ⌘↵ opens it here, Space or
  Esc closes. A conversation can be answered from the peek; a process
  stopped. A sub-agent card peeks in its own floating panel.
- **P2-6 — done.** Changes has Unified / Split (remembered), word-level
  highlights, folded unchanged lines, `n`/`p` hunks and Viewed checkboxes;
  review comments work in both layouts. The diff row above (line 130) is now
  a Lazybox lead.
- **P2-1 — done, partly.** A first-run welcome (subscription, workspace, the
  five keys, docs) and What's new once per release from bundled notes, both
  reopenable from Help and the command palette. Not done: phone pairing and a
  keyboard-preset choice in the welcome, media in release notes, self-update.

## Decision: no terminal emulator

Lazybox V2 does not compete as a terminal emulator (owner's decision,
2026-11-16). herdr's Ghostty-grade terminal, terminal splits and terminal
agents are out of scope. The existing workspace shell panel stays a minimal
escape hatch and gets no further investment; people use their own terminal
(Ghostty, iTerm…) and pipe output in with `lazybox log` / `lazybox run`.
Recommendations that depended on terminals (P1-3, terminal splits in P1-1,
OSC 7501 terminal agents in P2-7) are dropped.

## Sources

herdr: [herdr.dev](https://herdr.dev) · [guide](https://herdr.dev/guide/) ·
[TUI page](https://herdr.dev/tui/) · [keyboard (TUI)](https://herdr.dev/docs/keyboard/) ·
[mobile](https://herdr.dev/mobile) · [blog index](https://herdr.dev/blog/) ·
[herdr-app README](https://github.com/herdrdev/herdr-app) ·
[herdr-app CHANGELOG 0.1.4–0.1.16](https://github.com/herdrdev/herdr-app/blob/master/CHANGELOG.md) ·
[defaults/AGENTS.md](https://github.com/herdrdev/herdr-app/blob/master/defaults/AGENTS.md) ·
[defaults/keys.macos.json](https://github.com/herdrdev/herdr-app/blob/master/defaults/keys.macos.json) ·
[defaults/layouts/main.json](https://github.com/herdrdev/herdr-app/blob/master/defaults/layouts/main.json) ·
[defaults/mods](https://github.com/herdrdev/herdr-app/tree/master/defaults/mods) ·
[docs/commands.md](https://github.com/herdrdev/herdr-app/blob/master/docs/commands.md) ·
[docs/security.md](https://github.com/herdrdev/herdr-app/blob/master/docs/security.md) ·
[herdr TUI README](https://github.com/herdrdev/herdr) ·
[heise, 2026-11-02](https://www.heise.de/en/news/Herdr-desktop-app-A-hub-for-AI-coding-agents-11508175.html) ·
[Bitdoze gallery tour, 2026-11-08](https://www.bitdoze.com/herdr-app-layouts-mods/) ·
[HN: "Herdr has released native app"](https://news.ycombinator.com/item?id=50298436).
Repository statistics are from the GitHub API on 2026-11-16.

Not verified: how herdr feels in use (I did not install it); whether herdr
posts system notifications without a mod (the docs say toast plus sound,
heise says system notification); Lazybox behavior after a machine reboot.
