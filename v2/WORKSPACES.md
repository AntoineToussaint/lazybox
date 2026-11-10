# Workspaces and repositories

A **repository** is the project you work on: for GitHub, its `owner/repo`
identity. Checkouts, clones and worktrees are storage used by the Runner, not
separate repository choices.
A **workspace** is a named working area containing one or several repository
worktrees, with multiple independent conversations attached to that work. A named
empty workspace is also available for planning before any repository is needed.

GitHub App installation and the GitHub CLI are **not prerequisites** for local
repository setup. Git itself supplies branches and worktrees. The optional
GitHub API connection supplies repository discovery/import and issue/PR lists.

## Create or extend a workspace

1. Open **New workspace** and search by organization or repository name. The
   shared catalog includes registered sources, saved local scans and cached
   GitHub discovery. The same catalog appears in Settings → Repositories.
2. Select repositories. Selection does not clone or register anything. Previously
   selected repositories remain visible as removable chips while filtering.
3. Accept or edit the workspace name, then choose **Create workspace**. The Runner
   prepares only the selected sources and creates their worktrees. Progress is
   streamed; errors preserve the selection and request identity for retry.
4. Use a workspace's **Add repositories…** action to open the same picker with an
   explicit destination. Existing members are marked **In workspace**.
   Conversations and existing worktrees remain in place.

**Connections & folders** expands inside the picker: connect GitHub or choose a
folder to discover existing checkouts. There is no registration/import step and
no second repository browser in GitHub settings. Folder scans are explicit and
saved; reopening a picker does not rescan the filesystem. An empty workspace is
an explicit secondary choice. Naming policy remains configurable in Agents &
workspaces, outside the creation flow; the immutable disk slug is independent.

Discovery never clones. GitHub metadata survives closing windows and Runner
restarts. Local and remote observations of the same normalized `owner/repo` merge
into one choice; prepared sources take precedence over local candidates, then
remote-only entries. Forks remain distinct. A failed or incomplete GitHub refresh
retains the last complete snapshot and reports the source error. A changed GitHub
identity does not expose another account's cached remote discovery.

Repository selection is keyed by logical identity. The Runner resolves checkout
paths and revalidates remote identity during preparation. The default base is
`HEAD`; existing checkout contents and branches are not changed. Default folder
names use the repository name, adding a numeric suffix only for collisions.
Explicit folder names still require uniqueness. Successful source preparations
remain reusable after a later failure; retrying the same request does not clone
again or create another workspace.

## Catalog boundaries and configuration

`repositorycatalog.Source` discovers metadata; `repositorycatalog.Store` persists
complete provider snapshots; `Cache` separates refresh synchronization from reads.
The Runner merges this with existing local discovery and repository registration.
`PrepareWorkspace` owns source preparation and collection provisioning. The shared
React catalog reader/picker owns filtering and selection only. Existing low-level
RPCs remain available for compatibility, but the UI no longer chains imports.

Runner JSON configuration may contain:

```json
{"repositoryCatalog":{"autoRefresh":true,"refreshSeconds":300,"maxPages":1000,"githubEnabled":true}}
```

These are the defaults. `autoRefresh: false` makes GitHub refresh explicit;
`githubEnabled: false` disables GitHub discovery without disabling local sources.
`maxPages` bounds each pagination sequence. A limit or provider failure is an
incomplete refresh, never a successful partial catalog. Remote discovery is also
bounded by a 90-second timeout. Metadata lives in `repository-catalog.json` under
the resolved Runner state directory; credentials remain in the existing private
connection store and never enter the catalog. Automatic refresh occurs while a
repository browser is open, at most at its configured freshness interval.

The Runner resolves one source checkout or bare repository behind that identity.
Git can resolve this source outside a scanned folder through a linked worktree.
Already registered sources take precedence; existing workspace IDs and worktree
ownership remain valid when older registrations refer to multiple clones.
GitHub import reuses an already registered source for the same GitHub project.
Older location-based caches retain their chosen folders but require an explicit
refresh. The normalization reads origin configuration without a network request;
unknown SSH aliases and GitHub Enterprise hosts are not guessed. Renames across
old and new remote names cannot be reconciled offline by this identity model.

It skips descendant symlinks, hidden folders, dependencies and build output, and
stops descending when it finds a Git checkout or bare source. Each scan is bounded
by one minute, 10,000 directories, six subdirectory levels and 512 candidates;
partial results report limits and skipped folders. Up to 16 search roots are
retained in the isolated Runner state. Failed scans retain previous results.

