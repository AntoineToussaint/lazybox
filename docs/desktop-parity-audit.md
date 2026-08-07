# Desktop ↔ TUI parity audit (#957)

One authoritative, file:line-backed list of every gap between the ratatui
TUI and the Tauri desktop app, grouped by area and tagged by what closing
it actually requires. Supersedes the ad-hoc tracking in #817 (see
[Relationship to #817](#relationship-to-817)).

Line numbers are from the tree at the time of writing; treat them as
anchors, not guarantees.

## Method

Three surfaces were enumerated exhaustively and cross-referenced:

- **TUI render** — `crates/tui/src/components/{workspace_row,sidebar/*,right_pane/*,terminal_stack}.rs` (every pill/chip/badge the TUI draws).
- **Desktop render** — `apps/desktop/src/{main,model}.ts`, `index.html` (every badge the webview draws, and the row model `rowSignals` / `detailSignals` in `model.ts`).
- **Command surfaces** — the two channels the desktop can act through:
  - `DesktopCommand` (the `send_command` wire enum, `crates/server/src/api_gateway.rs:~400`, generated at `apps/desktop/src/generated/DesktopCommand.ts`) — 21 workspace/terminal mutations.
  - Tauri `invoke` handlers in `apps/desktop/src-tauri/src/main.rs` — `set_filters`, `set_search`, `set_sort_mode`, `set_mailbox`, `open_url`, plus infra (`begin_github_login`, `subscribe_events`, `send_terminal_frame`, `record_analytics`).
- **Wire contract** — the generated types in `apps/desktop/src/generated/` are authoritative for what physically crosses to the webview (`DesktopEvent`, `Workspace`, `Task`, `Session`, `DesktopTerminalSnapshot`, `DesktopInboxView`).

## Correcting the trigger

The issue's premise — "the desktop **doesn't show status** — CI/review/agent-state/conflict" — is **half true**, and the accurate version sharpens the fix:

- **CI and review ARE already rendered** on desktop rows. `rowSignals` (`apps/desktop/src/model.ts:120-139`) emits a CI pill (`ciSignal`, `model.ts:57-70`) and a review pill (`reviewSignal`, `model.ts:72-88`), plus a `reply` pill and an `N unread` pill. `renderWorkspaceRow` (`apps/desktop/src/main.ts:1169-1225`) draws them.
- What the desktop row **drops** is everything else the TUI row carries: **conflict/mergeable**, the **agent-state chip**, all **automation pills** (AUTO / ARM / FIX / track-main), the **model-tier badge**, the **`]N` snippet badge**, **behind-base**, **priority**, **labels**, **reviewers/assignees**, **pin/star**, **linked-checkout**, and **notes**.

So the headline is not "no status" but "a rich per-row status vocabulary collapsed to four pills." Nearly all of it is **render-only** — the data is already on the wire.

## Tags

| Tag | Meaning |
|---|---|
| **render-only** | Data already reaches the webview (on `Task` / `Workspace` / `DesktopEvent`). Pure frontend work — draw it. |
| **needs-wire** | Field is *not* on the desktop stream today; the daemon must serialize it first, then the frontend can draw it. |
| **needs-command** | No `send_command` variant and no Tauri `invoke` handler performs this action. Requires a new command *and* frontend UI. |
| **present** | At parity (may differ cosmetically). |
| **partial** | Present but narrower than the TUI. |

---

## 1. Status / row rendering (the headline gap)

Every visual indicator the TUI puts on a workspace row, activity pane, or
terminal tab, vs. what the desktop draws. Row indicators come from
`crates/tui/src/components/workspace_row.rs` and
`crates/tui/src/components/sidebar/pills.rs`.

### 1a. Sidebar workspace-row indicators

| Indicator | TUI source | Desktop | Wire | Tag |
|---|---|---|---|---|
| CI pill (CI OK/FAIL/MIX/RUN) | `pills.rs:120-147`, `workspace_row.rs:1033` | **rendered** (`model.ts:57-70`) | `Task.ci` | present |
| Review pill (APPROVED/CHANGES/REVIEW) | `pills.rs:96-116` | **rendered** (`model.ts:72-88`) | `Task.review` | present |
| Unread pill (`●N`) | `workspace_row.rs:624-644` | **rendered** (`model.ts:135-137`) | `Task.unread_count` | present |
| Reply-needed pill | (role/needs-reply) | **rendered** (`model.ts:131-133`) | `Task.needs_reply` | present |
| **Conflict pill (CONFLICT / `?`)** | `pills.rs:208-213` | **missing** — `rowSignals` never reads `mergeable` | `Task.mergeable` | **render-only** |
| **Agent-state chip** (spinner / `?` input / `✓` done / `✗` exited) | `workspace_row.rs:440-468` | **missing on row** (only on terminal tab) | `DesktopEvent::AgentState`, `DesktopTerminalSnapshot.agent_state` | **render-only** |
| **AUTO pill** (GitHub-native auto-merge) | `workspace_row.rs:947-960` | **missing** | `Task.auto_merge_enabled` | **render-only** |
| **ARM pill** (lazybox merge-on-green) | `workspace_row.rs:972-985` | **missing** (only a detail-pane toggle) | `Workspace.auto_merge_on_green` | **render-only** |
| **FIX pill** (auto-fix armed) | `workspace_row.rs:990-1003` | **missing** (only a detail-pane toggle) | `Workspace.policies` | **render-only** |
| **Track-main pill** (`⤓main` / `behind`) | `workspace_row.rs:1010-1031` | **missing** | `Workspace.track_main`, `track_main_behind` | **render-only** |
| **QUEUED pill** (in merge queue) | `pills.rs:214-219` | **missing** | `Task.is_in_merge_queue` | **render-only** |
| **`]N` snippet badge** | `workspace_row.rs:921-934` | **missing** | `Workspace.sent_snippets` | **render-only** |
| **Notes badge (`✎`)** | `workspace_row.rs:906-916` | **missing** | `Workspace.notes` | **render-only** |
| **Linked-checkout badge (`⎇ local`)** | `workspace_row.rs:889-901` | **missing** | `Workspace.linked_checkout` | **render-only** |
| **Label chips** | `workspace_row.rs:549-598` | **missing on row** (detail pane only, `model.ts:113-115`) | `Task.labels` | **render-only** |
| **Role badge** (A/R/@/·) | `workspace_row.rs:402-418` | **missing** (role rendered as lowercase text in row top, `main.ts:1194`) | `Task.role` | render-only (partial) |
| **Priority** | (not on TUI row either) | **missing** | `Task.priority` | render-only |
| **Behind-base** | *(TUI shows it via track-main only, not a row pill)* | **missing on row** (only an action-menu item) | `Task.is_behind_base` | render-only |
| Type glyph (PR/issue/Linear) | `workspace_row.rs:340-376` | via `taskReference` text (`main.ts:1192`) | `Task.kind` | partial |
| PR/issue number | `workspace_row.rs:378-400` | via `taskReference` | `Task.id` | present |
| Agent-runner badge (` C `, ` C×2 `) + jump digit | `workspace_row.rs:650-716` | **missing** | derivable from `Workspace.sessions` + `TerminalSpawned` | render-only |
| Shell badge (` S `) | `workspace_row.rs:816-833` | **missing** | derivable from sessions/terminals | render-only |
| **Model-tier badge (`◆ Opus`)** | `workspace_row.rs:727-746` | **missing** | **not on wire** (see §2) | **needs-wire** |
| Stale-issue title fade | `workspace_row.rs:492-495` | **missing** | `Task.created_at` | render-only |
| Age/time trailer | `workspace_row.rs:1065-1100` | **rendered** (`main.ts:1200-1207`) | `Task.updated_at` | present |

### 1b. Activity pane (RightPane)

The TUI activity pane (`crates/tui/src/components/right_pane/{mod,card}.rs`)
is a rich, cursor-navigable, per-row card list. The desktop
(`renderActivity`-equivalent around `main.ts:1286-1310`) is a **read-only
flat list capped at 30 rows** with author + relative time + body text.

| TUI feature | TUI source | Desktop | Tag |
|---|---|---|---|
| Per-row unread bullet | `card.rs:83-90` | missing | render-only |
| Per-row cursor / expand-collapse | `card.rs:93-103`, `mod.rs` (`ToggleRow`) | missing (no per-row expand) | render-only |
| Activity-row multi-select | `card.rs:106-115` | missing | render-only + needs-command |
| Reviewers row (`Reviewers: … / g r to request`) | `mod.rs:1364-1391` | missing | render-only + needs-command |
| Assignees row | `mod.rs:1392-1419` | missing | render-only + needs-command |
| State pill in header (OPEN/DRAFT/MERGED/…) | `mod.rs:1277-1324` | detail state pill present (`model.ts` `detailSignals`) | present |
| Diff-stat (`+N −M`) | *(TUI right pane)* | present in detail (`detailSignals`) | present |
| Description teaser ⇄ full reader modal (`d`) | `mod.rs:2341-2359`, `#448` | body shown flat, no teaser/reader modal, no `ToggleDescription` | render-only |
| Ask-about-this-PR (`a` in reader, `#945`) | reader modal | missing | needs-command |
| Summary-mode one-liner (`Shift-P`) | `mod.rs:2175-2222` | activity-pane mode `<select>` exists (`main.ts:312`) | partial |

### 1c. Terminal tab strip

TUI tab strip: `crates/tui/src/components/terminal_stack.rs:3469-3639`.
Desktop tab: `main.ts:1666-1667` (`terminal-tab-state`) + panel header pill.

| Indicator | TUI source | Desktop | Tag |
|---|---|---|---|
| Agent-state hint (working/input/done/exited) | `terminal_stack.rs:3572-3601` | **rendered** (`formatAgentState`, `main.ts:3595`) | present |
| **Model-tier badge (`◆ Opus`)** | `terminal_stack.rs:3627-3637` | missing | **needs-wire** |
| **On-main badge (`⎇ main`)** | `terminal_stack.rs:3616-3623` | missing | **needs-wire** |
| **No-permission badge (`⚠ no-perms`)** | `terminal_stack.rs:3605-3612` | missing | **needs-wire** |
| Exit banner (`⚠ agent exited … r restart`) | `terminal_stack.rs:4310-4380` | terminal state shows `exited N`; no restart affordance | partial |

**Net for §1:** with three exceptions (model tier, on-main, no-perms — all
terminal-scoped, see §2) **every dropped row/tab indicator is render-only** —
the data is already on the wire. This is the first thing to fix; it's what a
user notices immediately and it needs no daemon change.

---

## 2. Wire-data gaps

The only status the desktop **physically cannot render today** because it is
not serialized onto the desktop stream. All three are per-terminal and all
three already exist on the REST `/v1/agents` projection (`RunningAgent`,
`api_gateway.rs:214-247`) — they were simply never added to the webview's
`DesktopTerminalSnapshot` / `TerminalSpawned` path.

| Field | TUI badge | On REST? | On desktop stream? | Fix |
|---|---|---|---|---|
| **Model-tier label** (`Opus`, `gpt-5.5 · xhigh`) | `◆ Opus` (`terminal_stack.rs:3627`, `workspace_row.rs:727`) | yes — `RunningAgent.model` (`api_gateway.rs:236`) | **no** — `DesktopTerminalSnapshot` carries only `agent_state` (`generated/DesktopTerminalSnapshot.ts`) | add `model: Option<String>` to `DesktopTerminalSnapshot` + `TerminalSpawned` |
| **On-main flag** | `⎇ main` (`terminal_stack.rs:3616`) | yes — `RunningAgent.on_main` (`api_gateway.rs:238`) | **no** (desktop *sends* `on_main` in `SpawnAgent`/`SpawnShell` but the daemon never echoes it back per terminal) | add `on_main: bool` to the terminal snapshot/spawn event |
| **No-permission flag** | `⚠ no-perms` (`terminal_stack.rs:3605`) | yes — `RunningAgent.no_permission` (`api_gateway.rs:240`) | **no** | add `no_permission: bool` to the terminal snapshot/spawn event |

Everything else audited in §1 rides existing fields on `Task`
(`ci`, `review`, `mergeable`, `auto_merge_enabled`, `is_in_merge_queue`,
`is_behind_base`, `labels`, `reviewers`, `assignees`, `priority`,
`unread_count`) or `Workspace` (`auto_merge_on_green`, `policies`,
`track_main`, `track_main_behind`, `sent_snippets`, `notes`,
`linked_checkout`) or `DesktopEvent::AgentState`.

> **Protocol note:** editing `crates/ipc/src/**` bumps the protocol
> fingerprint — the desktop contract fixture (`apps/desktop/src/generated`,
> `compatibility.json`) must be regenerated (`make desktop-contract`) after
> any of these wire additions land.

---

## 3. Action parity (ActionKind → command)

All 83 `ActionKind` variants (`crates/tui-core/src/action.rs:423-512`)
against the desktop's two command channels. "DC" = a `DesktopCommand`
(`send_command`); "TI" = a Tauri `invoke` handler; "url" = `open_url`;
"client" = handled entirely in the webview.

### 3a. Actions at parity (already reachable on desktop)

| ActionKind | Desktop path |
|---|---|
| OpenWorkspace | DC `FocusWorkspace` |
| Work / WorkWith / SpawnAgent | DC `SpawnAgent` (agent + `model_alias` tiers) |
| SpawnShell | DC `SpawnShell` |
| SpawnAgentOnMain / SpawnShellOnMain | DC `SpawnAgent`/`SpawnShell` `{ on_main }` |
| NewWorkspace / StartAgent | DC `CreateWorkspace` |
| RenameWorkspace | DC `RenameWorkspace` |
| MarkAllRead | DC `MarkRead` |
| ToggleSnooze / LongSnooze | DC `Snooze` (presets) / `Unsnooze` |
| Archive | DC `Archive` |
| CloseIssue | DC `CloseIssue` |
| DeleteOrClose | DC `DeleteOrClose` |
| MergePr | DC `MergePr` |
| UpdateBranch | DC `UpdateBranch` |
| ToggleAutoMerge | DC `SetAutoMergeOnGreen` |
| ToggleAutoFix | DC `SetAutoFixPolicies` |
| ToggleTrackMain | DC `SetTrackMain` |
| SyncWorkspace | DC `SyncWorkspace` |
| EditNotes | DC `SetNotes` (`notes-form`, `main.ts:268-270,509`) |
| Reply | DC `PostReply` |
| Refresh | DC `Refresh` |
| OpenInBrowser | url `open_url` (`main.ts:2394`) |
| OpenFilterMenu | TI `set_filters` (`main.ts:918`) |
| OpenSearch | TI `set_search` (`main.ts:940`) |
| CycleSort | TI `set_sort_mode` |
| CycleMailbox | TI `set_mailbox` |
| ToggleRepoGroup | client `collapsedRepos` (`main.ts:359,1097`) |
| ToggleFocusMode | client (`.` chord, `main.ts:416`) |
| ToggleActivityPane | client `activity-pane-select` (`main.ts:312`) |
| OpenSettings | client settings dialog |
| OpenThemePicker | client theme list (`main.ts:307`) |
| CyclePane / FocusPaneLeft / FocusPaneRight | client focus |
| ResizeSplitter | client drag (`main.ts:2010`) |
| TerminalScroll | client |
| DismissNotice | client status line |
| Quit / ForceRedraw / ToggleMouseCapture | n/a (window-manager / native concerns) |

### 3b. Partial

| ActionKind | Gap |
|---|---|
| ViewDiff | DC `InspectWorkspaceDiff` gives a **read-only** diff view; the TUI also **annotates + sends the diff to the agent** — missing. |
| ManagePolicies | Individual toggles exist (`renderAutomation`, `main.ts:1353-1381`) but not the TUI's unified policies menu (merge-on-green + auto-fix + GitHub-native in one surface, `#363`). |
| OpenSnippets / LeaveTerminal (`]]s`) | A snippet picker exists via **Cmd/Ctrl-J** (`main.ts:2060-2066`); the `]]` leader family and read-only snippet catalog are absent. |
| OpenGlobalSearch | One `set_search` field; scoped-vs-global distinction not expressed. |
| NewProject | `CreateWorkspace` needs an existing `project_key`; registering a *new* tracked repo/local project happens only in the setup dialog, not as an action. |

### 3c. Missing — needs new command (`send_command` variant or `invoke`) + UI

| ActionKind | Section | Note |
|---|---|---|
| OpenEditor | Workspace | no open-in-editor command/handler at all |
| MoveToSpace | Workspace | Spaces tier unsupported on desktop |
| ImportCheckout | Workspace | no linked-checkout import flow |
| AddScanRoot | Workspace | no scan-root management |
| AdoptSessions | Workspace | session-move handoff absent |
| SendToSession | Workspace | agent-to-agent handoff (`#431`) absent |
| ConvertSession | Workspace | Continue/Critic session swap absent |
| CollapseIntoPr | Workspace | join-issue-into-PR absent |
| RequestReviewers | Workspace | reviewers not rendered nor editable |
| AddAssignees | Workspace | assignees not rendered nor editable |
| ManageLabels | Workspace | labels editable nowhere |
| SelectWorkspace | Sidebar | **no multi-select** |
| BroadcastToSelected | Sidebar | **no broadcast** (depends on multi-select) |
| ToggleRepoPin | Sidebar | pinning unsupported |
| ToggleFocusWorkspace | Sidebar | the `★ Focused` header renders but nothing can star into it |
| SelectRow | Activity | activity-row multi-select absent |
| ToggleRow | Activity | per-row expand/collapse absent |
| ToggleDescription | Activity | teaser/reader-modal absent (body shown flat) |
| UndoMarkRead | Activity | re-unread absent |
| ActivityTop / ActivityBottom | Activity | no activity cursor to jump |
| ToggleActivity | Activity | activity section collapse absent |
| JumpToWorkspace | Global | fuzzy workspace jump (`` ` ``) absent |
| JumpToAsking | Global | jump-to-asking (`!`) absent |
| JumpToFailingCi | Global | jump-to-failing-CI (`Shift-F`) absent |
| JumpToLimited | Global | jump-to-rate-limited absent |
| ResumeRateLimited | Global | bulk resume absent |
| OpenHelp | Global | Ask-Lazybox / keymap help absent |
| OpenTour | Global | guided tour absent |
| OpenSyncStatus | Global | sync-diagnostics view absent |
| OpenMessages | Global | messages log absent |
| OpenErrorInbox | Global | durable error store absent |
| InspectNotice | Global | full-text notice detail absent |

**Action tally:** ~35 at parity, ~5 partial, **~32 missing** (of which the
Workspace-section CRUD trio — reviewers/assignees/labels — and the
multi-select/broadcast pair are the most user-visible).

---

## 4. Interactions / UX parity

| Capability | Desktop | Tag |
|---|---|---|
| Filter menu (predicate multi-select) | **present** (`set_filters`, `main.ts:918`) | present |
| Search | **present** (`set_search`) | present |
| Sort cycle | **present** (`set_sort_mode`) | present |
| Mailbox switch (Inbox/Inactive/Snoozed) | **present** (`set_mailbox`) | present |
| Repo-group collapse | **present** (client) | present |
| Focus mode (`.`) | **present** (client) | present |
| Activity-pane mode (full/summary/hidden) | **present** (`<select>`) | partial |
| Settings dialog | **present** | present |
| Theme picker | **present** (in settings) | present |
| Snippet picker | **present** but Cmd/Ctrl-J, not `]]s` | partial |
| View diff | **present** (read-only) | partial |
| Repo summaries (`active · attention ⚑`) | **present** (`main.ts:1119-1148`) | present |
| Filter chips | **present** (`main.ts:989-1008`) | present |
| **Multi-select / broadcast** | **absent** | needs-command |
| **Skills picker (`]]l`)** | **absent** | needs-command |
| **Prompt history (`]]h`)** | **absent** | needs-command |
| **URL scan/open (`]]u`)** | **absent** (only per-task "open in browser") | needs-command |
| **Ask-Lazybox / help** | **absent** | needs-command |
| **Open-in-editor** | **absent** | needs-command |
| **Quick-jump nav** (`` ` `` / `!` / `Shift-F`) | **absent** | needs-command |
| **The `]]` leader family** | **absent** (only Cmd-J snippet shortcut) | needs-command |
| **Tour / messages log / error inbox / sync diagnostics** | **absent** | needs-command |

