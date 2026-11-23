# Artifacts: plans and reports as their own pages

Agents often write a plan, a review or a spec in a chat message, or in a
file such as `module-datasource/docs/PLAN.md`. You can track either as an
**artifact**: a versioned document of the workspace with its own page. You
can compare its versions, export it, and hand it back to an agent. It keeps
the link to where it came from.

## The model

The Runner keeps artifacts for each workspace
(`internal/runner/artifacts`, stored under `<state>/artifacts/<workspace>/<id>/`).
Each artifact has:

| Field | What it holds |
| --- | --- |
| `id`, `title`, `kind` | `kind` is plan, report, spec, notes or other. The title and kind can change at any time. |
| `format` | Markdown by default. HTML has a sandboxed preview and structured artifacts use JSON. |
| `versions` | Append-only, oldest first. Each version has a number, a time, a sha256 digest, a size, an **author** and a **source**. The author is `person`, `agent` (with its execution) or `file` (the tracked file changed on disk). The source is `message` (execution, conversation, message cursor, turn), `file` (workspace path), `agent` or `edit`. |
| `origin_session_id`, `origin_conversation_id` | The conversation the artifact was first taken from. Later messages of that conversation offer "Update from this message". |
| `tracked_path`, `tracking_problem` | The workspace file the Runner follows, and why it could not follow it lately (removed, not text, larger than 1 MiB). |
| `links` | An implementation roadmap can pin its source design and review by workspace artifact ID and revision. Direct mission/task links are reserved. |
| `retired` | Set while the artifact is retired (see [Retiring and merging](#retiring-and-merging)): when, by whom (`person` or `agent` with its execution), the artifact that `superseded_by` it, and a one-line reason. |

These rules hold for every caller:

- **Saving the same content again adds no version.** It returns the artifact
  unchanged, so a retried call is harmless.
- **`expected_version` protects reviewed changes.** If a newer version
  exists, the write is refused (`Aborted`) and the person reviews again.
- **Tracking an already tracked file returns the existing artifact.**
- **Limits:** a title of at most 200 characters, content of at most 1 MiB of
  UTF-8, and at most 1000 versions per artifact.
- **Deleting a workspace deletes its artifacts.** Deleting a conversation
  does not delete the artifacts taken from it.
- **Retiring never deletes.** A retired artifact keeps every version, is
  left out of `ListArtifacts` unless `include_retired`, and is restored as
  it was.

## Design and review to roadmap

New roadmaps are plan artifacts stored as versioned JSON using `roadmap.v1`. Use **Edit roadmap milestones** on the latest revision to update typed fields and save another revision. Concurrent edits are checked against the revision opened. Use `lazybox artifact template roadmap` for the JSON starter and publish it with `--kind plan --format json`. Older Markdown and plan.v1 documents stay readable.

For a milestone roadmap, **Export → Download mission plan** saves a proposal for `lazybox mission plan FILE`. Enter one dependency ID per line, and leave the field empty for an independent milestone. The Runner validates milestone IDs and the dependency graph on save. Mission export also requires a deliverable and definition of done for each task. Each task includes its source revision and milestone brief. Submitting the file requires an orchestrator grant. Execution has a separate approval step.

In **Artifact actions → Create implementation roadmap**, the reader seeds an editable milestone form from the headings in the displayed revision. Each milestone starts open with an editable ID, phase, deliverable, dependencies, definition of done, verification evidence and open decisions. Keep its ID when moving it between phases. Update the same plan as evidence arrives. Edit the milestones, optionally include a workspace review, then save a separate plan. The plan records each source revision. Source documents list their roadmaps; opening a source from a roadmap returns to the pinned revision even after the source changes. A deleted source leaves its ID and revision in the plan. Source links describe the plan’s origin and persist across edits.

**Draft mission proposal in chat** prepares a handoff containing the plan and pinned source references. In a window without a target conversation the handoff is copied. The person reviews and sends the draft. Mission creation and execution use the separate orchestration and approval flow.

### Artifact types and the Hub boundary

| Deliverable | A single Runner can support | Hub opportunity |
|---|---|---|
| Design, review and implementation roadmap | Versioned source, pinned revisions, editable milestones, side chat and offline export | Shared organization library and review across Runners |
| PR walkthrough or investigation timeline | Local diffs, findings, command references and evidence snapshots | Stable team URLs, authenticated external viewers and notifications |
| Dashboard | A snapshot from authorized Runner telemetry; a panel can show fresh local data | Federated queries and viewer-specific access across Runners |
| Slides or visual prototype | Standalone HTML with a local preview and downloadable source; a dedicated editor is a separate feature | Organization design systems, collaborative annotations and publication |

Local artifact identity belongs to the Runner. A future Hub can resolve Runner and workspace identities and enforce sharing rights. A local pinned reference grants no network access and creates no public URL.

## Proposed artifact extensions

Use the existing [sandboxed extension host](EXTENSIONS.md) for artifact
editors and previews. A versioned manifest contribution identifies a
workspace artifact type by extension ID and type ID, and names its editor,
preview, and optional export formats. The host resolves the contribution
when a person creates or opens an artifact. Conversations can publish the
same versioned source through the artifact API, regardless of their agent.

The Runner keeps the source, author, version history, and optimistic write
check. An editor receives the explicitly opened revision and returns proposed
source. Lazybox saves it only when the person chooses Save, with the expected
version from the revision that was opened. The preview receives just that
revision. Export asks the extension for bytes in a declared format and uses
Lazybox's normal save flow. The extension uses the host's existing approval
and network rules.

Start with two contributions:

| Type | Source | Preview and export |
| --- | --- | --- |
| Plan | Existing Markdown | Plan outline and checklist, with Markdown export |
| Visualization | Versioned declarative chart specification stored as text | Interactive chart, with source and static image export |

Keep Markdown as the default for existing artifacts. Every registered type
has a readable source view and source export available if its extension is
disabled, missing, or incompatible. Preserve the revision history across
extension upgrades. A type that needs a source migration proposes a new
revision through the same save flow.

The first implementation should exercise creating, editing, previewing, and
exporting both types through one manifest contract. Also test a conflicting
edit and opening an existing revision after disabling its extension.

## Where artifacts come from

**A chat message.** Every agent message has a quiet **Track** button that
appears on hover or keyboard focus, and its right-click menu has **Track as
artifact…**. Shift+right-click keeps the browser's own menu. The dialog
suggests:

- a title, from the message's first heading or else its first line;
- a kind, from the title.

It also shows a preview. When the conversation already has an artifact, the
dialog offers to update that one instead. Once tracked, the Artifacts panel
opens on the new page.

When the message links Markdown files of the workspace (a Markdown link, or
inline code naming one, as `docs/PLAN.md`), the dialog first asks what to
track: **Track `docs/PLAN.md`** (stays in sync with the file, as below) or
**Track this message's text**. A short message that mostly names the file
(at most 30 words, no code block) has the file chosen already; a longer reply
has its text chosen. Links resolve as opening them does.

On later messages of the same conversation, the right-click menu also offers
**Update "Title" from this message…**. This shows a line diff against the
latest version, folded around the changes, with a toggle to see the result.
Saving names the version that was compared. If someone saved a newer version
meanwhile, the diff is redone against it and nothing is overwritten.

**A repository file.** In Files, right-click a Markdown file and choose
**Track as artifact**. The editor's tab menu has the same action, and so does
the right-click menu of a file an agent names in a message: the link resolves
as opening it does (a choice when several files match), a non-Markdown file
shows the item disabled, and a file outside the workspace gets the Runner's
refusal. On a file link, the message's own item reads **Track this message as
artifact…**. The Runner
then checks the file every 2 seconds. A change becomes a new version (author
`file`) once the same content has been on disk for 2 seconds, so an agent
writing in several steps produces one version. The page shows "Synced with
`api/docs/PLAN.md`" and "v3 · file changed on disk · 2 min ago". The Runner
cannot know who edited the file, so it does not say. When the file
disappears, the last version is kept and the problem is shown.
**Stop syncing with file** ends the tracking and keeps every version.

**An agent.** Agents use the `lazybox` CLI with their own scoped credential:

```sh
lazybox artifact put PLAN.md --title "Datasource migration plan" --kind plan
lazybox artifact put - --id ID < PLAN.md      # a new version of the same artifact
lazybox artifact put STATUS.md --title "Status" --supersedes ID1,ID2,ID3
                                              # one artifact replacing several, which are retired
lazybox artifact retire ID [--superseded-by ID] [--reason "…"]
lazybox artifact get ID [--version N]         # prints the Markdown
lazybox artifact list [--all]                 # --all includes retired artifacts
```

Every agent's briefing (`commandBriefing` in
`internal/runner/command_output.go`) tells it to publish plans, design docs,
specs, reports and notes this way, to keep one artifact per document (each
revision is a new version with `--id`), to write the repository file too when
the document belongs there, to keep one artifact per topic (merging overlapping
ones with `--supersedes` rather than leaving pointer stubs), and never to put
these documents in an external
document tool or connector (Claude Docs, Notion, Google Docs…) unless the
person asks for that tool by name. An agent credential may call `ListArtifacts`, `GetArtifact`
and `PutArtifact` only for **its own execution's workspace**. Its versions
are always recorded as that agent, with source `agent`, whatever the request
claims. It may also call `RetireArtifact` (and `PutArtifact` with
`supersedes`) there, to retire, never to restore, and only artifacts agents
published (see below). Every other artifact call is owner only, including
`DeleteArtifact`, and a workspace terminal's credential can call none of them
(`internal/transport/auth.go`).

## Retiring and merging

Agents revise their thinking: a milestone plan, then a "what's left"
checklist, then "limitations and what's next", each overlapping the last.
The fix is one artifact per topic, and a way to get there that is safe for
the person: **retiring**.

- **Retire** (`RetireArtifact`, `lazybox artifact retire ID`) archives an
  artifact. It keeps every version and stays readable by its ID, but leaves
  the default list. It may name the artifact that **supersedes** it, of the
  same workspace, and give a one-line reason. Retiring one already retired
  records the new retirement.
- **Merge in one step** (`PutArtifact` with `supersedes`, `lazybox artifact
  put … --supersedes ID,ID`) publishes the artifact (new, or a new version
  with `--id`), then retires each listed one as superseded by it. Every
  listed artifact is checked first: if one may not be retired, nothing is
  published. An artifact cannot supersede itself.
- **Restore** (`RetireArtifact` with `restore`) brings it back as it was.
  Only the person restores.
- **What an agent may retire:** only an artifact of its own execution's
  workspace that **agents published**: every version authored by an agent,
  and no tracked file. One the person made, edited, took from a message or
  tracked from a file is theirs; the Runner refuses (`PermissionDenied`, with
  the title, telling the agent to ask). An agent never deletes:
  `DeleteArtifact` stays the person's (⋯ → Delete…).
- **The CLI prints** `Retired “Title” (artifact ID), superseded by “New”
  (artifact ID2).` for each artifact retired, which is what the
  conversation's card reads (`retiredArtifacts` in `tool-lines.ts`).
- **The latest plan** a review is briefed with (`reviewPlan`) skips retired
  plans.

In the app:

- **The conversation.** A put that superseded others shows one card,
  "Merged 3 artifacts into “Status”" (an update of one of them counts it
  too; one replaced by a new one reads "Replaced “Old” with “New”"), listing
  what it retired, with **Undo** (restore them all) and **Open**. A
  `lazybox artifact retire` shows "Retired “Title”", with what superseded it
  and **Undo**. Once restored the card says "Undone: … back in the list".
- **The Artifacts panel** lists only live artifacts. When some are retired,
  **Show retired (N)** under the list shows them in a *Retired* group (quiet
  rows, "Retired · superseded by “Status”"), reached by ↑/↓ like any row; the
  choice is kept per panel. A workspace whose artifacts are all retired says
  so. The palette, the header chip and "All workspaces" never list retired
  artifacts.
- **The page** of a retired artifact opens with a banner: "Retired:
  superseded by **Status**" (the title opens it), who retired it and when,
  the reason, and **Restore**.
- **Retire and Restore** are also in the page's ⋯ menu and the row's menu,
  for any artifact of the workspace. Neither asks first: nothing is deleted.

## Lazybox artifacts by default

A Claude agent signed in with a claude.ai account also gets that account's
**claude.ai connectors** (Claude's cloud MCP servers: Claude Docs, Gmail,
Notion…), and with them a way to write a plan somewhere other than Lazybox.
**Settings → Agents & workspaces → Use my claude.ai connectors in Lazybox
sessions** is off by default. Off, each new Claude session starts with the
Claude Code setting `disableClaudeAiConnectors: true` in its flag settings
layer and `ENABLE_CLAUDEAI_MCP_SERVERS=false` in its environment (the older
spelling, for a configured Claude binary that predates the setting)
(`workers/claude-sdk/src/options.ts`). The setting is read when a session
starts or resumes; a running session keeps what it started with.

What it does not cover:

- MCP servers configured on this computer (user or project settings,
  `.mcp.json`, plugins) still load: they are the person's own choice.
- Codex has no claude.ai connectors; its sessions are unchanged.
- Nothing changes on claude.ai: the connectors stay connected there and in
  other Claude Code sessions.
- A setting that already disables connectors (for example in
  `~/.claude/settings.json` or managed settings) wins even when the switch is
  on: any source that disables them disables them.

## The Artifacts panel and page

**The panel.** **Panels → Artifacts** (workspace header), `] ] a`, ⌘K
*Show artifacts*, or the chat header's "N artifacts" opens it (see
[Finding an artifact](#finding-an-artifact)). The workspace commands open the full workspace list. The chat header opens
a list filtered to that conversation. Tracking something also opens the panel.
The list shows each artifact's kind
icon, title, kind, latest version, who made that version, when, and the file
it follows, if any. A dot marks an artifact **updated since you looked**: a
version you did not make, newer than the last one you opened on this device
(kept per workspace in the window's view state). A filter appears when there are more than four
artifacts. The keyboard works the list:

- ↑/↓ (or j/k) move.
- Enter opens.
- Delete asks before deleting.
- The menu key, or right-click, opens the row's menu.

When the panel is wide, the list and the open page sit side by side. When it
is narrow, the page replaces the list and **Back** returns to it. The panel
docks, collapses and closes like any other panel.

**This workspace / All workspaces.** With more than one workspace, a
segmented control under the count switches the list (← → move between the
two; the choice is kept per panel). **All workspaces** lists every
workspace's artifacts grouped by workspace, the workspace changed last first
and each group newest first; ↑/↓ run across the groups, and choosing one in
another workspace reads it here. Delete is offered only for this workspace's
own. With none here but some elsewhere, the empty list says how many and
offers **Show all workspaces**; with none anywhere it says in one sentence
how artifacts appear.

## Finding an artifact

An artifact belongs to its workspace, but it is reached from where the person
is:

- **In the conversation.** When an agent's shell command runs
  `lazybox artifact put` (however the CLI is called) and its output has the
  CLI's "Published “Title” as artifact ID, version N (kind)" line, the
  conversation shows a card under the tool calls, at every activity detail:
  the kind's icon, the title, "Plan · v3 · updated by the agent", **Open**, and
  for an update **Compare with v2**. Open shows that version in the workspace's
  Artifacts panel (the latest follows new versions; an older one says "You are
  viewing v1 of 3"); Compare opens Changes on the two. A message tracked or
  updated from shows the same card under it ("tracked from this message").
  In a chat window of its own, with no workspace beside it, Open shows the
  page in that window, beside the chat (see *A chat in its own window*
  below). The card is read from the tool output
  (`publishedArtifacts` in `tool-lines.ts`): the Runner journals no artifact
  event of its own, so a put whose output was lost shows no card, though the
  artifact is in the panel.
- **The chat header.** "3 artifacts" is one of the header's number chips
  (`artifacts` in `header-layout.ts`): it counts the artifacts this
  conversation made or updated, or, when it made none, the workspace's, and
  is absent when the workspace has none. Like the other numbers it folds whole
  into ⋯ when the header is narrow. It opens the Artifacts panel on this
  conversation's artifacts, with a "From this conversation" pill whose ×
  shows the whole workspace again. In a chat window of its own it opens them
  beside the chat, in that window.
- **A chat in its own window** (`DetachedArtifacts.tsx`). Everything that
  opens an artifact there — a card's Open or Compare, the header's chip —
  shows it in that window: the Artifacts panel (list and page, one at a time
  when narrow) on the chat's right, resizable from its edge (remembered for
  the window), closed with × or Escape, the focus going back where it was. On
  a window narrower than 720 px it covers the chat until closed. **Open in
  main window** asks the main window to show the same artifact in its
  workspace's Artifacts panel and brings it forward (a navigation request,
  like the palette's). Send to agent fills this chat's message box; Open
  source message scrolls this chat when it is this conversation, and asks the
  main window otherwise. A file link in that window already opens in the
  quick-look window there.
- **⌘K.** The palette lists the artifacts of **every** workspace, newest
  first, with the kind, the version and the workspace ("Spec · v1 · Beta
  release"); a workspace's name finds its artifacts. `*` narrows the palette to
  artifacts. Choosing one in another workspace switches to that workspace and
  opens it in its Artifacts panel.

The lists are read with `ListArtifacts`, one workspace per call: one list per
workspace is shared by the panel, the chat, the chip and the palette
(`useArtifacts.ts`), reread every 4 seconds while the panel shows it and every
15 seconds otherwise, and only while the window is visible. Other workspaces
are read only while the palette or "All workspaces" is open, or to count them
for an empty workspace.

**The page** is a reader. A slim toolbar holds the kind (its icon and name;
choose another to change it), the workspace, Document / Changes, History,
Send to agent, Export, Open in its own window and ⋯; once you scroll past the
title, it names the document. The kind is shown there only, never again as a
pill in the title block.

- **The title block** sits 24 px under the toolbar: the title in the display
  face, with responsive sizing (double-click to rename, Enter to save,
  Escape to cancel), then one small line: who made the shown version, when,
  the **version picker**, and the reading time. The page shows one title: a
  leading `# Title` that repeats the artifact's title, or nearly does (a
  "(final)" or a subtitle added, a word changed: `repeatsTitle` in
  `artifact-reader.ts`), is hidden and the artifact's title stands. "Synced
  with `path`" and a tracking problem sit above the title.
- **The document** aligns its title, headings, prose, tables, code and diagrams
  on one left edge. Prose uses a measure of up to 76 ch, in Inter at 16 px
  with 1.7 line height and responsive gutters; headings with anchors, Newsreader for titles
  (both bundled, so the desktop app needs no network). Blockquotes that open
  with `> **Note**`, `**Tip**`, `**Important**`, `**Warning**`, `**Caution**`
  or `**Decision**` (or GitHub's `> [!NOTE]`) become callouts; a bold title
  after the kind (`**Decision: reads first**`) becomes the callout's title.
  Tables sit in their own frame with a sticky header and quiet zebra rows, and
  a cell that reads as a status or a level (Done, In progress, Planned,
  Blocked, High, Low…) becomes a pill. Task lists show checkboxes; code
  blocks their language and a copy button on hover; Mermaid diagrams render at
  once, fit the column, and keep zoom, pan and full screen behind a toolbar
  that appears on hover; footnotes jump both ways.
- **On this page**: when the reader is at least 900 px wide and the document
  has three or more sections, the outline (200 px at most) sits beside it and
  follows the reading position; on a narrower reader, beside a chat or on a
  phone, it is the **On this page** button at the end of the title block's
  line, and the text takes the outline's room. Nothing scrolls sideways down
  to 360 px but a wide table, in its own frame.
- **History** is closed until you open it: the toolbar's History button (with
  the version count) or **H**. Each window remembers whether it is open. It is
  a timeline of every version with its author and time; choose one to read
  it ("You are viewing v2 of 5 · What changed since · Back to latest"), or
  **Compare with vN** to see what it changed. It sits beside the reader on a
  wide page, which gives it its room, and over it on a narrow one.
- **Changes** compares the shown version with any earlier one. **Rendered**
  (the default) shows both versions side by side, rendered: unchanged blocks
  once on each side (long runs fold into "N unchanged blocks"), a replaced
  block beside its replacement with the changed table rows, list items and
  paragraphs marked, added and removed blocks tinted. Narrow, the two sides
  stack. **Text** shows the line diff. The choice is remembered per window.

The page follows new versions by itself.

**Page actions:**

- **Send to agent** puts a reference in the message box of the conversation
  beside the page. It is never sent for you. The reference has the title,
  the kind, the version, an outline of the headings, and the
  `lazybox artifact get ID` command so the agent can read the whole text. In
  its own window, **Discuss in chat** opens a resizable chat beside the
  artifact and puts a reference to the selected version or excerpt in the
  draft. It starts with the source conversation when available, and you can
  switch to another conversation in the workspace or start one explicitly.
  **Copy artifact reference** is also available in the artifact actions menu.
- **Open in its own window** opens the page in its own native window on
  desktop (`open_artifact`, one window per artifact) or browser window on
  the web (`?view=artifact`). Opening the same artifact again focuses its
  existing window and preserves the version and reading position.
- **⋯** has Rename, Open source message (opens the conversation and scrolls
  to the message, with the usual highlight), Open source file, Stop syncing
  with file, Retire (or Restore, when retired), and Delete… (asks first).

## Export

The **Export** menu has:

- **Copy as Markdown**: the selected version.
- **Download .md**: saves the selected version, named after the title, for example
  `datasource-migration-plan.md`.
- **Download .html**: saves the selected version as a standalone HTML document
  with inline styles and embedded rendered diagrams. HTML artifacts download
  their original source.
- **Print or save as PDF…**: prints the rendered document alone, as a
  document: a title block (kind, title, version, date, author, workspace),
  22 × 20 mm page margins, page numbers, light callouts, tables whose rows do
  not split, code that wraps, and no app chrome, buttons or outline. It waits
  briefly for a diagram still being drawn; a diagram drawn in a dark theme
  prints as its light negative. "Save as PDF" is the system print dialog's own
  option. The desktop app opens the native print dialog.
- **Save to repository…**: pick a workspace repository and a path (by
  default `docs/<title>.md`). The file is written but never staged or
  committed, so review it in Changes. Replacing an existing file is a
  second, explicit confirmation, made against the version of the file you
  saw. **Keep the artifact in sync with this file** (on by default for an
  artifact that is not yet synced) then tracks that file.
- **Create GitHub issue…**: opens an issue in a workspace repository's
  GitHub project through the GitHub connection. The body is the artifact
  plus one line naming it and its version. Clicking **Create issue** is the
  explicit go-ahead for that record.

## Orchestration

The mission planner accepts proposed tasks and dependencies. Approving that proposal and starting work are separate operations (see ORCHESTRATION.md). The artifact store accepts pinned design and review inputs. Direct mission/task brief and evidence links are reserved for a later integration.

Agents can publish a derived roadmap with `lazybox artifact put plan.md --title "Implementation roadmap" --kind plan --design ID@VERSION --review ID@VERSION`. Either input can be used alone. To continue work, update the saved plan by ID; later revisions keep its original inputs.

Use `lazybox artifact template pr-walkthrough`, `incident-timeline`, or `release-checklist` for editable JSON starters. The first two use the report schema. The checklist uses the existing interactive to-do renderer. Fill in real evidence and publish the file with the normal artifact command.

## Known limits

- **Every workspace is one call each:** "All workspaces" and ⌘K call
  `ListArtifacts` once per workspace. That is fine on one machine with tens of
  workspaces; a Runner with hundreds, or a remote one, would want an
  all-workspaces `ListArtifacts` (additive in the proto, owner only, since an
  agent's credential is limited to its own workspace).
- **A workspace window fixed to one workspace** opens another workspace in its
  own window from ⌘K; that window does not open the artifact chosen.
- **HTML artifacts** export their original HTML. The print action is
  available for rendered documents, including structured documents.
- **Desktop export** opens the system Save dialog for Markdown, HTML or JSON.
  The browser uses its standard download flow.
- **Page numbers** use CSS page margin boxes (`@bottom-center`). They are
  tested in Chromium (`npm run test:artifact-reader` prints the sample design
  doc to `.cache/artifact-reader-print.pdf`); WebKit, the macOS desktop
  webview, is not verified and may print without them.
- **The rendered comparison is by block:** a moved paragraph shows as removed
  in one place and added in another, and footnote references inside a
  compared block do not link across blocks.
- **Comments anchored to paragraphs** are not built.
- **GitHub issue creation is tested only against a local test server:** the
  client call is tested that way, and the handler is tested only for
  refusing when GitHub is not connected. It has not been run against GitHub
  itself.
- **Not tested across a Runner restart:** file tracking resumes from the
  stored artifacts when the Runner opens, but this has not been exercised.
- **The message right-click menu lives in `MessageArtifactActions.tsx`:**
  it is a small handler of its own. When the app-wide context menu lands,
  its items move there with one registration under the `message` extension
  point.


The reader limits paragraph width on large displays and gives diagrams, code
blocks and tables more room. The outline beside the document can collapse.

## Review findings reports

The Code review and Security review conversation templates publish a report
when the reviewer reports its coverage. The version carries the
`lazybox.review-findings/v1` schema identifier. Its Markdown includes a JSON
block with the captured Git revisions, coverage, and findings with stable IDs
and source locations. The review card opens the original result version.

### Reviewing shell commands

In a Markdown artifact, label executable examples with a shell fence such as
`sh`, `bash`, or `zsh`. **Review in terminal** opens a terminal below the
document and puts the command at the prompt. Press Return there to run it.
The artifact can be in the workspace or in a detached window. For a
multi-line block, the shell needs bracketed paste so every line stays at
the prompt until you submit it. Copy remains available for other shells.

By default, the terminal starts at the workspace root. To target a home
folder or checkout, put **Run in your home folder** or **Run in the home
checkout of REPO** directly before the block. The same destination
instructions work with a standalone inline-code command. HTML previews and
version comparisons remain display-only.
