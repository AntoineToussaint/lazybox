# Artifacts

Agents write plans, reviews and specs — in a chat message, or in a file such as
`docs/PLAN.md`. Track one as an **artifact** and it becomes a versioned page of
the workspace: compare versions, export it, and hand it back to an agent.

![The Artifacts panel with an artifact page open](../images/artifacts.webp)

## Track something

| From | How |
| --- | --- |
| **An agent's message** | Hover the message and click **Track**, or right-click → **Track as artifact…**. The title and kind (plan, report, spec, notes) are suggested, with a preview. When the message links a Markdown file, the dialog offers **Track `docs/PLAN.md`** (stays in sync with the file) first, chosen already for a short message that mostly names it. Later messages of the same conversation offer **Update "Title" from this message…**, which shows the diff before saving. |
| **A Markdown file** | In Files, right-click the file (or the editor tab, or the file's link in a message) → **Track as artifact**. Lazybox follows the file: each settled change becomes a new version, and the page says *Synced with `api/docs/PLAN.md`*. **Stop syncing with file** keeps every version. |
| **The agent itself** | Agents publish with `lazybox artifact put PLAN.md --title "Migration plan" --kind plan`; their briefing tells them to. |

## One artifact per topic

Agents revise as they go, and three overlapping documents are worse than one.
An agent keeps one artifact per topic: it publishes a new version of the same
artifact, or merges overlapping ones into one with
`lazybox artifact put STATUS.md --title "Status" --supersedes ID1,ID2,ID3`,
which **retires** the old ones. It can also retire one alone with
`lazybox artifact retire ID --superseded-by ID`.

Retiring is safe and reversible:

- A retired artifact keeps every version. Nothing is deleted; only you
  delete (⋯ → **Delete…**).
- An agent can retire only artifacts agents published. Yours, one you edited,
  and files you track stay put unless you retire them.
- The conversation says what happened: **Merged 3 artifacts into “Status”**,
  with **Undo** to bring them all back.
- The Artifacts panel hides retired artifacts. **Show retired (N)** under
  the list shows them.
- A retired artifact's page says **Retired: superseded by Status**: click the
  title to open the replacement, or **Restore** to bring it back.
- You can **Retire** or **Restore** any artifact yourself from its ⋯ menu,
  or by right-clicking its row.

## Read and compare

An artifact lives in its workspace, and you reach it from wherever you are:

| From | How |
| --- | --- |
| **The conversation** | When the agent publishes or updates one, a card shows under its steps: the title, "Plan · v3", **Open**, and **Compare with v2** for an update. A message you tracked gets the same card. |
| **The chat header** | **3 artifacts** counts what this conversation made (or the workspace's, when it made none). Click it for the Artifacts panel showing just those; the pill's **×** shows the whole workspace. It is not there when there are none. |
| **⌘K** | Lists the artifacts of every workspace, with the workspace's name. Type `*` to search only artifacts. One from another workspace switches there and opens it. |
| **The Artifacts panel** | `]] a`, or the **Panels** menu in the header → Artifacts. **All workspaces** at the top lists every workspace's artifacts, grouped by workspace. |

The list shows each artifact's kind, latest version, who made it and when. On the
page:

- **Document** — the rendered Markdown; Mermaid blocks render as diagrams.
- **Changes** — the diff between the shown version and any earlier one.
- **History** — every version with its origin (*you*, *the agent*, *file
  changed on disk*) and time.

The page follows new versions by itself. Rename by double-clicking the title.

## Use it

- **Send to agent** puts a reference in the message box of the conversation
  beside the page — title, version, an outline, and the command that prints the
  whole text. You decide when to send.
- **Open in its own window** for a second screen.
- In a chat you moved to its own window, an artifact opens **beside the chat,
  in that window**. Drag its edge to resize it; × or Esc closes it, and **Open
  in main window** shows it in the workspace.
- **Export:** Copy as Markdown, Download .md, Print or save as PDF, **Save to
  repository…** (written but never committed — review it in Changes), or
  **Create GitHub issue…** in a workspace repository.

Deleting a workspace deletes its artifacts; deleting a conversation does not.

## Preview an HTML artifact

Publish a self-contained page with
`lazybox artifact put preview.html --title "Deploy walkthrough" --format html`.
Update it with `lazybox artifact put preview.html --id ID`. The format is fixed
at creation and independent of the kind. Select a version to preview or
download its original HTML. **Changes** shows a source diff.

The preview supports inline styles, embedded raster images, and native HTML
controls such as expandable details. It runs in a sandbox with scripts and
external resources blocked. Repository file tracking uses Markdown.

Use **Source** to read the original HTML in Lazybox. For artifacts managed
directly in Lazybox, select an older revision and choose **Republish vN** to
make that content current while preserving the intervening history.

Select text in the Markdown reader or HTML source to **Send selection to
agent**. The draft includes the excerpt and a command to read the exact
revision you had open. In a standalone window, Lazybox copies that draft.
