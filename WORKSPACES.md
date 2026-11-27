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

1. Open **New workspace** and search by repository name. Include a slash to search by owner and repository, such as `acme/api`. The
   shared catalog includes registered sources, saved local scans and cached
   GitHub discovery. The same catalog appears in Settings → Repositories.
2. Select repositories. Selection does not clone or register anything. When creating
   or cloning a workspace, selected repositories appear above the searchable
   list of unselected repositories. Both sections use checkbox rows grouped by
   owner. Searching keeps the full selection visible.
3. Accept or edit the workspace name, then choose **Create workspace**. The Runner
   prepares only the selected sources and creates their worktrees. Progress is
   streamed; errors preserve the selection and request identity for retry.
4. Use a workspace's **Add repositories…** action to open the same picker with an
   explicit destination. Existing members are marked **In workspace**.
5. To remove one, open the workspace’s **Manage repositories…** action and
   choose **Remove…** beside the checkout. Confirm to remove the clean
   worktree while keeping its local branch and repository library entry.
   End active workspace sessions, terminals, and commands first. If a session
   blocks removal, the dialog identifies it. Commit or
   move changed, untracked, or ignored files, and finish or abort a rebase,
   merge, cherry-pick, revert or bisect, before removing the checkout.
   Ignored generated directories (`node_modules/`, `build/`, `dist/`, `target/`,
   `coverage/`, `.next/`, and `.cache/`) are removed with the checkout at any depth.
   Tracked edits inside them still block removal. Conversations and existing worktrees remain in place.

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
paths and revalidates remote identity during preparation. **A new workspace
branch starts from origin's default branch as it is now** (or from another
branch the person chose for that repository: [Start from another
branch](#start-from-another-branch)): before creating
the worktree (New task, Add repository, Start from an issue, an agent's
repository request, `lazybox new`, `lazybox create-collection --spec`), the Runner asks
origin which branch its HEAD names (never assuming `main`), fetches it into
`refs/remotes/origin/<branch>` and records `origin/HEAD`, then branches from
that commit — never from a source copy's stale branch, the branch a checkout
has out, or another workspace's branch
(`internal/runner/fresh_base.go`, `internal/gitpublish/refresh.go`,
`internal/runner/collections/base.go`). If origin cannot be reached and the
Runner has a usable copy of the default branch, it uses that commit and
reports when it last fetched it. A repository without origin starts from its
`HEAD`. An explicit base
(a branch to continue, an orchestrator's parent branch) is used as given.
Only remote-tracking refs change; existing checkout contents and branches are
not changed. Default folder
names use the repository name, adding a numeric suffix only for collisions.
Explicit folder names still require uniqueness. Successful source preparations
remain reusable after a later failure; retrying the same request does not clone
again or create another workspace. An empty Git repository needs an initial
commit before Lazybox can create its worktree. Create the commit, push it if
the repository has a remote, then retry the same request.

### Start from another branch

When the work belongs on a long-lived branch rather than the default one (a
new product line developed on `feat/v2-command-output` while `main` is the
released one), a repository's workspace can **start from** that branch of
origin instead (`internal/runner/start_from.go`):

- **New task.** Each chosen repository shows a quiet chip, **from main ▾**.
  Clicking it, or `⌥B` (the focused repository's chip, else the first), opens
  a searchable picker of origin's branches: origin's default first, then the
  branches chosen lately, the five most recently committed (*Active*), then all
  of them by name. Typing filters; a name origin has but the list does not
  show yet can be typed and chosen (*Start from `x`*), and the Runner checks it
  with origin. `↑`/`↓`, `↵` picks, `Esc` closes the picker (not the sheet).
  The list is asked of origin only when a picker opens (one `ls-remote`,
  nothing fetched) and reused for a minute; until then the chip uses what the
  Runner already has, without the network. A repository not on the Runner yet
  lists nothing; a typed branch still works.
- **Add repository** and **New workspace** show the same chips under the
  repository list, with the same picker and `⌥B`.
- **Default every time.** A new workspace starts from the repository's
  default branch, typically `main`. A branch chosen in an earlier workspace
  appears under *Recent* and must be selected again to use it. A repository
  with an unreachable origin can use the last known default, with a progress
  notice. If the Runner has no known remote default, it asks you to choose a
  branch. A local-only repository prefers `main`, then `master`, then its
  current branch if neither exists.
- **Fresh, with the same offline fallback.** The chosen branch is fetched
  first into `refs/remotes/origin/<branch>` (`gitpublish.RefreshBranch`;
  `origin/HEAD` is left alone) and the workspace branches from that commit.
  If origin cannot be reached it starts from the branch's last fetch and says
  so: *Couldn't update api; started from feat/x as of Nov 17 09:12*
  (`collections.BranchBase`). A branch this Runner never fetched cannot start a
  workspace offline, and one origin does not have fails the preparation with
  *api has no branch feat/x on origin: choose another to start from* — neither
  is guessed.
- **Elsewhere.** `PrepareWorkspace.start_from` maps repository identity to
  branch (an omitted repository, `""`, or the default branch's name uses
  origin's default). `lazybox new … --start-from
  B` (with `--from` for an issue too) applies B to every repository.
  `lazybox conversation new --workspace new --start-from B` starts the new
  workspace's repositories from origin's B, fetched first, instead of the
  asking workspace's branches; the person sees it on the request.

## When an agent needs another repository

An agent works only in its workspace's worktrees. When its task needs a
repository the workspace does not hold (another module, a library it must
change, a sibling service), its briefing tells it to ask instead of reading or
editing the repository elsewhere on disk:

```bash
lazybox workspace repos                      # this workspace's repositories, paths, branch
lazybox workspace request-repo acme/module-directory --reason "the parser imports its tokenizer"
```

`request-repo` takes a name, numeric GitHub ID (including `github-42`), `owner/repo`, a GitHub URL or a path (made
absolute). The Runner matches it against the [catalog](#create-or-extend-a-workspace)
— IDs and qualified names require an exact identity. Bare names allow alternate separators and small typos; a path inside a catalog
checkout finds that repository; a git folder the catalog does not know yet is
offered as itself — and records a **pending request on the asking
conversation**. It adds nothing. If the best match is already in the
workspace it says where, and records nothing; asking again for a repository
that is still waiting returns the same request.

The person sees the request where approvals wait: as a card in the
conversation ("Claude asks to add **acme/module-directory** to this
workspace", the reason, the match and where it comes from, the workspace
branch it will use, other matches to switch to) and as **Repository request**
in the Inbox. The card offers:

- **Add** (`y`): the workspace's own add path (`PrepareWorkspace` with the
  request's id), with its progress in the card. A retried click is the same
  addition. A folder outside the catalog is registered first, as **Choose
  folder…** does.
- **Choose another…** (`c`): the **Add repositories** picker, searching for
  the likely repositories with the match preselected.
- **Decline…** (`n`): with an optional reason the agent is told.

Adding the repository any other way (the workspace's **Add repositories…**)
also answers a request for it.

The command waits up to 90 seconds (`--timeout`; `--no-wait` returns at once)
and prints the outcome: the new worktree's path and branch (exit 0), the
person's refusal and reason (exit 4), or that there is no answer yet (exit 5).
The request stays open for 24 hours; an answer the agent is no longer waiting
for is sent into its conversation as a short message, queued behind a running
turn. A request outlives a Runner restart, and so does an agent waiting on one.

Agents can only ask, and only for their own workspace (`RequestRepository`
names the asking execution, from which the Runner takes the conversation and
workspace; `WaitRepositoryRequest` reaches only the execution's own requests;
`ListWorkspaceRepositories` only its workspace). Adding and declining stay the
owner's; a workspace terminal can do none of it. A conversation may have three
requests waiting at once and make ten an hour.

## Home checkouts

A repository's **home checkout** is the person's own regular clone of it, at a
stable path outside every workspace: where they run logins (`acme login`),
long-lived dev servers (`make playground`), one-time tool setup and manual
tests. Workspaces are temporary worktrees named after the task; a home stays.
User guide: [Home checkouts](docs/guides/home-checkouts.md).

**Where it comes from.** Settings → Repositories → Home checkouts, or the
workspace's repositories dialog, offers per repository:

- **Create**: a clone at `<homeCheckoutRoot>/<owner>/<name>` (Runner config
  `homeCheckoutRoot`, default `~/lazybox/repos`), or another path. The folder
  must not exist.
- **Use an existing folder…**: adopt a checkout the person has. It must be the
  top folder of a checkout of the same repository (same GitHub project or Git
  history, or a clone of the registered folder), outside the worktree root and
  the Runner state.
- A repository registered from a regular checkout (a local folder) has that
  folder as its home already (`home_kind: source`); a GitHub import's bare
  source copy is never a home.

**Why a clone, not a worktree.** A created home is `git clone --local` of the
Runner's source copy. Git hard-links the immutable object files, so it is fast
and cheap, yet self-contained: it survives the source copy and a `git gc`
there. A worktree of the source (`git worktree add`) would share refs, config
and branch locks with every agent's workspace (an agent's `git branch -f` would
move the person's branch, and the person's branch could not be checked out in
a workspace), and would break with the source. Borrowing objects through
alternates (`clone --reference`) would let a `gc` in the source corrupt it.
After the clone, `origin` is the source's own remote (the real GitHub URL; no
token), the remote-tracking refs are the source's view of that remote (never
an agent's workspace branch), and the default branch is checked out tracking
`origin`.

**What never happens there.** Lazybox never deletes a home (forgetting one,
removing the repository or deleting a workspace leaves its files), never
commits, resets or switches its branch, and reads its state with
`git --no-optional-locks status` so even the index is untouched. No workspace
operation reaches it (`TestAWorkspaceNeverTouchesTheHome`).

**Shells.** `OpenTerminal` takes `repository_home` (a repository id) or
`user_home` instead of a workspace directory: the person's login shell starts
there, still as a terminal of the workspace (its panel, its credential, its
cleanup; see [terminals](TERMINALS.md)). The Terminal panel's directory list
offers each member's home and **Home folder (~)**.

**Agents.** The briefing says a home is the person's and read-only for the
agent (read files and run read-only commands; never edit, commit, reset, stash,
pull, switch branches or start servers there), to use it rather than a
temporary clone to run a repository's own tools at the latest main, that
commands needing a terminal (logins, browser sign-in) cannot run from its
shell, and to hand them back explicitly, whole (`env -u VAR …`), as "run in the
home checkout of <repo>: `<cmd>`" or "run in your home folder: `<cmd>`".
Repeat the destination in its own paragraph for each command. Put prerequisites
and ordering separately so each command gets a terminal button. A new conversation's workspace context names
each member's home; `ListWorkspaceRepositories` (so `lazybox workspace repos`,
`lazybox repos`, `lazybox repo home`) returns it to an agent's credential.
`ListRepositoryHomes`, `SetRepositoryHome` and `OpenRepositoryHome` are the
owner's alone.

**Run in … home.** Use **Select commands…** to choose steps, then **Run selected in order**. Each selected command runs in its named location after the previous command exits successfully. Leave alternative commands unchecked. Cancel remaining commands to let the current command finish and skip the rest. After a failure, completed steps are unchecked for retry. Earlier replies keep their buttons when the agent continues.

A reply gets one button per distinct command it hands to the person (`home-run.ts`): the
briefing's phrase is read for certain; otherwise a command the message plainly
hands over ("run … yourself", "you'll need to run", "needs a real terminal",
a login) is offered, never one the agent says it ran or one for
later. The command is kept exactly (`env -u VAR`, every argument; in a fenced
`sh` block too), less a leading `cd` into another absolute folder (a temporary
checkout the home replaces, whose name also picks the repository). A click
runs it in a shell there in the Terminal panel (a real PTY: logins and browser
sign-ins work, which an agent's commands, without a TTY, cannot do): the shell
already open there when it is idle at its prompt, else a new one, once that
shell can read input ([terminals](TERMINALS.md#run-a-command-for-you)). A
script of several lines or a command with a placeholder is typed at the
prompt instead, for the person to review and run with Return. The ⌄ beside it
offers **Paste in terminal (don't run)**, the other places, the person's own
terminal app (`open` at the folder, the command copied) and Copy. A repository without a home gets one first ("Create api home
and run"). A global tool's login (`gcloud`, `aws`, `gh`…) goes to the home
folder. When the shell's command-end mark arrives (`TerminalStatus.command_ends`,
see [command output](COMMAND_OUTPUT.md)), the button says *Finished · exit N*.
A command launched directly by the handoff’s Run button sends its exit status
to the conversation automatically. Pasted commands and unsuccessful reports
offer **Tell the agent it’s done**.

**Update to latest main.** `UpdateRepositoryHome` (owner only; the row, its ⋯
menu, `lazybox repo home <name> --update`) fetches origin with the person's own
credentials (Git never prompts), then fast-forwards the default branch only
when it is checked out, has no tracked changes and no commits of its own;
otherwise it says why and changes nothing. A fast-forward that would
overwrite an untracked file is Git's to refuse. `RepositoryHome.behind` (as
last fetched) shows *N behind origin/main*. Agents are told to use the home,
not a temporary clone, for a repository's own tools at the latest main.

**Storage.** The home path is the repository record's `home` in the library
(`repositories.json`), so it survives restarts. Libraries written before it
have none: nothing is migrated on disk, and a local repository's source folder
serves as its home until the person chooses another. Paths stay on the
Runner's computer; nothing is shared or reachable beyond loopback, and a
paired phone's grants include none of these RPCs.

## Catalog boundaries and configuration

`repositorycatalog.Source` discovers metadata; `repositorycatalog.Store` persists
complete provider snapshots; `Cache` separates refresh synchronization from reads.
The Runner merges this with existing local discovery and repository registration.
`PrepareWorkspace` owns source preparation and collection provisioning. The shared
React catalog reader/picker owns filtering and selection only. Existing low-level
RPCs remain available for compatibility, but the UI no longer chains imports.
Workspace list and session metadata reads use the last published records
while Git is preparing worktrees, so an existing workspace stays browsable.

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

### When a repository's history changes

A GitHub repository is listed by its name (`github:owner/name`) and, once listed
or copied, by GitHub's ID for it. The name can come to mean another history:
the repository is renamed away and a new one (a new ID, a new root commit) is
made under its name, or its history is force-pushed with a new root. Making a
workspace from Lazybox's copy, or from a listing whose GitHub ID GitHub now
gives another name, would silently mix two histories, so New task refuses it
(`RepositoryReplaced`; it used to say only "repository identity changed;
refresh and retry", which refreshing did not fix). The Runner checks a copy's
GitHub ID against the name only when GitHub answers: offline it is not refused.

The error card says plainly what happened ("The repository's history changed
on GitHub since Lazybox copied it", or that the name now belongs to another
repository) and offers **Replace Lazybox's copy**. It asks first and shows what
changes (`RefreshRepositoryIdentity` without `confirm`): the repository GitHub
has now, and the workspaces made from the old copy. Confirming clones a fresh
copy into a directory of its own, registers it in place of the old one, and
retries the start. **Settings → Repositories** offers the same for any GitHub
repository (Replace copy…), for example after a force-push, which Lazybox
does not detect by itself.

Workspaces made from the old copy keep working: their worktrees are registered
in the old copy, which stays on disk, hidden from the library (as Remove does),
never deleted. Their branches and commits are untouched; pushing from them
still goes to `github.com/owner/name`, which now has the new history, so a push
of the old branch there needs care. New workspaces, and repositories added
later, use the fresh copy. Only a copy Lazybox made is replaced; a checkout you
registered yourself is yours to update with Git.

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
recognized GitHub origin use their canonical Git common directory instead. A
repository removed from the library and added again from a different clone gets
an entry of its own: the removed entry stays, hidden, because the workspaces
made from it name its Git directory and need it to be deleted. The library persists across Runner restarts. A registered
repository whose folder is missing at startup (moved, deleted, unmounted) is
marked unavailable instead of stopping the Runner: every workspace still
loads, and creating a workspace from it says why it cannot. Adding the
repository again fixes it: from the same path it is the same entry again; from
a new path (the folder was moved) the working clone gets an entry of its own
and the unavailable one is hidden, kept for the workspaces made from it. A checkout needs an
initial commit; subfolders and `.git` paths are rejected rather than silently
registering a parent repository.

New workspace folders use a unique three-word petname generated once. The display
name can be edited in its sidebar row with **Rename workspace** (Enter to save,
Escape to cancel). In a conversation, `lazybox workspace rename "DISPLAY NAME"` changes the same sidebar label for the current workspace. The editor retains its original workspace identity if selection
changes, without changing its folder, member
paths or branches. Existing workspace folders remain in place. Double-clicking a
repository selects it while retaining other selections; checkboxes support
multi-repository selection. The name field opens focused, its suggestion selected so
typing replaces it. The suggested display name follows selected repository
names (`api`, `api + web`, or `authority +4` for more, never a run-on list) until
the user types a custom name. The branch follows the name (`Login fix` →
`feature/login-fix`) and is shown under it. **Edit branch** overrides it. If
a selected repository already has that branch, the dialog focuses the branch
field so the user can enter another name and retry. Deselecting
and reselecting updates an automatic suggestion. **Settings → Agents & workspaces**
sets whether suggestions use repository names or a memorable generated name.
Renaming later never renames the fixed disk slug or branches. A workspace still
named by a generated petname shows a quiet **Name it** chip on its card: one
click opens the inline rename with a suggestion from its repositories.

Every workspace card has the same four lines, and so the same height, selected
or not, whatever it holds:

1. its name;
2. one repository line (the owner once, the first names and `+N`; the full list
   is the tooltip and the repository dialog), or "No repositories" with **Add…**;
3. its git line: the branch (`+N` when repositories differ), then uncommitted
   files (●), commits not pushed (↑), pushed commits not pulled (↓), commits
   behind the default branch (⇣), and the pull request with a dot for its
   checks. The tooltip spells it out per repository. The Runner's
   `ListWorkspaceGitSummaries` reads local Git only (no fetch) for every
   workspace in one request; pull requests come from what GetPullRequest last
   read, refreshed in the background for pushed branches. Windows poll it every
   20 seconds while visible, on focus, and shortly after a turn or a commit;
4. the conversation it speaks for (one needing you, else one at work, else the
   newest; `+N` counts the others): one mark, its title, then the live status
   and elapsed time. The title gives way first, then the status's words; the
   time is never cut. Provider, model and account are the tooltip.

**The hover card.** Rest the pointer on a workspace card for 400 ms, or press
⌥↓ while it has focus, and a card opens beside it (never over it: right of the
sidebar, left of it when there is no room, below it in a narrow window). It
holds the workspace's name, the issue or task it started from (`#123 Fix
uploads`, linked) and its age (`created 2 days ago · active 5 min ago`); a
highlighted **N waiting for you** when an approval or a question is pending,
which opens the Inbox; each repository as `owner/name` with its branch, `↑3 ↓1`
against the default branch and `+120 −30 · 6 files` (or *clean*); its pull
requests with pills for state, checks and review; its conversations with a live
dot, agent and model, and state (Working, Needs you, Done; four, then
`+N more`, though a fifth shows rather than `+1 more`); what serves (`web :5173`, from running actions and commands that listen
on a port); and the token cost so far. Its footer has Open, New conversation,
Open in Terminal, Reveal in Finder (desktop app) and Copy path. The pointer can
cross into it (it stays 220 ms after leaving the row or the card); Esc closes
it, and Tab goes round its links and buttons. Space on the row still peeks.
While it is the row's, the row's own tooltips stay quiet. What the sidebar
holds shows at once; line counts (`ListWorkspaceGitSummaries` with `changes`:
one `git diff --numstat` against the merge base with the default branch,
uncommitted work included, untracked files counted as files), running actions,
processes and usage are read when the card opens, kept 30 seconds, and shown as
placeholders until they answer. `WorkspaceHover.tsx` renders it;
`workspace-hover-model.ts` is the pure half.

**Your order.** Drag a workspace card up or down to put the list in your
order: a press that moves a few pixels picks the row up (a click stays a
click, ⌥-click still peeks, a right-click still opens its menu), a line shows
where it lands, the list scrolls while the row is held near its edge, and Esc
puts it back. The drag selects no text and opens no hover card, and the click
it ends with selects nothing. `⌥⇧↑` / `⌥⇧↓` on a focused card move it one
place and keep its focus, and a screen reader hears where it went (`⌥↓` alone
is the hover card's; off a Mac, where `⌥⇧↑` / `⌥⇧↓` resize panels, a focused
card keeps them, `ownedByFocus`). The order is the Runner's
(`ArrangeWorkspaces`, kept in `.lazybox-collections/arrangement.json` beside
the records): `ListCollections` answers in it, so it survives a reload and a
restart and every window and a paired phone list alike. A workspace the order
does not name yet (a new one) comes first, newest first; with no order kept
the list is by ID, as before. With several Runners, each section reorders
within its own Runner. The move shows at once and goes back if the Runner
refuses it. `useListReorder.ts` is the gesture, `workspace-order.ts` the pure
half, `collections/arrangement.go` the Runner's.

In an expanded workspace, drag a conversation row or focus it and press
`⌥⇧↑` or `⌥⇧↓` to move it one place. Pinned conversations stay at the top.
The Runner saves the order for that workspace across windows and restarts.

**Peek.** Alt-click a peekable row, press Space on it (like Quick Look), or
choose **Peek** in its context menu to see the item live over the current page
without navigating: a conversation (its latest messages, status, a message box
and Stop turn; a workspace row peeks the conversation it speaks for), a
process (facts, its output, Stop behind a confirmation), a command's output,
an artifact, a mission task, or a file in Files. In the peek ← and → step
through the rows around it, ⌘↵ (Ctrl+Enter) opens it here, Space or Esc
closes it and focus returns to the row last shown. The overlay moves by its
header and resizes from its corner. A sub-agent card peeks in the
conversation's own floating sub-agent panel. Rows opt in with `data-peek`
(`peekProps` in `peek-model.ts`); the overlay is `Peek.tsx`.

Example workspace:

```text
gently-bright-otter/
  api/    # branch: feature/gently-bright-otter
  web/    # branch: feature/gently-bright-otter
```

## Sessions, windows and modules

**New session** exposes account and permission choices directly, with model,
reasoning effort, gateway, and additional context under **Model & context**.
**New task** puts reasoning effort next to the model for Claude and Codex.
**Quick start** uses the last successful provider, account, model, effort, and
gateway selection. Choices are shared by its panels/windows. A refresh
never starts an execution. One-off permissions, context, and rotation pools are not
remembered; Runner defaults apply unless chosen explicitly in New session. Quick start pauses for account selection if its saved profile is unavailable.
With no saved choice, setup suggests a configured managed account, then the first
profile of that provider. In-flight or resumed sessions are not changed.

In a running conversation, pick **Reasoning effort** from the conversation
menu or choose a model and effort from **/model**. The choices come from the
provider's model catalog. Changes take effect from the next turn. The Runner
remembers the choice when the conversation resumes or switches subscriptions.
In a running Claude or Codex chat, `/mcp` lists MCP servers in the conversation.
`/compact` requests manual compaction, and for Claude it can take focus
instructions. Use `/effort` for the current setting or pass an effort level,
`/status` for conversation and context information, `/diff` to open Changes,
and `/copy` to copy an agent reply. In Codex, `/skills`, `/apps`, and `/plugins`
list the enabled provider inventory. Slash input Lazybox does
not handle goes to the agent through the normal message flow.

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

**Run** in the header starts the workspace's default action (a dev server, say)
from its repositories' committed `.lazybox/workspace.yaml`; **Actions** and
**Preview** panels list, control and show them. Each workspace gets its own
block of ports, setup runs after its worktrees are created and an archive
script before it is deleted, all once you approved the scripts. See
[WORKSPACE_RUNTIME.md](WORKSPACE_RUNTIME.md).

**Terminal** in the header opens your login shell in the workspace, on the
Runner, in a Terminal panel docked beside the work; the panel can also open one
in a repository. The shell keeps running when the window closes or the Runner
restarts. See [TERMINALS.md](TERMINALS.md).

### Arrange panels

A page is a dock, like an IDE's. Every panel (the conversation, Workspaces,
Agents, Files, Editor, Changes, Command output, Terminal, Processes, Missions,
Actions, Preview, Artifacts, Usage, Ask Lazybox) has a tab, and the tab is
how you arrange it:

- **Drag a tab onto another panel**: onto one of its edges to split it there
  (left, right, top, bottom), onto its middle to join its tabs, or onto the
  edge of the whole page. A live outline shows where it lands before you let
  go. Drag a group by the empty space beside its tabs.
- **Drag a divider** to resize; a focused divider moves with the arrow keys
  and says its size. The Workspaces sidebar keeps **one width in pixels** for
  every workspace and window size: only dragging its own edge changes it, and
  a panel dropped beside it does not squeeze it. A page whose only docked
  panel is the sidebar (every other one closed, floating or in its own window)
  shows an empty content area beside it with the page's empty state
  (`EMPTY_CONTENT` in `dock-bridge.ts`, never saved), rather than a sidebar
  across the page (`test:layout-stability` switches seven such pages).
- **Double-click a tab** (or ⇧⌘M, or `] ] m`) to maximize its group; again to
  restore. A conversation's preview tab is kept open instead; a middle-click
  closes a tab.
- **Right-click a tab**, or **⋯** beside the tabs, for the group menu: Split
  right / down, Maximize, Minimize (to its tab bar, in a column; click the tab
  or press Enter on it to open it again), Float over the page, Open in new
  window, Move to new page and Keep open / Use as preview tab (chat: keep
  the preview tab, or make a regular tab the preview tab), Dock left / right / top / bottom (the keyboard way to move a
  panel to an edge of the content area), Close other tabs, Close tabs to the
  right, Close.
- **Select all** (Ctrl+A or ⌘A) selects text within the focused panel. Text fields, editors and terminals keep their own keyboard behavior.
- Opening a panel brings its tab forward, including when the window is too narrow
  to show it beside existing panels or another group is maximized.
- **×** on a tab closes the panel. Every panel can be closed; **Panels** in the
  workspace header lists every kind, focuses one that is shown and reopens a
  closed one where its kind was last docked. ⌘\ splits the active panel: a
  tab moves into a group of its own beside its group, or, when it is alone,
  Panels opens to choose what goes beside it. ⌘W closes the active panel, ⌃Tab
  goes through every tab (the active group's, then the next group's), ⇧⌘T
  reopens the last conversation tab closed. These and the rest of the group menu (split down,
  float, open in new window, minimize, show a panel, change or reset the
  layout) are actions of the action registry (`actions.ts`, group *Layout*):
  in the command palette, **Settings → Shortcuts** and the `]]` menu, each
  rebindable. `layoutCommands()` also lists every preset and every panel kind
  as plain functions.
- **Open in new window** puts a group in a window of its own. Its panels stay
  the same panels, run by the main window: an editor keeps its unsaved edits,
  a terminal its shell, Files and the Editor keep sharing one workbench
  (opening a file from Files or a chat goes to the Editor in that window). A chat opens
  instead in its own native window, which survives the main window, as
  before. A Web panel goes too, its page with it: its native view moves into
  the panel's window (`Webview::reparent`, `viewer.rs`), history, scroll and
  forms intact, and is measured there (`native-web-view.ts` places each view
  in the document its panel is in). The window is named to the host by the
  token in its `popout.html?w=` address, and only the window that opened it can
  move views into it. Putting the group back, or closing its window, brings
  the pages home first, hidden, so they show again where the panels land. (Its
  ⋯ **Move to a new window** still opens a Web window of its own, which
  survives the main window.) Popped-out groups come back docked when the page
  loads again. A Web panel's native view steps aside while a tab is dragged
  and under a group floating over it, and follows its panel when the dock
  moves, maximizes or floats it.
- **Putting a window back.** Every panel in a window of its own goes back the
  same ways, to where it was: its group and place among the tabs, else beside
  the groups it sat next to with the same side and share, else beside the
  largest group (`placeBack` in `workspace-model.ts`; a popped-out group
  returns to the group it left, dockview's reference group). In that window:
  the labelled **Put back in main window** button (a chat's window toolbar, a
  popped-out group's tab bar), ⇧⌘D (*Put back in main window*, rebindable, in
  the Window menu of the desktop app), or closing the window — closing never
  loses a panel. In the main window: the quiet **In other windows** strip under
  the header lists each one with **Show** (its window to the front) and **Put
  back**, plus **Put all back** with more than one; the palette offers *Put
  … back in main window* for each and *Put all windows back*. A chat
  goes back to the window it came from (the desktop host tells that window:
  `return_chat`, `chat-return`), which comes forward on it. A saved chat whose
  window is gone (closed while the main window was, or not restored) comes back
  to the page when it loads.
- **Layout** in the header applies a preset, shown as a picture: Chat focus
  (the default), Chat + Changes, Chat + Editor + Terminal, Review (Changes,
  Editor, Chat), Everything. **Save current layout as…** keeps the arrangement
  by panel kind, for every workspace; saved layouts are renamed and deleted
  from their picture; **Reset to default** goes back to Chat focus. A preset
  keeps every panel the page already has of a kind it uses (the same
  conversation, draft and shell) and closes the others.

The Terminal panel docks like any other panel and nothing more: there are no
terminal splits or terminal-specific layout features (Lazybox is not a
terminal emulator; see [TERMINALS.md](TERMINALS.md)).

The edge you last docked each kind of panel at is remembered per person: a
terminal you moved left opens left next time. Moving, tabbing, floating,
popping out or rearranging never reloads a panel: each one renders once into
an element of its own that the dock moves, so drafts, terminal sessions,
editor models and scroll positions stay. Switching workspace never moves a
divider: the other workspace keeps its own panels but takes the current sizes
wherever its arrangement has the same shape. On a phone, panels show one at a
time from the panel switcher, with a reachable ×; the dock is not used there.

The left column is **Workspaces above Agents**, both in view, in every
built-in layout (Agents takes 45 % of the column; both resize, close and move
like any panel). Each workspace lists all its conversations under it, one line
each — status mark, title, age, pinned ones first; provider and model are the
tooltip. A workspace with several folds from the chevron in its icon's place,
and folded, its fourth line speaks for the most urgent one ("+2" for the
rest). Archived conversations leave both lists (History keeps them) unless
one waits for you.

**Agents** lists every conversation in every workspace by urgency (needs you,
failed, ready for review, working, done) or by workspace, with live status,
age and an unread dot; Done starts folded, and every section folds with its
count. A conversation's running sub-agents nest under it, folded (→ shows
them, ← hides them). Type to filter (`/`; Escape clears): the title,
workspace, what it needs, the model, a sub-agent's task. j/k move, Enter
opens, Space peeks, `p` pins, `m` marks read, F2 renames. Pin and mark read
appear on hover or focus in the age's place. Pins are kept by the Runner
(`PinConversation`), so every window and a paired phone show the same ones;
pins kept in a window before that move to the Runner once. **Inbox** (`]] i`)
brings the Agents panel forward with its first row focused; only when the
page has no Agents panel does the Inbox open as a list over the page.

Every conversation row — Workspaces, Agents, the Inbox, History — has one
menu (`ConversationActions.tsx`): Open · Open in new tab · Open beside · Open
below · Open in new window · Move to new page · Peek, then (when it is open in
a tab with others) Close other tabs · Close tabs to the right, then Rename ·
Pin · Mark as read, then Copy ID · Stop, then Archive… · Delete…, each of the
last two behind a confirmation. Archive (`ArchiveConversation`) stops and
deletes nothing; Unarchive brings it back. Open beside (below) splits a tab
for that conversation right of (under) the conversation on screen.

#### Conversation tabs

Every chat panel is a tab of one conversation, as editors are tabs of files
(`workspace-model.ts`, "Conversations"; `useConversationTabs.ts` wires it).
With **Open a workspace's conversations as tabs** on (Settings → Chat, the
default; `workspace.conversation-tabs`), every open makes a kept tab (a single
click opens or focuses one), saved chats load as kept tabs (`settleChats`
without previews: a page from before tabs, or the first tabs build's preview
tab, keeps its conversation), and when a workspace's page loads its open
conversations (`workspaceConversations`: not ended, not archived away, the
latest execution, most recent first) join the chat group as kept tabs, up to
six (`showWorkspaceTabs`); a tab the person closed is remembered per
workspace and not reopened until they open it. With it off, the preview tab
below applies.
A chat group's **preview tab** (`preview` on the panel, its title in italics)
is where a single click on a row shows a conversation; the next single click
replaces it. A double-click, typing in it, ⌘-click (a tab behind the one in
front), a middle-click, Open in new tab and a new conversation make a
**regular tab**, which stays until closed. Every chat keeps its own
conversation (a `pinned` binding); a chat saved following the page (`follow`,
older pages, templates, imports) becomes the preview tab of what it shows
(`settleChats`, on load and on every save). The page's selection is the
conversation of the tab in front: bringing a chat tab forward (a click, ⌃1,
⌃⇥, a drag) selects it (`followFrontTab`), so the sidebar, Command output and
Changes follow. A conversation opened from another (an orchestrator's agent;
a review) opens with `openRelated` (`openRelatedConversation` in
`conversation-requests.ts`): a regular tab right after its parent's, behind it.
Closing a tab (×, a middle-click, ⌘W) never ends its conversation; the
neighbour comes forward and ⇧⌘T (`layout.reopen-tab`) reopens the last tab
closed on the page where it was. A full page (16 panels) opens nothing and
offers to close its oldest preview tab. Tabs are saved with the page.
A tab behind another (or in a minimized group) does not follow its
conversation (`PanelShownContext`; `AgentWindow` and the live bar): its
reader keeps what it showed and catches up from its cursor when the tab comes
forward. A browser allows six connections to the Runner, and a workspace's
tabs each holding a `WatchEvents` stream took all six, so every other read of
the window (its review, its sessions) waited behind them. The tab's status
dot comes from the Runner's session index, not the stream, so it stays live.

#### How it is built, and why dockview

The dock is [dockview](https://dockview.dev) `8.8.3` (MIT; the `dockview`
package, its framework-free core: we render tabs and content ourselves).
Hand-rolling it was the alternative: the previous layout was a binary tree of
single panels with our own split handles and four edge zones, and each of the
asks (drop onto any panel's edge or middle with a preview, tab groups,
floating groups, popped-out windows, maximize, keyboard-accessible dividers)
would have been a new subsystem to write and keep working. dockview has them,
is actively maintained, and had no blocker:

- **Our panels stay ours.** Each panel's content element is its stable host
  (`usePanelHosts`), handed to dockview as is, with the `always` renderer:
  dockview positions it over its group instead of moving it, so a tab switch
  or a rearrangement never reparents or remounts it, and a page rebuild with
  the same panels (`fromJSON` with `reuseExistingPanels`) leaves every one in
  place. `DockLayout.tsx` brings dockview to the saved arrangement in place
  and reports every change made in dockview back.
- **WebKit and Tauri.** Drags use dockview's pointer strategy, not HTML5
  drag and drop, so they behave the same in WKWebView, Chromium and the
  Playwright tests. A popped-out window is `window.open` of this app's own
  blank `popout.html`, drawn by its opener; in the desktop app the host gives
  that request a native window through Tauri's `on_new_window`, keeping the
  opener relationship WebKit needs, and refuses every other URL
  (`is_popout_page` in `host.rs`). Those windows are not saved in the window
  layout and need no IPC.
- **Size.** At most about 400 KB of minified script (94 KB compressed) plus a
  13 KB compressed stylesheet, in the main bundle; Monaco (725 KB compressed)
  and Mermaid's layout engine are much larger.

The saved form is ours, not dockview's (`workspace-model.ts`, version 2): a
tree of splits whose leaves are tab groups, with sizes as shares, plus
floating groups, popped-out groups and chats in native windows. Shares make a
page look the same in any window; the sidebar's pixel width and a minimized
group's tab bar are applied on top (`dock-bridge.ts`). Version 1 layouts (a
binary tree of single panels) migrate when read: every panel becomes a group
of its own in the same place and size, a collapsed panel in a column becomes
a minimized group, a chat in its own window stays there, and the Workspaces
list gains the Agents list under it. A version 2 page (or a saved layout)
that still has Agents as a tab beside Workspaces gets the same column when it
is loaded; nothing else in it moves. The original text is kept beside the migrated one
(the same key with `:v1` appended), once, so a migration never loses a
layout; a layout that cannot be read is left untouched, with a notice and
**Reset retained layout**. That holds whether it is read at startup or when
the workspace is switched to: the workspace opens on the default layout,
locked (the dock draws it and saves no rearrangement) until it is reset or a
layout is imported. The error banner offers **Retry now** only for a dropped
Runner connection; any other error has × to dismiss it. Opening Changes or
Missions on a page of its own when the workspace has eight pages says to
remove one first.

A conversation shows in at most one chat panel per page. Choosing a
conversation brings its tab forward, including one brought back from its own
window (a regular tab); otherwise the preview tab shows it. Putting a chat back
removes the returning panel when the page already shows that conversation, and
saved pages with duplicate chat panels are repaired when they load (a regular
tab wins over the preview tab).

### Agent telemetry and workspace accounting

The Runner now owns a streaming metering gateway, a SQLite usage ledger and
provider-neutral context snapshots. The **Usage & cost** module can be added to
any workspace page; chat has a compact context/usage disclosure. Select
**Metered · track workspace cost** in a new session's gateway options. Direct
sessions retain provider-reported observations where supported.

The **Processes** panel (also from **Panels** in the workspace header) lists
every agent, managed command and terminal the Runner hosts, across workspaces,
with PID, uptime, CPU and memory: open, attach to (tmux) or stop any of them, and
clean up processes no Runner supervises. See
[the Processes panel](SESSION_HOSTS.md#see-every-hosted-process-the-processes-panel).

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

### The Web panel

A **Web panel** is a small browser beside the work: an address bar that takes
an address (`github.com/org/repo`, `localhost:3000`) or words to search for,
back and forward, reload (stop while loading), a progress bar, zoom, **copy
address** and **open in your browser** (↗). The page's origin is always on
screen, in bold before the rest of the address, with a padlock for https, an
open padlock for plain http on the web, and a terminal mark for this machine.
The panel's header shows the page's title and icon (in the browser build, its
host: another origin's title is private to it). A page may hold several Web
panels; each keeps its own history (50 pages) and zoom with the panel, across
restarts.

- **Open one**: ⌥-click (Alt-click) any web link in Lazybox: a chat message,
  an artifact, a pull request or check. It opens in the page's Web panel used
  last, or in a new one docked beside the work. A right-click on a link offers
  **Open in Lazybox**, **Open in a new Lazybox panel**, **Open in your
  browser** and **Copy link** (the `link` context-menu scope, for every web link
  in the window); hovering a link says what ⌥-click does. **Page
  actions (⋯) → New web panel** and `] ] b` open an empty one, which lists the
  workspace's running app previews to open with a click.
- **A plain click** still opens your browser, as before. **Settings → Web &
  links** swaps the two: links open in a Web panel and ⌥-click opens the
  browser. ⌘-click (Ctrl-click off a Mac) and Shift-click always open the
  browser; on a Mac, ⌃-click is a right-click and shows the menu. The same page sets
  the search engine (Google, DuckDuckGo, Bing, Kagi, Startpage). Windows
  without a workspace page (a chat or artifact window) open links in the
  browser.
- **⋯ → Move to a new window** (also on a right-click on the panel's header)
  moves the page into a window of its own with the same toolbar; **Move back to the workspace** there opens it in a new Web
  panel of the main window and closes the window.
- The workspace's **Preview** panel ([WORKSPACE_RUNTIME.md](WORKSPACE_RUNTIME.md))
  is the same viewer, on the running action's address.
- The address bar refuses anything but a web page: `javascript:`, `file:`,
  `data:`, an app's own scheme, an address with a user name or password
  (`https://bank.example@evil.test`), and Lazybox's own address.