---

## Recommended fix order

1. **Render the status the wire already carries (render-only, no daemon change).**
   In priority order of user visibility: conflict pill, agent-state chip on
   rows, automation pills (AUTO/ARM/FIX/track-main), `]N` snippet badge,
   labels on rows, notes/linked-checkout badges. This is the gap the user
   notices immediately and it is pure `apps/desktop/src/{main,model}.ts`
   work driving `Task` / `Workspace` / `DesktopEvent::AgentState` fields
   that already arrive.
2. **Add the three terminal-badge wire fields** (model tier, on-main,
   no-perms) to `DesktopTerminalSnapshot` + `TerminalSpawned`, then render
   them on the tab strip (and the model tier on the row). Small, contained
   daemon change; regenerate the contract after.
3. **Reviewers / assignees / labels** — the highest-value missing *actions*
   (they're day-to-day PR triage). Each needs a `DesktopCommand` + a small
   editor UI; the read-side data is already on `Task`.
4. **Multi-select + broadcast** — a structural addition that then unlocks
   bulk merge/snooze/archive/mark-read at desktop parity.
5. Everything else in §3c / §4, prioritized as the desktop roadmap dictates.

---

## Relationship to #817

This matrix is intended to **supersede** the Tier B/C bullet tracking in
#817: it is the single grouped, file:line-evidenced, tagged list #817 was
gesturing at. Recommended: close #817 as tracked-by-this-doc (or convert
its remaining bullets into issues cut from §2–§4 here), and treat §1's
render-only status set as the immediate follow-up work item.

Related context: #816 (Tier A, landed), #806 (`docs/desktop-remote-readiness.md`),
#936 (2-col layout, landed as `9af18dc0`).