Registration performs local Git inspection only. It neither contacts a remote
nor reads, copies or changes authentication. It preserves the source checkout's
files, current branch, Git configuration and permissions. Workspace creation
adds separate worktrees and new branches through the existing Runner manager;
uncommitted changes in the source are not copied into the new worktrees.

Repeating registration through another clone or worktree of the same GitHub
repository returns the existing library entry. Local repositories without a
recognized GitHub origin use their canonical Git common directory instead. The library persists across Runner restarts. A checkout needs an
initial commit; subfolders and `.git` paths are rejected rather than silently
registering a parent repository.

New workspace folders use a unique three-word petname generated once. The display
name can be edited in its sidebar row with **Rename workspace** (Enter to save,
Escape to cancel). The editor retains its original workspace identity if selection
changes, without changing its folder, member
paths or branches. Existing workspace folders remain in place. Double-clicking a
repository selects it while retaining other selections; checkboxes support
multi-repository selection. The suggested display name follows selected repository
names (for example `api + web`) until the user types a custom name. Deselecting
and reselecting updates an automatic suggestion. **Settings → Agents & workspaces**
sets whether suggestions use repository names or a memorable generated name.
Display naming never renames the fixed disk slug or branches.

Example workspace:

```text
gently-bright-otter/
  api/    # branch: feature/gently-bright-otter
  web/    # branch: feature/gently-bright-otter
```

## Sessions, windows and modules

**New session** exposes account and permission choices directly, with model,
gateway and additional context under an expandable section. **Quick start** starts
once from an explicit click using the last successful provider/account/model/gateway
selection for this workspace. Choices are shared by its panels/windows. A refresh
never starts an execution. One-off permissions, context, and rotation pools are not
remembered; Runner defaults apply unless chosen explicitly in New session. Quick start pauses for account selection if its saved profile is unavailable.
With no saved choice, setup suggests a configured managed account, then the first
profile of that provider. In-flight or resumed sessions are not changed.

All sessions in one workspace use the same working folder, branches and files.
They retain independent transcripts, provider identities, commands and continuation
chains. Use different workspaces when changes need separate files/branches.
The Runner tracks each attached execution, including commands that outlive an
agent. Closing one session does not release another session's ownership.

**Open workspace window** opens the whole workspace in a native window that can
be moved to another screen. It retains the workspace while allowing conversation
switches. Closing the window does not end sessions. Each workspace has its own
saved pages, panel arrangement and drafts; changing another workspace does not
replace them. The main window and that workspace's window use its saved layout.

The shared surface accepts a registry of trusted, build-time `PanelModule`s.
Modules receive the selected workspace ID, view target and Runner context.
Chat, repository navigation, output, sessions and application assistants use that
same interface. Unknown saved modules retain their configuration. This is a UI
extension seam, not a downloadable plugin sandbox or an orchestration scheduler.
Future orchestration modules must call Runner-owned execution APIs; window
lifecycle and layout must never own agent lifecycle.

### Agent telemetry and workspace accounting

The Runner now owns a streaming metering gateway, a SQLite usage ledger and
provider-neutral context snapshots. The **Usage & cost** module can be added to
any workspace page; chat has a compact context/usage disclosure. Select
**Metered · track workspace cost** in a new session's gateway options. Direct
sessions retain provider-reported observations where supported.