**In the desktop app the page is a native web view**, a Tauri child webview
(`Window::add_child`, Tauri's `unstable` multiwebview feature) of the window the
panel is docked in, placed over the panel's box. Sites that refuse to be framed
(`X-Frame-Options`, `frame-ancestors`: GitHub, Google and most sign-in pages)
show, which an iframe cannot do. The alternative, a separate native viewer
window with its own toolbar, would not dock, resize or move with the work, and
would put every page in a window to manage: the panel is what was asked for.
The costs of the child webview, and what pays for them:

- It is drawn above the whole page, so it follows the panel's box on every
  layout change (resize, docking, a divider, scrolling, the page's own ⌘+ zoom)
  and steps aside — hidden — while anything of the app's own would be under
  it: a menu or popover over it, any modal dialog, the dock preview while a
  panel is dragged, a collapsed panel, another page or Settings. A view whose
  panel goes away is hidden at once and closed a minute later, so a visit to
  Settings and back keeps the page as it was (`native-web-view.ts`).
- Its frame is the page area under the panel's address bar, clipped to every
  ancestor that clips the panel, in the app page's CSS pixels with the viewport
  it was measured in. The host converts it to points against the app webview's
  own size (so the app page's ⌘+ zoom and the display's scale cancel out) and
  snaps the edges inward to whole points, so a fractional edge never overlaps
  the sidebar or the address bar (`viewer.rs`, `logical`). Never against the
  window's `inner_size()`: on macOS, once a window has a child webview, Tauri
  answers that from tao's original content view, which wry replaced and which
  stops following the window, so a window resized or maximized after its first
  viewer kept its creation size and every page was drawn too high and too far
  left, over the address bar. The panel and all its ancestors are watched for
  size changes (a sidebar, a split, the Agents panel closing move it), as are
  the window, the root's appearance attributes and the display's pixel ratio.
- A window with a child webview is no longer a single-webview "WebviewWindow"
  to Tauri, so the host's commands take the calling webview and window instead
  (`host.rs`). `unstable` is pinned with Tauri (`=2.12.5`); an upgrade re-checks it.
- Keyboard focus is in the page while you use it, as in a browser; app
  shortcuts work again once you click outside it. Its own ⌘+/⌘- do not zoom it:
  use the panel's zoom.

**A viewer is not part of the app.** Tauri gives a webview the capabilities
that name its own label or its window's label, so the app's capabilities name
the app's webviews (`main`, `workspace-*`, `chat-*`, `console-*`,
`artifact-*`, `web-*`) and never a window; a viewer is labelled `viewer-…`,
which nothing matches (`no_capability_reaches_a_web_viewer`). Tauri also refuses
any command from a remote origin without a remote capability, and there is
none. A viewer only loads http and https pages (and the `about:`, `blob:` and
`data:` frames pages are made of); it never navigates to the app's own origins
(`tauri.localhost`, `ipc.localhost`, `asset.localhost`) or to `file:` or another
app's scheme — such a link shows **Open it** in the panel instead, which hands it
to the system. Its website data (cookies, storage, cache) is its own store
(macOS 14+; on older macOS WebKit's single per-app store, which the app's
`tauri://` origin shares with no web origin), so sign-ins made in a viewer stay
there. It never downloads: a download shows **Open in browser**. Camera,
microphone and other device permissions are denied. Nothing fills in
credentials: no script of Lazybox's runs in the page except one that reads its
title and icon link after it loads; the icon is fetched by the host without
cookies, and a page on the web never makes it fetch from this machine or the
local network (`viewer.rs`).

**In a browser** (the web client) the page is an iframe, sandboxed without top
navigation and sent no referrer. A browser does not tell a page that a frame was
refused (Chromium and WebKit both report a refused frame exactly like a loaded
cross-origin one, checked with Playwright), so: a page that has not loaded after
12 seconds shows **This page isn't loading here** with **Open in browser** and
**Try again**; a refused frame a browser leaves blank (`about:blank`) shows
**This site can't be shown here**; and a page from another site carries a
dismissible line, *Blank page? Some sites refuse to be shown inside other
apps*, with **Open in browser**. Links followed inside a page from another
origin stay that page's own history (in the desktop app every load is history).

Not verified: the desktop app with the native viewer was compiled, unit-tested
and linted, not run in a built app: the separate data store and the icon fetch
are unexercised in WKWebView. Placement and hiding are tested up to the host
call (a recording host in `scripts/fixtures/web-view-host`, and `logical` in
`viewer.rs`); that WKWebView then draws at that frame is checked by eye.
Windows and Linux (the `data_directory` store) were not built.

**Tests**: `npm run test:web-viewer` (the panel in the browser build: link
routing by click, ⌥-click and menu, the address bar, search, history, zoom,
copy, the stalled-page fallback, Settings, a new panel, moving to a window and
back, persistence, phone width; and with a recording native host on a Retina
display, the frame over the page area through sidebar, window, notice, text
size, split and maximize changes, hidden under menus, context menus and other
pages); `node --import tsx --test
scripts/web-viewer.test.ts` (addresses, search, history, zoom, link hints,
placement and clipping); `make check-desktop` (URL and navigation policy, frame
scaling against the app webview, zoom, Retina and inward snapping,
icon rules, the web window surface, capability isolation).

## Review and conversation continuity

**Review changes** opens a workspace page using the same module registry as chat
and output. The Changes module resolves members on the Runner, verifies their
Git registration, and shows local index/worktree status and bounded diffs. It is
also available from **Panels**. Different repository failures stay
independent. GitHub API access is not required for this local inspection.
Each repository also carries its PR loop: commit, push, open a pull request,
follow its checks, send failing checks to the conversation, and merge. See
[the PR loop](GITHUB.md#the-pr-loop). Every diff takes inline review comments,
kept per workspace on the Runner and sent to a conversation as one message;
see [review comments](GITHUB.md#review-comments).

A diff reads **unified or split** (side by side; the choice is remembered for
every window of the Runner, and a pane narrower than 700px shows unified until
it is wider). Both mark the words that changed between a removed line and its
replacement, fold long unchanged runs, and count the unchanged lines between
hunks; where the diff's new side is the working file (uncommitted changes, or a
branch file with no uncommitted edits) a click shows them, read from that file
and refused if it no longer matches the diff. Comments work the same in both:
removed lines from the left, everything else from the right. Each file has a
**Viewed** box, kept per workspace in the client's preferences for this Runner
(not on the Runner) with the diff it was ticked on: a file whose diff has changed is unticked when it is next shown.
Keys in the Changes page: `n`/`p` next and previous hunk, `Shift+N`/`Shift+P`
next and previous file, `v` toggles Viewed (and moves on to the next file still
to review), `s` switches unified and split.

### Queued messages

Use **Delete** on a queued message to remove it before the agent receives it.
The running turn and the order of the other queued messages are preserved.

### Per-turn checkpoints and revert

Before each agent turn starts, and again when it ends, the Runner saves every
workspace repository's files: committed, uncommitted and untracked, but not
ignored. Each repository gets a commit under a private ref,
`refs/lazybox/checkpoints/<conversation>/turn-<n>-before` (and `-after`), whose
parent is the HEAD it was taken on; across a multi-repository workspace a
checkpoint is that set of refs. It is written through a private copy of the
index, so the repository's index, HEAD, branch and stash are never touched, and
when nothing changed the previous commit is reused (no new objects). Untracked
files over 32 MiB, past 20,000 per checkpoint, or in nested repositories are left
out and named. A turn waits for its checkpoint (bounded to two minutes); a
repository that cannot be read is reported on the turn and the turn still runs.
Assistant conversations are not checkpointed. Implementation:
`internal/runner/codecheckpoints` (unrelated to `checkpoints`, the execution
handoff).

In the conversation, each finished turn ends with a quiet marker: what it
changed (files, lines), **Show changes in this turn** and **Revert to before this
turn…**. Show changes opens the workspace's Changes page compared with that turn
(before → after; turns are also in its **Compare with** list); a chat in its own
window shows them in a dialog. Revert first previews exactly which files will be
restored, removed or put back, and says what stays: commits made since remain on
the branch (the revert shows as uncommitted changes against them), ignored
files, files too large to checkpoint, repositories added since. It is refused
while any agent in the workspace is in a turn or has a message queued. Before
writing anything the Runner saves the current files as a safety checkpoint, then
rewrites only the paths that differ and checks the result, naming any file it
could not restore. A path where something the safety checkpoint left out sits
now (an ignored file, one too large or past the untracked limit, a nested
repository, or a folder still holding ignored files that the revert would
replace with a file) is left as it is and named: undo could not bring it back. **Undo revert** (in the toast, and on the reverted turn's
marker) restores the safety checkpoint; an undo is itself undoable. The
conversation is kept, not truncated: a Runner notice records the revert, and the
agent is told with the next message (through the same path as instruction
updates) that files were reverted and must be re-read. In a Codex coding conversation, **Fork after this turn…** opens a separate
conversation through a completed turn. The original can continue working while you open the fork.
**Edit and restart here…** on a supported user prompt sends a revised prompt
using the history before it. Click **Restart from here…** on a prompt, or press **Esc** twice in an empty composer and choose a prompt with the arrow keys and Enter. Codex and Claude support prompt restart. Choosing current files stops the previous continuation,
which stays available in History. The picker searches older prompts as history loads. Before confirming, the dialog checks the native boundary and shows the repositories in the retained checkpoint. A skipped file disables isolated restart. Reopening the same prompt after a lost confirmation in this tab recovers the revised text and workspace choice. The source link opens the conversation at
the original reply or prompt.

See the [prompt restart implementation and roadmap](PROMPT_RESTART.md) for provider boundaries, file coverage and release verification.

In a managed workspace, the dialog can create separate worktrees from the
turn's saved files (after a fork, before a retry). Restart defaults to a separate workspace when available and lets the original conversation continue. The checkpoint is the base
commit of each new branch, including any uncommitted files it captured.
Ignored files are outside that snapshot. The Runner requires each recorded
repository snapshot to be available and complete. **Use current workspace
files** shares the live working directory with the source conversation.
Codex requires bounded thread forks. Claude uses the saved native assistant message at the end of a completed turn. Older Claude turns without this identifier cannot supply that boundary. Turn
boundaries from a different native thread and retry prompts with attachments
are currently unavailable.

Each conversation keeps its latest 100 turns and 20 reverts; older ones are
pruned with their refs (`codeCheckpoints` in the Runner configuration changes
the bounds). Deleting the conversation deletes its record and every ref. The
RPCs (`ListTurnCheckpoints`, `ListTurnChanges`, `ReadTurnDiff`,
`PreviewRevertCode`, `RevertCode`, `UndoRevertCode`) are the owner's alone: an
agent's or terminal's credential cannot call them.

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
event buffer. It renders the latest 40 messages initially; **Show earlier messages**
reveals retained messages while preserving the reading position. Scrolling through
history moves a bounded window of rows in either direction. A jump to an old
prompt mounts the rows around that prompt. Incoming output
only scrolls when already following the bottom. **Jump to latest** floats over the
transcript. Sending a message or choosing **Jump to latest** keeps the newest
turn visible as the reply arrives.
Replay gaps and terminal stream errors appear in the chat with **Retry conversation**.
They never masquerade as a new, empty conversation.

Chat cost totals include all execution attempts in the conversation. Context uses
the latest reported snapshot per agent; workspace cost aggregation remains separate.

Local Markdown links and inline `path:line` references open in the **Editor**
panel (a folder opens in **Files**); Alt/Option-click opens the quick-look window
instead and Cmd/Ctrl-click opens beside the current file. Before opening, the
Runner resolves what the agent wrote (`ResolveWorkspacePath`): the path as given,
then each shorter tail of it at the workspace root and under every repository
(so `scripts/x.json`, `./scripts/x.json` and an absolute path from another
checkout all find `api/scripts/x.json`), then a path-suffix match, then the file
name. Several matches open a chooser, nearest first; only a reference that names
nothing in the workspace is reported. In the desktop app, an unmatched absolute
or `~/` image, audio, or video path on the local computer opens in its default app.
Runner resolution stays within the workspace: traversal is reduced to a lookup by tail and every candidate is opened
through `os.Root`, so a symlink out of the workspace never matches. Previews are
bounded to 1 MiB and report binary files explicitly. **Open file** in Changes
opens the changed path as is, without resolution.

**Files** is a tree of the workspace with its repositories at the top, folders
loaded as they are opened, file-type icons, and unsaved files marked. ↑/↓ (j/k)
move, →/← (l/h) open and close folders or step in and out, Enter opens a file
and focuses it, Space opens a preview, `*` opens sibling folders, `/` starts
**Go to file**. The active file is revealed as tabs change. Opened folders are
remembered per workspace; shown folders refresh on focus and every 12 seconds.

**Go to file** (Cmd/Ctrl+P, `]] f`, or the Files search box) is a fuzzy finder over
every file in the workspace's repositories. `SearchWorkspaceFiles` ranks on the
Runner over an index of `git ls-files --cached --others --exclude-standard` per
repository, so ignored files are left out; the index is bounded (200,000 paths,
10 s) and cached for 10 s, and results that are not regular files inside the
workspace are dropped. An empty query lists recently opened files; `name:42`
opens at line 42; Cmd/Ctrl+Enter opens beside, Alt+Enter quick-looks.

The **Editor** panel holds files in tabs, in up to four groups split right or
down (buttons on each group, Cmd/Ctrl+\ and Cmd/Ctrl+Alt+\). The editor's
keys are its own only while the focus is in it; elsewhere Cmd/Ctrl+\ splits the
panel in front. A single click
opens a preview tab (italic) that the next one replaces; a double click, an edit
or Enter keeps it. Tabs show an unsaved dot, reorder by drag or
Cmd/Ctrl+Alt+Shift+←/→, move between groups by drag, cycle with Cmd/Ctrl+Alt+←/→,
and close with ×, middle click or Alt+W; closing an unsaved file asks to save,
discard or cancel. Cmd/Ctrl+Alt+1…4 focuses a group. Two groups on one file share
its text with separate cursors; each remembers its place in every file. Groups,
tabs, order and recent files persist per workspace; drafts live for the window
and a page close with unsaved edits asks first. Visible files poll the Runner
every 4 seconds: a clean file takes the new disk version, an edited one shows
**Use disk version** / **Keep mine** rather than losing either. Files and the
editor dock together to the right of the conversation, the pair taking most of
the width; opening a file goes to the Editor wherever it is, even in its own window.

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
diagram is present, renders in strict mode into an SVG image (a `data:` URL, which
the desktop CSP's `img-src 'self' data:` allows), and falls back to copyable source
on invalid/incomplete input with the reason in words (`diagram-errors.ts`): the line
Mermaid could not read, an unknown type, a limit, a theme colour, a renderer that
did not load. Renders are serialized through one queue (Mermaid's configuration is
window-wide) into an off-screen, laid-out box, so a diagram in a folded or hidden
panel still measures its text; a failure that is not the text's own is retried when
the diagram is shown, a few times at most, and the full error goes to the console
once. Console logs retain exact raw text.

The chat header is one row: the conversation title (click to rename in place;
while it has none, the workspace branch leads, never the folder's generated
name, with a quiet **Name it** that suggests a title from the first message),
then agent, model, account and the live status, and on the right the context
meter, cost, **Activity** (quiet at 0), pop-out and ⋯. A session that opened
and never ran a turn reads **Ready** and takes the keyboard, not "Starting":
the Claude SDK reports `session_ready` only after the first prompt. Runner
restart dividers are left out of a conversation that is otherwise empty.

The live status moves while the agent works, in the header, the line above the
composer, sidebar rows and inbox rows alike: a turning ring and the step with
its elapsed time (`Running · Bash · 12s`, `Thinking · 4s`); waiting for you
breathes; nothing moves under reduced motion. Rows use the Runner's
`AgentActivity.phase`, `tool` and `phase_since_unix_millis`; the open chat
refines them from its journal.

Activity appears beside the composer while a turn runs: Thinking, Running a tool,
Writing, Waiting for the model, or Compacting context when the provider reports
that phase. Unknown
activity stays generic Working; no hidden reasoning is displayed. Root completion
and background-agent activity remain distinct. Connection recovery has its own
label. This transport is Connect streaming RPC, not SSE; text deltas are rendered
before stream completion.

## Delete and resource cleanup

Git checks and worktree removal for a deletion run outside the shared
workspace registry lock. Other workspaces remain readable during cleanup.

Deleting a workspace or conversation moves it out of the normal lists for
five minutes. The **Undo** notification is visible for five seconds. Open
**Recently deleted** at the bottom of the Workspaces sidebar to undo during the rest of the
five-minute window. The Runner persists the deadline, and
existing files, messages and running processes stay in place during that
window. At expiry the Runner checks the existing cleanup safeguards and
permanently deletes the item. For a workspace, this removes the entire folder,
including all worktrees and any approved ignored or outside-repository files.
If an expected worktree is still registered at its verified path and its
`.git` link is missing, deletion requires discard consent. The Runner asks Git
to restore the link before validating and removing the worktree. If cleanup
fails, Recently deleted shows the failure until you dismiss it.

**Workspace actions → Delete workspace** uses a confirmation button without name
typing. It previews active executions and local
files, states the detected risks, and asks explicitly before stopping sessions
or discarding changed/untracked/ignored files. Git-ignored directories named `node_modules/`, `build/`, `dist/`, `target/`, `coverage/`, `.next/`, or `.cache/` can be removed without discard consent at any depth. Tracked edits inside them still require consent. The preview identifies repositories with Git-ignored files separately from repositories with uncommitted or untracked files. A repository whose HEAD is
detached (its commits may be on no branch) or with a rebase, merge,
cherry-pick, revert or bisect stopped part-way counts as changed too, even
when `git status` is clean: removing the worktree deletes its HEAD history and
the operation's state, so deleting asks you to discard first. When the dialog offers multiple
options, **Check all** selects them together, including artifact deletion.
You can untick any option before choosing **Delete workspace**. Selecting
the options moves focus to Delete workspace, or Cancel if deletion is blocked.
Cleanup first fences new workspace
attachments, closes owned provider processes, cancels and waits for owned commands,
then removes each worktree through Git and removes the workspace folder. Source
repositories, branches (including committed work), accounts and conversation
history remain. The destructive operation is not performed when opening the dialog.

A durable deleting state supports retry after partial removal; deletion never
silently becomes success when a process, source identity, root path, or Git
registration cannot be verified. Unknown ownership after a Runner crash ("Owner
uncertain") must be settled first, and the dialog says which agents and commands
and offers Stop them and continue, or They're already stopped, continue
([unconfirmed ownership](RESOURCE_OWNERSHIP.md#unconfirmed-ownership)); the
Runner clears by itself what it proves stopped. External pipe producers are not Runner-owned processes and cannot
be declared stopped by cancelling a log reader. Tombstones prevent retried create
requests from resurrecting deleted workspaces. Deleted workspaces are hidden from
the workspace list, and their conversation history cannot resume into missing files.

**End session** in chat uses `StopSessionResources` to stop that provider and its
owned commands. Closing a window still only disconnects its readers. Closed agent
sessions release the concurrent execution cap; retained history does not consume
an active-session slot. The existing low-level `CloseSession` RPC still ends only
the provider, for callers deliberately keeping managed commands alive.

Each agent runs in its own [session host](SESSION_HOSTS.md) (optionally in a tmux
window you can attach to; see [hosting backends](SESSION_HOSTS.md#hosting-backends-session-hosts-default-or-tmux)). Window
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

Quick look (Alt/Option-click a file reference, or from an editor group's menu)
opens a tall, draggable, resizable surface with Markdown preview and the same
locally bundled Monaco editor; **Open in editor** moves it to a tab. Detached
chat windows, which have no editor panel, open every reference here. Monaco
starts loading when the Files or Editor panel mounts or a file link is pointed
at; a slow load shows a quiet outline after 300 ms, never a loading label.
Editing in quick look is explicit. Save compares the loaded
revision with the file on the Runner and rejects a changed file while retaining
the draft. Lazybox edits serialize per workspace manager and use a same-directory
atomic replacement with existing mode bits; external writers do not participate
in that lock. Binary, truncated, symlink and read-only files cannot be edited.
Unsaved drafts require an explicit discard before closing or refreshing.

**Delete conversation** appears in conversation row actions and the chat session
menu. One click moves it to **Recently deleted**. Use **Undo** in the notification or
**Recently deleted** within five minutes to restore it. After five minutes, the Runner
stops and waits for owned execution, releases
workspace ownership, removes Lazybox attempts/transcripts/uploads and retains
identity-only deletion tombstones so legacy import cannot resurrect them. Uncertain
process ownership blocks deletion. Workspace files, subscriptions, accounting,
command audit logs and the provider's own history remain. **End session** retains
conversation history; **Delete workspace** has its own file/resource check.


The chat follows its rendered content height, including delayed diagrams and
viewport changes, while the reader is at the bottom. Scrolling up suspends follow;
returning to the conversation restores that reading position. **Jump to latest**
and sending a new message return to the latest rows, release earlier rows from the view,
and resume following incoming output. Earlier history stays available by scrolling up. The header shows the provider-reported
main-agent model, captured from startup/context/usage events as well as the saved
session metadata. Internal account IDs such as `claude-default` are presented as
“Local CLI sign-in,” separately from the model.

Sidebar navigation shows the workspace name with its repository identities underneath. Conversations use compact title/model rows; the account is shown only when a named account is known. A closed execution is labelled Saved, not an ended conversation. Action menus remain inside their rows. Workspace deletion initially focuses Cancel; draggable headers retain keyboard movement without a hover instruction popup.

Resuming preserves the mounted transcript and its projected messages. The successor reader inherits the predecessor’s contiguous timeline cursor and view state, then reads only the new events. An unrelated execution never inherits another conversation’s transcript.

## Compact navigation and keyboard controls

A workspace with exactly one known conversation is one sidebar entry. Clicking it
opens that saved conversation; it never starts an execution. Multiple
conversations appear as children only in the selected workspace. Repository names
stay on the workspace row. The plus icon beside its name opens a new conversation;
the row menu distinguishes deleting the conversation from deleting the workspace.
The sidebar loads one paginated history index independently of selection, merging
live execution updates and retaining its last complete history on refresh failure.

Settings → Shortcuts offers three key presets (Vim-style `]]` and `h j k l`, a
`Ctrl+b` prefix with arrow keys, and arrow keys) and editable bindings.
Space separates a sequence; `Ctrl+b` denotes a chord. The default `] ]` leaves the
composer for navigation, `] ] s` opens snippets, `] ] c` opens a new conversation,
and `] ] w` creates a workspace. In navigation, `j/k` move, `h` focuses the parent
workspace, and `l` opens the entry and focuses its composer. The `Ctrl+b` preset
uses `Ctrl+b` followed by arrows or `s/c/w`. After the prefix, a menu card lists
every next key. Settings → Shortcuts also adds your own entries to that menu: one
key sends a snippet, or inserts it when a draft is already there.
A single `]` remains draft text. IME composition and ordinary form fields are not
navigation targets. Bindings are client preferences persisted through the host's
view-state store; they do not change the Runner's permissions or agent settings.

### Vim editing

Settings → Shortcuts has two opt-in switches, applied immediately and persisted
with the other keyboard preferences: **Vim editing in the message box** and
**Vim editing in the file editor**.

The message box gets a small modal layer (`composer-vim.ts`, unit-tested in
`scripts/composer-vim.test.ts`). An empty box starts in insert mode, which is the
plain textarea: typing, IME composition, paste, attachments, snippets and the saved
draft behave exactly as without Vim, and Enter keeps the configured send key.
Esc switches to normal mode, shown by a `NORMAL` chip and a block cursor. Normal
mode supports `i a A I o O`, `h j k l w b e 0 ^ $ gg G`, `x X dd dw D cw cc C yy p P`,
operators with any of those motions (`d c y`), counts (`3w`, `2dd`, `d2w`), `u` and
Ctrl-r (one insert is one undo step), visual mode `v` with `y d c`, and Enter sends.
Cmd/Alt shortcuts stay with the browser; a paste in normal mode lands at the cursor.
Vim edits go through the same input path as typing, so the draft persists. The
leader's first key (`]` by default) is never a Vim command: `]]` opens the menu from
normal and visual mode without inserting a bracket, and from insert mode the first
`]` is text until the second removes it, as before. When a send empties the box the
next message starts in insert mode.

The Monaco file editor uses `monaco-vim` 0.4.4 (CodeMirror's Vim keymap) with a
status line for the mode, pending keys and the `:` prompt. `:w` saves through the
normal save path, `:q` closes through the unsaved-changes guard, `:q!` discards,
and `:wq`/`:x` save then close (staying open if the save fails). Esc inside the
editor always stays there: it leaves insert or visual mode and never closes the
window; Esc elsewhere in the window still closes it. While Vim is on, Monaco uses
its textarea input rather than Chromium's EditContext, which delivered keys typed
just before Esc after Vim had left insert mode (dropped or reordered text).
monaco-vim imports `monaco-editor/esm/vs/...` paths that monaco-editor 0.55+ no
longer exports; `scripts/monaco-vim-compat.mjs` is a labelled Vite stopgap mapping
them to the exported paths, wired into both app configs and the editor UI tests.
`npm run test:vim-editing` covers both surfaces in WebKit and Chromium.

## Snippets

The composer snippet icon and the configurable snippet shortcut open the same
searchable picker. Preview the exact delivered body, then **Insert** (Shift+Enter)
or **Send snippet** (Enter). Existing draft text disables immediate snippet sending;
insertion preserves it. Every confirmed send or insert counts as a use: the
last-used snippet comes first, then favorites, and **Frequent** sorts by use
count (shown as `3×`). While searching, exact and prefix matches on the
snippet name come first, followed by words in the name, description, and
category. Use `body:` followed by a phrase to search inside snippet text.

In the message box, ↑ in an empty box (or with the caret at its very start)
recalls sent messages like a shell: this conversation's first, then every
other's, newest first; ↓ walks back and returns the draft it replaced. In Vim
normal mode the arrows keep their Vim meaning.
The existing message/receipt and resume paths own delivery and retry identity.

The Runner's `snippets.Catalog` seam has a command adapter. The shipped
`lazybox-snippets` helper (`tools/lazybox-snippets`) ships its built-in catalog
from `builtin.yaml`, applies global and launch-directory overrides in order,
and resolves skill-backed bodies. It is read-only. `snippetCommand` and
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

The chat keeps one live phase line. **Activity** in the chat header opens tool
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

Favorites are saved in native view state
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

The Workspaces sidebar refreshes saved conversation history in pages and
retries a temporary Runner failure before showing a warning. If the refresh
still fails, existing entries remain visible and **Retry history refresh**
appears below **History**. History polling pauses while the Runner is slow
or disconnected and resumes automatically when it responds again.

Deletion treats a member folder with a missing Git link as potentially dirty, even after Git prunes its worktree registration. Explicitly discarding it removes the remaining workspace files and preserves the source repository and branch.

### Arranging detached windows on desktop

Open a second workspace or detach a conversation, then place both windows on the same display. Under **Surfaces → Arrange Windows**, choose **Equal Columns**, **Focused Window + Stack**, or **Grid**. The focused window goes first. Focus + Stack gives it two thirds of the display and stacks the other windows beside it. Other displays keep their current arrangement. Full-screen and minimized windows are excluded from tiling.

Use **Move Focused Window to Next Display** to send a detached conversation or workspace to another monitor. Use **Save Current Window Positions** before rearranging a laptop and external displays. **Restore Saved Window Positions** reapplies that snapshot to matching open windows for this Runner. Windows whose display is missing fit onto the primary display. Restoring positions keeps each workspace, conversation and draft open. Normal app restart continues to restore the last live arrangement automatically.

The Back button in the workspace header returns to the previous workspace, conversation, page, or Settings tab in this window. Press Alt+Left or Command+[. In Documentation the same shortcuts return through the pages you read, then close the viewer.