Records retain workspace, conversation, execution, native-process attempt and
account attribution across continuations and rotation. Workspace application
assistants retain the workspace ID too, with their existing read-only restrictions.
Forwarded child requests count toward their workspace even when the CLI supplies
no reliable child identifier; the UI explicitly marks that attribution unavailable.
See [the telemetry contract](INTERFACES.md#telemetry-and-metering) for pricing,
partial observations, persistence and supported protocol limits.

The UI uses Workspace. Existing `Collection` RPC/type names and
`.lazybox-collections` metadata remain internal historical names. Version 4
manifests retain immutable folder slugs, multiple ownership claims and repository-addition retry records;
older manifests load without moving their folders. Older binaries do not support
these manifests, so use an updated Working build after this upgrade.
The [storage and ownership contract](COLLECTIONS.md) describes those internals.

## Review and conversation continuity

**Review changes** opens a workspace page using the same module registry as chat
and output. The Changes module resolves members on the Runner, verifies their
Git registration, and shows local index/worktree status and bounded diffs. It is
also available through Customize page. Different repository failures stay
independent. GitHub API access is not required for this local inspection.

Conversation names belong to the durable conversation, across native resumes.
Automatic titles come from the first attempt; a follow-up after resuming does not
replace that title. Explicit renames override the automatic title.
Selecting a workspace restores its saved conversation selection; it does not
choose a different open execution from the Runner catalog. **Conversation history**
searches titles across all workspaces, including conversations retained after deletion.
Reading history never starts an execution. For a normally stopped, resumable
conversation, send the next message to continue using its saved account, model,
gateway and permissions. Uncertain process ownership still requires explicit
reconciliation. Views anchored to an earlier attempt follow its recorded successor
without creating another execution. Drafts and send identities are retained before
resuming delivery.

Chat folds streaming deltas into complete messages independently of the diagnostic
event buffer. It renders the latest 100 messages initially; **Show earlier messages**
reveals retained messages while preserving the reading position. Incoming output
only scrolls when already following the bottom; **Jump to latest** resumes following.
Replay gaps and terminal stream errors appear in the chat with **Retry conversation**.
They never masquerade as a new, empty conversation.

Chat cost totals include all execution attempts in the conversation. Context uses
the latest reported snapshot per agent; workspace cost aggregation remains separate.

Local Markdown links and inline `path:line` references open a read-only file preview.
The Runner resolves absolute or workspace-relative paths inside the selected workspace;
a single-repository workspace also accepts repository-relative paths. Preview is
bounded to 1 MiB, rejects traversal and escaping symlinks, and reports binary or
missing files explicitly. Markdown can be rendered or read as source, and line
references highlight their target. **Open file** in Changes uses the same preview.

Sidebar filtering is scoped on the Runner before pagination, and continuations
retain the original creation order. Native personal views use the persistent
Runner identity and private native storage, so restarting with a new local port
or Vite origin no longer strands pages and draft keys.

## Current limits

The current UI discovers and registers existing Runner checkouts. The native
folder chooser belongs to the local desktop Runner; remote filesystem browsing
and generic clone-by-URL are not implemented in this slice. Use an existing clone, or clone using your normal Git workflow before
adding it. This does not grant a new GitHub API application access to an
organization: issues/PR API access remains a separate authorization decision.

## Dogfood interaction rules (2026-11-03)

The default page is **Work**. The unused built-in Monitor page is removed when
restoring old layouts; other customized pages and their selected conversations
are preserved. The sidebar is a workspace list with conversations nested under
only the selected workspace. Standalone conversations have an explicitly named
**Without a workspace** section, hidden when empty. Repository details describe
the selected workspace; rename and delete live in the always-visible actions menu
on each workspace row. Existing conversations precede search and new-conversation
actions. Search is explicitly scoped to conversation titles within that workspace
and is hidden for a single conversation. Counts say
`repo`/`repos` rather than displaying an unexplained number. A selected conversation
shows its account, while the conversation switcher uses titles.

The UX principles here are scope first, one visible hierarchy, and progressive
disclosure: everyday work is workspace → conversation; Git locations and technical
request metadata stay in details. New conversation belongs to the selected
workspace. Background execution is never inferred from a window's visibility.
No monitoring/dashboard workflow is added without a concrete use case.

Chat, Help and Planning share the Markdown renderer: headings, lists, tables,
inline/fenced code, JSON highlighting/copy and Mermaid diagrams. Approval cards
show the tool description and command first, with structured request metadata in
an expandable JSON view. Diffs use the same code presentation. Raw HTML remains
inert and message images do not fetch external URLs. Mermaid loads only when a
diagram is present, renders in strict mode into an SVG image, and falls back to
copyable source on invalid/incomplete input. Console logs retain exact raw text.

Activity appears beside the composer while a turn runs: Thinking, Running a tool,
Writing response, or Compaction when the provider reports that phase. Unknown
activity stays generic Working; no hidden reasoning is displayed. Root completion
and background-agent activity remain distinct. Connection recovery has its own
label. This transport is Connect streaming RPC, not SSE; text deltas are rendered
before stream completion.

## Delete and resource cleanup

**Workspace actions → Delete workspace** uses a confirmation button without name
typing. It previews active executions and local
files, states the detected risks, and asks explicitly before stopping sessions
or discarding changed/untracked/ignored files. Cleanup first fences new workspace
attachments, closes owned provider processes, cancels and waits for owned commands,
then removes each worktree through Git and removes the workspace folder. Source
repositories, branches (including committed work), accounts and conversation
history remain. The destructive operation is not performed when opening the dialog.

A durable deleting state supports retry after partial removal; deletion never
silently becomes success when a process, source identity, root path, or Git
registration cannot be verified. Unknown ownership after a Runner crash requires
reconciliation. External pipe producers are not Runner-owned processes and cannot
be declared stopped by cancelling a log reader. Tombstones prevent retried create
requests from resurrecting deleted workspaces. Deleted workspaces are hidden from
the workspace list, and their conversation history cannot resume into missing files.

**End session** in chat uses `StopSessionResources` to stop that provider and its
owned commands. Closing a window still only disconnects its readers. Closed agent
sessions release the concurrent execution cap; retained history does not consume
an active-session slot. The existing low-level `CloseSession` RPC still ends only
the provider, for callers deliberately keeping managed commands alive.

Each agent runs in its own [session host](SESSION_HOSTS.md), not tmux. Window
reloads reattach readers to the same execution. A Runner restart re-attaches to
the same native process and to the commands it started. Only a machine reboot, or a host the Runner cannot
adopt, falls back to native conversation resume.

Context usage opens in a bounded popover so inspecting it does not displace chat.
Category colors match the segmented capacity bar; compaction reserve and free
space are separate from used context. Deferred tools are listed outside context.
Provider estimates retain their provenance; token cost is a separate disclosure,
with API-equivalent estimates distinguished from subscription allowance and bills.

### Navigation and file editing

The sidebar contains workspaces and their conversations. Repository membership is
managed from a workspace's **Manage repositories…** action or its repository count
in the header. The add dialog names the destination workspace. Renaming changes
only presentation and does not reset the Runner connection or transcript reader.
Conversation rows show provider, reported model (or an explicitly requested model, with missing reports labeled
as unknown), account label, and status. **Conversation history** searches titles across
all workspaces; it is not a full-text transcript search.

Each window caches the last 16 inactive conversation readers, retaining the
projected transcript, cursor, reading position and visible message count. Reopening
one reconnects after its cursor. Provider execution remains owned by the Runner;
this cache does not launch, resume or keep an agent process alive. Restarting the
application still reads durable Runner history.

File references open a tall, draggable, resizable surface with Markdown preview
and a locally bundled Monaco editor. Editing is explicit. Save compares the loaded
revision with the file on the Runner and rejects a changed file while retaining
the draft. Lazybox edits serialize per workspace manager and use a same-directory
atomic replacement with existing mode bits; external writers do not participate
in that lock. Binary, truncated, symlink and read-only files cannot be edited.
Unsaved drafts require an explicit discard before closing or refreshing.

**Delete conversation…** appears in conversation row actions and the chat session
menu. It stops and waits for owned execution when explicitly confirmed, releases
workspace ownership, removes Lazybox attempts/transcripts/uploads and retains
identity-only deletion tombstones so legacy import cannot resurrect them. Uncertain
process ownership blocks deletion. Workspace files, subscriptions, accounting,
command audit logs and the provider's own history remain. **End session** retains
conversation history; **Delete workspace** has its own file/resource check.


The chat follows its rendered content height, including delayed diagrams and
viewport changes, while the reader is at the bottom. Scrolling up suspends follow;
returning to the conversation restores that reading position. **Jump to latest**
and sending a new message resume follow. The header shows the provider-reported
main-agent model, captured from startup/context/usage events as well as the saved
session metadata. Internal account IDs such as `claude-default` are presented as
“Local CLI sign-in,” separately from the model.

Sidebar navigation shows the workspace name with its repository identities underneath. Conversations use compact title/model rows; the account is shown only when a named account is known. A closed execution is labelled Saved, not an ended conversation. Action menus remain inside their rows. Deletion initially focuses Cancel; draggable headers retain keyboard movement without a hover instruction popup.

Resuming preserves the mounted transcript and its projected messages. The successor reader inherits the predecessor’s contiguous timeline cursor and view state, then reads only the new events. An unrelated execution never inherits another conversation’s transcript.

## Compact navigation and keyboard controls

A workspace with exactly one known conversation is one sidebar entry. Clicking it
opens that saved conversation; it never starts an execution. Multiple
conversations appear as children only in the selected workspace. Repository names
stay on the workspace row. The plus icon beside its name opens a new conversation;
the row menu distinguishes deleting the conversation from deleting the workspace.
The sidebar loads one paginated history index independently of selection, merging
live execution updates and retaining its last complete history on refresh failure.

Settings → Shortcuts offers Vim, tmux and arrow presets and editable bindings.
Space separates a sequence; `Ctrl+b` denotes a chord. The default `] ]` leaves the
composer for navigation, `] ] s` opens snippets, `] ] c` opens a new conversation,
and `] ] w` creates a workspace. In navigation, `j/k` move, `h` focuses the parent
workspace, and `l` opens the entry and focuses its composer. The tmux preset uses
`Ctrl+b` followed by arrows or `s/c/w`. A pending hierarchy shows available actions.
A single `]` remains draft text. IME composition and ordinary form fields are not
navigation targets. Bindings are client preferences persisted through the host's
view-state store; they do not change the Runner's permissions or agent settings.

## Snippets

The composer snippet icon and the configurable snippet shortcut open the same
searchable picker. Preview the exact delivered body, then **Insert** (Shift+Enter)
or **Send snippet** (Enter). Existing draft text disables immediate snippet sending;
insertion preserves it. Only confirmed sends enter the five-item recent list.
The existing message/receipt and resume paths own delivery and retry identity.

The Runner's `snippets.Catalog` seam has a command adapter. The shipped
`lazybox-snippets` helper links the existing `lazybox-config` Rust library so built-in
content, profile paths, global/launch-directory overrides and skill-backed bodies
remain authoritative in one place. It is read-only. `snippetCommand` and
`snippetDirectory` in Runner configuration can select another compatible adapter
and launch-directory layer. An unconfigured adapter produces a visible error.
The native Dev launcher builds the helper; staged Working releases contain a frozen
copy covered by the source digest. No snippet is executed by the helper.
This restores catalog/preview/insertion/delivery; V1's tracker workflow badges,
artifact coordination, and broadcast/next-workflow automation are not implemented
in V2 by this change.

Repository preparation streams recognized Git transfer stages and numeric counts.
The overall progress bar is indeterminate because cloning and worktree creation
have different units; stage percentages are Git's reported values. Raw Git stderr
and credentials are never forwarded to the UI. Losing the stream cancels the
owned clone process group, and retries reuse successfully prepared repositories.

Command execution/output is supported. An interactive PTY shell with tmux-style
pane multiplexing is not yet implemented; keyboard presets do not imply one.

### Activity inspection and context evidence

The chat keeps one live phase line. **Activity** in the context bar opens tool
history; calls and results remain collapsed until requested. Claude tool results
are correlated with their call IDs; Codex items retain their start/completion
state. Sub-agent calls are attributed separately. Missing results say “Result
not reported”, never “Completed”. The client retains the latest 1,024 activity
events separately from its 80-event diagnostic tail and labels truncation.

Claude's `requesting` status means an API wait, not compaction. Only native
`compacting` displays that phase; a clear status or compact boundary clears it.
Context categories expand into provider-reported names and counts. Claude's
summary endpoint supplies estimates without per-category token-count API calls.
Tool inspection previews are bounded at the provider boundary and again in the client. No prompt bodies, tool schemas or absolute memory paths enter the context report.
Codex currently supplies window usage/capacity but no category detail; missing
categories remain unavailable. A context snapshot's observation time is visible.

Workspace rows show each repository on its own line. Right-click opens the same
workspace menu as the ellipsis; F2 starts inline rename. File links can browse
folders under the workspace, including references prefixed by its actual folder
name. File access remains confined by `os.Root`. Automatic inline-code links
require file-shaped text, so `origin/main` and `feature/example` stay plain code.

**Review changes** shows uncommitted changes. Unstaged diffs compare the index
with the working tree; staged diffs compare HEAD with the index. Untracked files
show their content. This view does not compare committed branch work with main.

Snippet search now includes the body. Favorites are saved in native view state
and can be filtered; built-ins/global/project content remains owned by V1's
catalog adapter. This is still a picker, not a create/edit/delete snippet manager.

Activity rows use provider action descriptions with tool names as secondary labels.
The newest calls appear first; search includes descriptions and tool identity, and
Running/Failed filters narrow the list. Timestamps are the Runner's recorded event
times, displayed in the viewer's local time. Elapsed time measures observed call
to result (including approval/provider waits); it updates while a call is running.
Missing timestamp pairs remain unavailable, and turn completion never substitutes
for a tool result. Older history with missing results has one explanatory note.
Commands use syntax highlighting/copy; full input JSON is a separate disclosure.
