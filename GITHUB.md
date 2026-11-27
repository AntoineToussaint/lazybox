# Optional GitHub connection

The Runner owns a private GitHub connection, repository source copies,
read-only issue/PR access and the [PR loop](#the-pr-loop) (push, open, read and
merge a workspace branch's pull request). The desktop submits a credential only
during an explicit connection; responses never return access or refresh tokens.

## Connect without installing an App

1. Open **Settings → Repositories → Connections & folders → Connect with a personal token**.
2. Follow the steps in the form. **Create a token on GitHub** opens a classic
   token already set to the `repo` scope. For SSO organizations, click
   **Configure SSO** on that token and authorize each organization. Paste the
   token; the output of `gh auth token` also works. The Runner validates your
   identity before saving it. The form clears the token after submission.
3. Open **New workspace**, select repositories directly, and create it. Browsing
   discovers metadata; the Runner prepares sources only when creating or extending
   a workspace. There is no separate import or library-selection step.

No GitHub App configuration, installation or GitHub CLI is required for this
path. Discovery uses the authenticated user's repository API, including owner,
collaborator and organization membership, filtered by the token's permissions.
The API adapter uses pinned `google/go-github/v93` behind the provider interface;
Git supplies clone and worktree operations.

GitHub supports [classic and fine-grained personal tokens](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens).
Classic tokens with `repo` scope support private repositories across
organizations and outside-collaborator access, and the writes the PR loop makes.
Fine-grained tokens can grant Contents, Issues and Pull requests access to
selected repositories (read for discovery; **write** on Contents and Pull
requests, read on Checks and Commit statuses, for the PR loop), but currently
have outside-collaborator and multi-organization limitations. Neither kind
bypasses organization token policies, approval requirements or SSO.

One GitHub identity is supported per Runner. To change from an existing App
connection, explicitly **Disconnect** it in Lazybox first, then connect the
personal token. This preserves imported repositories and sessions; it does not
revoke the App on GitHub. A different account cannot silently replace the saved
identity. Replacing an invalid token for the same account validates and saves
it before replacing the old credential. Personal tokens are not OAuth-refreshed:
revocation or expiry is reported when GitHub rejects a request; supply a new token.
An expiry date is not inferred when GitHub has not supplied one.

The existing GitHub App device sign-in remains available when configured. Its
repository discovery uses the App's accessible installations. **Connect with
GitHub App** completes browser authorization without entering a personal token.

GitHub is optional for coding. [Add an existing repository](WORKSPACES.md) on
the Runner to create workspaces without any GitHub API authorization.

## Optional GitHub App developer setup

Register a GitHub App owned by the Lazybox organization or developer account.
Use separate Dev and Working app registrations when testing different grants.
Use the official [GitHub App registration settings](https://github.com/settings/apps/new):

- Give the App a unique Lazybox name and its actual product homepage.
- Enable **Device Flow** and leave user access token expiration enabled.
- Set repository **Issues** to read-only, **Checks** and **Commit statuses** to
  read-only, and **Contents** and **Pull requests** to read and write (the PR
  loop pushes branches and opens and merges pull requests; with read-only
  grants those actions are refused with an explanation and everything else
  works). Metadata read access is implicit. No organization permissions,
  webhook events, callback server, client secret or private key are needed.
- Disable webhooks for this read-on-request implementation. Choose who can
  install the App according to the intended audience; organization installation
  may require an organization owner.
- Record the public **Client ID** (not the numeric App ID) and App URL slug.
  Install the App on explicitly selected repositories.

Supply those public values in the selected Runner's configuration:

```json
{
  "githubApp": {
    "clientId": "<registered-public-client-id>",
    "slug": "<registered-app-url-slug>"
  }
}
```

Merge this field into the existing config; retain profile, permission and
gateway settings. The [channel launcher](ENVIRONMENTS.md) creates a config
per environment. Restart the selected Runner when it is idle. Configuration
is persisted on that Runner; changing the App while an identity is connected
is rejected, rather than replacing that identity. No real App ID is shipped
in this prototype. An unconfigured Runner still offers personal-token connection.
Users do not see an App-registration form or need to create their own App.

GitHub's [device authorization flow](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-user-access-token-for-a-github-app)
issues an expiring GitHub App user token. The Runner requests `offline_access`
and [refreshes it](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/refreshing-user-access-tokens)
without an App secret for this device-flow grant. A refresh rotates both
tokens, so it runs on the connection's own context (30 s at most), one at a
time — every request that needs it waits for the same one — and is saved
whether or not the request that started it is still waiting; the connection's
lock is never held across a GitHub call. Access is the intersection
of the user's rights, App permissions and selected installation repositories.
One GitHub identity is supported per Runner in this slice; Claude/Codex
subscription selection is independent of that identity.

## Ownership and recovery

- No response, error, repository URL or browser storage contains a saved token.
  Submitted personal tokens exist transiently in the client and travel over the
  authenticated Runner transport. Private credentials are atomically persisted in an owner-only regular file
  beneath the resolved Runner state. No ambient `GH_TOKEN`, `GITHUB_TOKEN`,
  `gh auth`, Git credential helper or V1 auth store is used for this connection.
- Device polling survives a desktop disconnect; reconnect recovers the pending
  flow. Runner restart requires a new unfinished authorization. Completed
  authorizations survive restart, and refresh is serialized.
- Imports resolve the selected GitHub repository ID again, validate its
  canonical HTTPS identity, and clone a private bare source under
  `<state>/repositories/github-<id>/source.git`. Clone credentials exist only
  in the Git child environment, never in its arguments, origin URL or config.
- A bare clone's origin has no fetch refspec, so git would record no
  `origin/<branch>` for a workspace's push. Lazybox gives origin the one a
  normal clone has (`+refs/heads/*:refs/remotes/origin/*`): an agent's own
  `git push` then records the branch as pushed, for the sidebar, ahead/behind
  and the PR loop, and `git fetch` updates `origin/*`. The Runner adds it on
  start to copies imported before, when origin has no refspec at all; it
  fetches nothing and changes nothing else, and never touches a checkout the
  person registered.
- Registration persists in the Collection library. A completed owned clone
  can be recovered if registration failed; unknown existing paths are refused.
  Partial batches retain successful imports and retry only the remainder.
- Disconnect removes only this Runner's saved GitHub credentials. It retains
  repository copies, Workspaces and conversations. Remote GitHub authorization
  can be revoked separately in GitHub settings.

## The PR loop

**Changes** shows, above each repository's files, the one next step in bold:
**Commit N…**, **Push N** (or **Publish branch** the first time), **Create
PR…**, then the pull request's card, where **Send failing checks to the agent**
or **Merge** is next. Nothing happens without an explicit click.

- **Commit…** lists the changed files (all selected; untick to leave some
  uncommitted) and drafts the message from the conversation's title, else from
  the files. **Ask the agent to write it** sends the conversation that did the
  work a short request (visible in the conversation) and fills the box with
  its reply's fenced block; the agent is told not to run Git. **Commit &
  push** does both. ⌘↩ / Ctrl+Enter commits.
- **Create PR…** prefills the title (the single commit's subject, else the
  conversation's title) and a description listing the commits, with the
  repository's default branch as base and a draft option. It pushes unpushed
  commits first, and says so; uncommitted files are named as not included.
- The **card** shows state, review decision and every check run and commit
  status of the head, failing first, each with a **Logs** link. Every pull
  request the loop reads or creates is also [tracked](#tracked-pull-requests). It refreshes
  every 30 s while checks run, every 2 minutes otherwise (not while the
  window is hidden), and on demand.
- **Send failing checks to the agent** opens the conversation beside Changes
  and puts the failing checks, their results and log links in its message box,
  after any draft. It is never sent for you.
- **Merge** (squash, merge commit or rebase; the choice is remembered in this
  browser) is offered only when GitHub reports the pull request green and
  mergeable; otherwise the card says why (failing or running checks, more
  checks than Lazybox reads, a required review, conflicts, branch protection). It asks for confirmation, merges only
  the head shown (a later push makes GitHub refuse), and shows GitHub's own
  refusal, such as a branch-protection rule, as it is stated.
- **Update branch** is offered instead of Merge when the repository requires
  branches to be up to date and this one is behind its base (GitHub's
  `mergeable_state` `behind`), with green checks, no draft and no review still
  to come: the same rule as the Inbox's **Out of date**
  (`canUpdateBranch`, `pullStatus`). GitHub calls such a pull request mergeable
  (no conflicts) but refuses to merge it. It asks first, and updates only at the
  head the question was asked for: a push since closes the question. The update
  merges the base into the branch on GitHub, and its checks run again before it
  can merge.

The Runner RPCs are `CommitWorkspaceChanges`, `PushWorkspaceBranch`,
`CreatePullRequest`, `GetPullRequest`, `MergePullRequest` and
`UpdatePullRequestBranch`. They take a
workspace and member name, never a path; they are the owner's alone (an agent's
scoped credential is denied, like every RPC off its allowlist).

- Commit runs `git add --all` and `git commit` in that member's worktree only
  (selected paths commit with `--only`, leaving other staged changes as they
  were), refuses an empty commit or a detached HEAD, and runs the repository's
  hooks with the person's Git identity. A missing identity, a held
  `index.lock` or a failing hook is explained.
- Push sends `HEAD` to `origin` under the branch's own name with
  `--set-upstream`, never forced; a diverged remote branch is refused and
  explained. It records `origin/<branch>` itself too, for a source whose
  origin has a refspec that does not cover the branch.
- An HTTPS GitHub origin is pushed with the connection's credential as an HTTP
  `Authorization` header in the git child's environment only
  (`GIT_CONFIG_COUNT`), exactly as clone does: never in arguments, the remote
  URL, Git config, a credential helper or an error (stderr is redacted). The
  person's global and system Git config are left out of that push so a
  credential helper or URL rewrite cannot capture or redirect it, and it runs
  no hook (`core.hooksPath=/dev/null`): a `pre-push` hook the repository or an
  agent installed would inherit that environment. An SSH or
  non-GitHub origin is pushed with the person's own Git setup (SSH agent,
  `BatchMode`, no prompts) and never receives the GitHub credential. The
  repository's own config (hooks, `insteadOf`) still applies: whoever can edit
  it, including an agent working in the worktree, can already read the Runner's
  state as the same OS user, so this does not widen an agent's reach.
- Pull requests use the same connection through go-github. A branch's pull
  request is found by `owner:branch` (an open one first); the review decision
  is each reviewer's latest approving or blocking review, else
  `review_required` while a request is outstanding.
- Every list a decision rests on — the head's check runs and commit statuses,
  the reviews, a branch's pull requests — is read page by page to its end
  (GitHub's `Link` header), up to 1000 entries (`github.MaxListed`). A head
  with more checks than that, none of those read failing, is `incomplete`:
  never `success`, so Merge is not offered and `pr.checks-passed` is not
  emitted; the card says to check them on GitHub, and the sidebar and hover
  card say "checks not all read" (a neutral dot, never green). A failure among those read
  is still a failure. Reviews past 1000 are read from the newest end, so each
  reviewer's latest review still decides.
- A GitHub rate limit is told apart from a refusal on every request (sign-in,
  reads, writes, job logs): `x-ratelimit-remaining: 0` (until
  `x-ratelimit-reset`), `retry-after`, a 403 whose message names a rate
  limit, and any 429. The error says the connection still works and when to
  try again (`after 15:04 UTC, in about 12 minutes`; a minute when GitHub
  names no time), instead of sending the person to check the token's
  permissions or reconnect.

A repository whose origin is not on GitHub can commit and push; its card says
pull requests need GitHub. Without a GitHub connection, a GitHub origin says to
connect one.

## Tracked pull requests

The PR loop acts on a workspace branch's pull request. Agents also open pull
requests themselves, on other branches, with `gh pr create`. Lazybox tracks
every pull request a workspace **delivers**, whoever opened it, and brings
what happens to it back to the agent in one click
([PROJECTS_AND_INTEGRATIONS.md](PROJECTS_AND_INTEGRATIONS.md), phase 2; the
user guide is [Review an agent's work](docs/guides/review-agent-work.md#pull-requests-your-agents-open)).

### The store and the provider

`internal/runner/items` keeps, in one private file written atomically
(`<state>/items/items.json`):

- **links**: item, workspace (its name kept for history), conversation, role
  (`origin`, `delivers`, `mentions`), source (`person`, `branch`,
  `agent-output`, `cli`, `started-from`) and when;
- **snapshots** of each linked item (state category and label, head and base,
  checks with their runs, review decision, mergeable state, when it closed),
  with each read's validators;
- **branch sightings**: every branch a workspace repository has been on;
- the latest **transitions** (200).

A Runner restart loses nothing, and the first read after one is compared with
the persisted snapshot, so what happened while it was down is emitted once. A
deleted workspace's delivered pull requests stay, marked deleted (its mentions
and branches go).

GitHub is the built-in **provider**, behind the contract phase 3's extension
providers implement: `Name`, `Resolve(url)` (no network) and
`Refresh(items, etags)`, which returns only what changed; an optional
`BranchFinder` lists the pull requests of a branch. The sidebar's git
summaries read their pull request from the store (`BranchPullRequest`), so
`ListWorkspaceGitSummaries` never waits on GitHub.

### Detectors

| Detector | How | Role |
| --- | --- | --- |
| Person | The PR loop's **Create PR** (and any read of the branch's pull request) | delivers |
| Branch | Each branch the sidebar's summaries or the background pass (every minute) see a repository on is asked about (`head=owner:branch`, conditional) until a week after it was last seen: at the open interval once known to be pushed (a remote-tracking ref, or the upstream `push -u` records), else every five minutes (a push without `-u` from a source whose origin has no fetch refspec leaves no trace locally). A pull request opened after the branch was first seen (with an hour's grace) is linked, open or not; an older one only when its head commit is on the workspace's local branch (`merge-base --is-ancestor`): its work, opened before tracking saw the branch. Any other older one is someone else's work on a branch of the same name. The repository's default branch (`origin/HEAD` as the clone records it, else `main`, `master`, `trunk`, `develop`) is never asked about. | delivers |
| Agent output | The engine hands every tool call and result to the Runner (`TurnHooks.Tool`). A call whose command opens a pull request (`gh pr create`, `hub pull-request`, `glab mr create`, a tool named `create_pull_request`) delivers the PR URL on its own final output line or in the create tool’s structured result. The Runner reads the executable command from the tool input. A direct create after a push or PR-body heredoc is recognized. Help, dry runs, commands followed by another command, and ambiguous output require an explicit `lazybox link`. The Runner ignores the branch-creation hint (`/pull/new/branch`) printed by `git push`. For `lazybox run`, the CLI classifies the actual command arguments when registering the run. The editable console title does not determine whether a PR was created. On start, the Runner reads the end of each live workspace conversation's journal (updated this week, its last 5,000 records) once for what such a command printed while no detector ran (before tracking existed, or while the Runner was down), and remembers how far it read. Any other GitHub pull request or issue URL in a tool's output is a mention (at most five per output, a hundred per workspace), never tracked or notified. | delivers / mentions |
| Explicit | `lazybox link URL [--role]` with the agent's scoped credential (its own workspace and conversation only), or the owner's `LinkItem`. The briefing tells agents to run it when they open a pull request some other way. A URL (with or without `https://`), `--role` or `--workspace` makes `lazybox link` this verb, never the one that puts a release's CLI on PATH; in an agent, `lazybox link` alone asks for the URL. `lazybox items` lists the workspace's pull requests with their checks and review. An issue is never delivered. | delivers / mentions |

What agents link (agent output, `lazybox link` with an agent's credential)
is bounded, since every delivered pull request is polled: it delivers only a
pull request of a repository a workspace member's origin is (anything else is
recorded as a mention); at most 50 such delivered links per workspace and 20
per conversation (`ResourceExhausted` past them); and `lazybox link` at most
10 times a minute per conversation. A workspace keeps at most 500 links (its
oldest go first), so one workspace never pushes another's out.

### Tracking

A background pass every ten seconds reads what is due, in batches of 25:

- open pull requests about every **60 s** while a window has focus (a window
  asks `ListLinks` with `focused`; it counts for two minutes), every **5 min**
  otherwise;
- merged or closed ones **once a day**, until **7 days** after they ended;
- reads that keep failing back off (doubling, up to 30 min), an item never
  read successfully (a 404) included.

Two reads of one pull request can finish out of order (a reaction's read
beside the pass, the PR loop's): a read that began before the snapshot kept
was observed is dropped, so an older answer never replaces a newer one or
replays its transitions.

Every read is conditional: the pull request and each page of its head's
check runs and commit statuses carry their ETag (`If-None-Match`), and a 304
costs no rate limit. A list is unchanged only when every page answers 304; a
change on any page reads the list again from its first page. A read that
stopped at 1000 checks keeps no validators, so the next one is read in full
and says `incomplete` again rather than carrying a partial read forward.
Reviews are read again only when the pull request changed (submitting a
review changes it). A rate limit pauses tracking until its reset (at least
five minutes) and `ListLinks` says so; a revoked
credential stops it until reconnected. Any other failed lookup or read is
said too: `ListLinks.tracking_error` names what could not be read and why (a
private repository answers 404 to a connection that cannot see it), for the
workspace it concerns, until a read answers. The Pull requests panel shows it
as one quiet line; `lazybox items` prints it on stderr. Only linked, delivered
items are polled.

### Events

Each transition is emitted once, never on a poll that finds nothing new:
`pr.opened`, `pr.checks-failed` (per head), `pr.checks-passed`,
`pr.changes-requested`, `pr.approved`, `pr.merged`, `pr.closed`,
`pr.conflicts`. A pull request first seen open is opened; first seen already
failing, with changes requested or in conflict, it says so too; first seen
merged is history. `pr.checks-passed` needs every check read: checks that were
`incomplete` pass only once a read reaches them all. Each event goes to:

- **hooks** ([automate.md](docs/guides/automate.md)), with the pull request in
  the event (`LAZYBOX_EVENT_PR_URL`);
- the **delivering conversation's journal** as a `pull_request` record, one
  quiet line in the conversation (`WatchEvents`);
- **phone push**, under the existing settings: checks failed, changes
  requested and conflicts as failures, checks passed, approved and merged as
  ready for review;
- the **Inbox**, whose rows are derived from the current state (a row per
  delivered pull request that checks failed on, has changes requested, is in
  conflict, is out of date with a base it must match, or is ready to merge;
  **Dismiss** hides one until its state or head changes).

### Reactions

The **review-pr-ci** snippet instructs the original author to follow and repair CI
on the PR it prepares. The two PR panel reactions below require a click. Both go to the
conversation that delivered the pull request (else the workspace's most
recent), as a message Lazybox wrote with its origin chip, queued behind a turn
in progress.

- **Send the failures** (`SendPullRequestFailures`, prompt source
  `pr-checks`): for each failing GitHub Actions job (at most six), the job's
  log (`/actions/jobs/{id}/logs`, its storage redirect followed without the
  credential); its failing step (the one GitHub ended with `##[error]`) goes
  through `commands.Compressor`, the filter `lazybox run` uses, keyed by the
  step's command, so `go test` output gets the go-test filter. The full log is
  saved in the agent's output folder and named in the message. A token
  without Actions read access gets a note naming the permission; a check from
  another CI is named with its link and summary.
- **Address the review** (`ImportPullRequestReview`, prompt source
  `pr-review`): GitHub's inline review comments become Review comments on
  their lines (quoted from the diff hunk the reviewer read, anchored like any
  comment; replies folded in; author `github`, key `github:<id>`, each
  imported once), with the reviews' own summaries; the review loop's message
  for them is sent and they are marked sent. Replying on GitHub is not done.
  Only reviews and comments whose author is the repository's `OWNER`,
  `MEMBER` or `COLLABORATOR` (GitHub's `author_association`) are imported and
  sent; the others are counted in a note, to the person and the agent, and
  stay on GitHub. Comments and reviews are read from their newest end: a pull
  request with more than 1000 of either sends its newest 1000, and a note to
  the person and the agent names the list that was cut (review comments,
  reviews or both) and says the older ones were not read and stay on GitHub.

Both messages quote everything GitHub wrote — the title, check names,
summaries and logs, review summaries and comments — in one external-content
block (`workitems.QuoteExternal`, the frame `workitems.Quote` uses), with
framing that says it is data, not instructions; Lazybox's own words stay
outside it.
- **Merge**: `MergePullRequest` with `item` merges a tracked pull request on
  any branch of a workspace that delivers it, only at the head the person saw.
- **Update branch**: `UpdatePullRequestBranch` brings a tracked pull request
  that is behind a base its repository requires it to match up to date, only
  at the head the person saw. After a merge or an update, refused or not, the
  Runner reads the pull request again, so the row shows why GitHub refused
  rather than the state the person acted on. GitHub updates a branch in the
  background (it answers 202), so after an update the Runner reads it again,
  once a second for up to ten seconds, until the new head shows; if the caller
  leaves first, it records what it read. A sign-in GitHub refuses on the write
  itself is reported as the connection's problem.

### Access

`ListLinks` and `LinkItem` admit an agent's credential for its own execution
only (the Runner derives the workspace and conversation). Everything else —
`UnlinkItem`, the reactions, merging, updating a branch — is the owner's; a
paired phone reads with `read` and sends failures or a review with `respond`,
never merges or updates a branch.

## Review comments

Every diff on the **Changes** page (uncommitted, staged, and the branch
compared with the workspace start, the default branch or any other branch)
takes inline comments, and the agent receives them as one message.

- **Comment.** Hover a line and click **+**; drag along the **+** column or
  Shift-click a second line for a range (within one hunk). Removed lines take
  comments too. The box takes Markdown; ⌘↩ / Ctrl+Enter saves, Esc cancels.
  From the keyboard: Tab to a line's **+**, Enter (Shift+Enter extends).
- **Threads** sit under the last line they cover and say which diff and commit
  they were made on (`uncommitted changes on 1a2b3c4d`, `branch diff
  main…1a2b3c4d`). Edit, Delete (confirmed), Resolve, Won't fix and Reopen are
  on the card. A comment whose lines are not in the diff on screen is listed
  above it with the lines quoted.
- **Review tray.** **Review** in the Changes header (with the pending count)
  opens the tray: every comment of the workspace by file, filtered Pending,
  Sent, Done or All. Clicking one shows it in its diff. It opens by itself once
  there are comments, unless you closed it. The agent reviews under way, with
  their loop controls, stay at the top of the tray and Send at its bottom while
  the comments scroll between. In a narrow Changes panel (a chat docked beside
  it, a small window) the diff keeps its whole height and the left column shows
  **Files** or **Review**, tabs above it (←/→ between them; each keeps its
  scroll); Review in the header brings the tray to the front.
- **Send to the agent.** **Send N comments to the agent** sends one message directly to
  the conversation chosen in the tray (the current one by default), opened
  beside Changes. It includes each comment with `member/path:line`, the
  quoted lines in a fenced block, the diff it was read in, and whether it is
  outdated. Untick a comment to leave it out. After delivery, the
  comments are marked **Sent** to that conversation, and **Reopen** makes one
  pending again. With **Close the reviewer after sending** on (the default, remembered on this device), a reviewer whose
  review is over and has nothing left to hand on is ended and archived, never
  deleted ([agent types](AGENT_TYPES.md)).

### Anchoring and freshness

The Runner keeps each comment's member, path, side, line range, quoted lines
and their content hash (trailing whitespace ignored), the member's commit, and
the file's hash. On every read it anchors each comment against the working
tree, as V1's review `Freshness` does for a report:

| State | Meaning |
| --- | --- |
| current | The quoted lines are still at their lines. |
| moved | The same lines exist elsewhere; the comment re-anchors to the occurrence nearest where it was made. |
| outdated | The lines are gone (or the file is). The quote is kept and shown. A comment on removed lines is outdated once the file changes at all. |
| unknown | The file could not be read (too large, binary, unavailable). |

The diff on screen places a thread by its quoted lines, not by number alone,
because a branch diff shows the commit while the anchor is computed on the
working tree.

Disposition follows V1's finding outcomes: **open** (pending), **sent**,
**resolved**, **won't fix**. With **Resolve sent comments when a commit changes
their lines** ticked in the tray (off by default, per workspace), a sent
comment is resolved when HEAD has moved since it was sent, that commit range
changed the file, and the quoted lines are in neither HEAD nor the working
tree. It stays visible, labelled `Resolved by <commit>`, and can be reopened.

### Storage and access

Comments persist in `<state>/reviews/<workspace>.json` (private file, atomic
writes), so every window of the Runner shows them (windows re-read every 4 s
while visible and on focus) and they survive restarts; deleting the workspace
deletes them. The RPCs are `ListReviewComments`, `AddReviewComment`,
`EditReviewComment`, `DeleteReviewComment`, `SetReviewDisposition` and
`SetReviewAutoResolve`; each returns the whole anchored review. They take a
workspace, a member name and a relative path, never an absolute one, and are
the owner's alone: an agent's or a terminal's scoped credential is denied,
reading included.

Not implemented: publishing comments to GitHub as a pull request review,
comments in the Editor (it has no diff view), and replies within a thread.

## Agent review

Another agent reviews a conversation's work in one click, read-only, and its
findings land in the same Review tray as your comments. It is the first,
person-triggered step of the reviewer loop ([ORCHESTRATION.md](ORCHESTRATION.md),
slice 2).

Review conversations are read-only. Use **Open author conversation** on
the review banner to ask for implementation or merging. Reviewers file
findings through the Runner's dedicated tool, which returns a saved finding
ID without needing the review shell to reach the Runner over the network.
The CLI remains available for older review sessions.

**Where to start one.** Each starts a review of the *author* conversation's
work (the page's conversation; when the page shows a review conversation, the
one it reviews — a review is never itself reviewed):

| Entry point | Reviews |
| --- | --- |
| Changes header **Review with Codex ▾** | what the page compares: the workspace's changes since it started, uncommitted only, vs a branch, or one agent turn |
| A turn's ⋯ menu (and its messages' context menu) **Review this turn with Codex / with Claude** | exactly what that turn changed (its checkpoints) |
| The PR card **Review the PR with Codex ▾** | the pull request, from where it left its base |
| The Changes file list's context menu **Review the changes with Codex / with Claude** | the page's comparison |
| The conversation header's **Review** (`ReviewFromChat.tsx`; its tooltip names the reviewer, its chevron picks another for one run; in a slim panel it folds into the ⋯ menu) | the workspace's changes since it started (committed, uncommitted, untracked) |
| ⌘K **Review this workspace's changes** (`] ] ⇧R`), **Review changes with Codex / with Claude** | the workspace's changes since it started |
| The message box **`/review [codex\|claude] [focus…]`** (`/review` alone: the default reviewer; words that name no reviewer are the focus; `/review tray` opens the tray) | the workspace's changes since it started; nothing is sent to the agent |

The default reviewer is **Settings → Agents & workspaces → Review by another
agent → Default reviewer** (`ReviewSettings.reviewer_mode`): `other` (the
default) is the other agent than the author's — Codex reviews Claude's work,
Claude reviews Codex's and an ACP agent's; `same` is the author's agent in a
fresh conversation; `fixed` is `reviewer` (a profile id or a provider, ACP
agents included) on `reviewer_model` ("" its default). A setting saved before
the mode existed is `fixed` when it names a reviewer, else `other`. An entry
point that starts the default reviewer names none in `StartReview`, so the
Runner applies the setting (`reviewerProfile`); the client mirrors it to name
the reviewer (`review-model.ts reviewerChoices`). The chevrons list every
agent that can run and, for one with several, each subscription; naming one
starts exactly it, or fails when it is not available.

**When the default is not available** — its program is missing or cannot run
in the last probe (none is run to start a review), it has no profile, or all
of its subscriptions need a new sign-in — the other agent stands in for a
fixed one, and the author's own agent, in a fresh conversation, for the other
or the same one. The run's `fallback` says so in one sentence (*Codex is not
available, so a fresh Claude conversation reviews.*), shown quietly on the
review's card and in the button's tooltip, and appended to its `detail`.

**The focus** (`StartReviewRequest.focus`, at most 32 KiB; `/review <focus>`)
is a section of the brief (`{{focus}}`, or before `{{report}}` when the
template has no `{{focus}}`) and is kept on the run.

The chat’s review status follows reviews of that conversation. The workspace review
tray shows reviews across its conversations.

**Where the reviewer shows.** The client opens the reviewer conversation as a
background tab (`openReviewTab`, `workspace-model.ts openBesideChat`): a chat
pinned to it, right after the tab of the chat that shows the reviewed
conversation, in that chat's group, on the page that shows it (the selected
page first; else beside the page's conversation). The tab in front, the page's
conversation and the focus do not move; a chat already showing the reviewer
is kept (one panel per conversation), and a full page (`PANEL_LIMIT`) says the
review is in the sidebar. The tab is titled `Review · Codex` and its badge
(`reviewTabBadge`) is a live dot while it reviews, then `3 findings` (red with
a blocker open), `done` or `failed`. A conversation row that names a
conversation shown in a pinned tab brings that tab to the front and focuses
it, rather than switching the page's conversation.

**What starting does** (`StartReview`, owner only). The Runner opens a
conversation in the same workspace with permission **read-only** (Codex's
read-only sandbox; Claude's plan mode), titled `Review · <scope> · Codex`
(`Turn 3`, `Changes vs main`, `PR #12`), linked to the source conversation,
and sends it a brief generated from what it already records:

- the goal: the source conversation's first message (prompt log, else its
  first checkpointed turn) and the workspace's latest *plan* artifact;
- the scope and how to read it: `git -C <repo> diff <before> <after>` for a
  turn's checkpoint commits, `git diff <base>` / `merge-base` for changes,
  `diff origin/<base>...HEAD` for a pull request (with its number, title, URL);
- the files in scope with their status;
- the source's `lazybox run` reports (command, exit status, full-output path);
- how to report (below), that it is read-only, and to end with a summary.

The brief is a template (**Review brief** in the same settings; Reset returns
to the built-in one, stored as empty so it improves with Lazybox).
Placeholders: `{{goal}}`, `{{plan}}`, `{{scope}}`, `{{diff}}`, `{{files}}`,
`{{evidence}}`, `{{author}}`, `{{reviewer}}`, `{{conversation}}`, `{{focus}}`,
`{{report}}`; a template without `{{report}}` still gets the reporting
instructions at its end, or no finding would arrive. The brief is a message
Lazybox sent (prompt source `review`), shown in the reviewer conversation,
which is a normal conversation you can talk to.

**How findings arrive.** The reviewer files each finding with
`lazybox review add --file <repo>/<path> --line N[-M] --severity
blocker|major|minor|nit --title … --body …` ([CLI](services/runner/code/cmd/lazybox/README.md)).
The Runner reads the lines from the file as it is, so a finding is a review
comment anchored like yours (current, moved, outdated), written by that agent
(`from Codex`), with a severity (V1's words: critical/high are blockers,
medium is major, low is minor; an unknown word is refused, never demoted) and
a title. The same file, line and title (or `--key`) again updates that
finding instead of adding one. `lazybox review list` shows what it filed.
`AddReviewFinding` and `ListReviewFindings` are on the agent allowlist only
for the caller's own execution, and the Runner refuses an execution that is
not the reviewer of a review in its own workspace; the review, workspace and
author come from the execution, never the request.

**Triage.** A new finding is **To triage** (disposition `proposed`), never
sent as is. In the tray (filters by state, author and severity) and on its
card in the diff: **Accept** makes it pending; **Dismiss** asks why and keeps
the reason (`dismissed`); **Ask Codex** opens the reviewer conversation with
a follow-up about that finding started in its message box. **Send N comments
to the agent** sends accepted findings and your pending comments to the
author's conversation as one message (the author is preselected), each finding
exactly as filed: its severity, title, id and author, the quoted lines and the
body. The button sends the message immediately.

**Following it.** The tray lists the workspace's reviews, live
(`Codex is reviewing turn 3 · 1 blocker so far`). The author's conversation
gets a card where the review started (`review_started`): live while it runs,
with **Cancel review** ([below](#cancelling-a-review)), then `Codex reviewed
turn 3 · 2 blockers, 3 minor` with **Open review** (the tray, on that review's
findings) and **Open conversation**. A review is done when the reviewer's turn
ends (`review_finished`); one whose conversation ended or failed first says so.

**Missions.** When the author conversation is an attempt on a mission task,
the review is evidence on that task (`review:<id>`, kind `review`): pending
while it runs, then failing while blockers are open (flag `blockers`), which
keeps the task from *verified* even with green checks.

**Automatic reviews.** **Review every turn that changes files** (off by
default) starts a review of each turn that ended with changes, unless one of
that conversation is running or its loop is still going.

### The review loop

A review does not end at its first pass: the two agents work it out through
structured channels only, and you see every step.

1. **Round 1.** The reviewer files findings (`lazybox review add`) and ends its
   turn. The round closes with what it filed and how many blockers are open.
2. **Findings reach the author** exactly as filed — severity, title, finding
   id, the quoted lines, the body — never paraphrased: the Runner's message
   (`reviews.FindingsMessage`) and the tray's (`review-model.ts reviewMessage`)
   are pinned to one text by a shared golden file
   (`internal/runner/reviews/testdata/findings-message.json`). How they get
   there is the loop's mode, per review (the tray) and by default
   (**Settings → Review by another agent → Between rounds**):
   - **Ask me between rounds**: you accept and send them from the
     tray; nothing is sent for you.
   - **Send findings automatically** (the default): Lazybox sends open findings of every severity, including rounds with only minors or nits (accepting
     proposed ones) and marks them sent.
   - **One pass**: no re-review.
3. **The fix.** When the author's turn that began after the findings were sent
   ends *with changes* (its checkpoints), the next round starts. A turn without
   changes, or one that began earlier, does not count.
4. **Re-review.** The reviewer gets only that turn's diff
   (`git -C <repo> diff <before> <after>` of its checkpoints) and the findings
   still open, each to settle: `lazybox review resolve <id> --note "…"` or
   `lazybox review reopen <id> --note "…"` (back to pending, to send again).
   Silence is not a disposition: a finding it leaves alone counts as still
   open. New problems are new findings; open ones are not filed again.
5. **It ends** — always with its reason on the review card, in the tray and in
   the timeline: **Clean ✓** (no blocker open; said only when no major is
   open either — with majors open it reads *No blockers · 2 major open*, the
   detail *No blocker open after 1 round; 2 major still open.*, and the card
   offers **Send 2 findings to Claude**, which sends the remaining findings directly to
   the author), **max rounds** (a blocker open
   after the limit, 3 by default, 1–10), **stuck** (the same blocker still open
   after two fixes), **error** (a message could not be delivered), or you
   **stopped** or **took over**. Max rounds, stuck and error ask you: the tray
   marks the review *needs you* and the author's conversation gets a
   *Review needs you* line with **Open review**.

**Questions between the agents.** `lazybox ask author "…"` (from the reviewer)
and `lazybox ask reviewer "…"` (from the author) carry a question to the other
conversation as a labelled message (`[Lazybox] Codex (reviewing your work)
asks you: …`); the reply — that conversation's next turn — comes back to the
asker's call (it waits up to `--timeout`, 90 s by default; `--no-wait`
returns at once), or, once it no longer waits, as a message in its
conversation. Both conversations show the exchange as labelled lines
(*Codex asked Claude: …*, *Claude answered Codex: …*), and the review's
timeline keeps it. The messages Lazybox writes here and in the loop (the
brief, findings, a re-review, a question, its answer) carry their origin
(`MessageOrigin`, `origin` in the operation_accepted record's data): the
conversation shows each as one such line that opens to the exact text the
agent received, never as the person's message, and a line the question
already says is not shown twice. Only the author and the reviewer of one review in the same
workspace may ask each other (`AskAgent`, `WaitAgentAsk` on the agent
allowlist for the caller's own execution; the other side comes from the
review, never the request); one question per side at a time, none back while
the other side waits on yours (each would wait on the other), twelve an hour.

**Your controls** (tray, per review): **Between rounds** (the mode),
**Pause** (no re-review or automatic send until **Resume**), **Stop loop**, and
**Take over** (stops it and opens the author's conversation). The loop only
sends messages: it never pushes, merges or edits, and every agent keeps the
permission it started with. Its **Timeline** lists the rounds, findings,
verdicts, questions and answers, who did what and when.

**Durable.** The loop's state — mode, round, phase, rounds, timeline,
questions — is on the review in `<state>/reviews/<workspace>.json`, written
before the Runner acts on it; a restarted Runner re-attaches its agents' turn
hooks and carries on (a turn already under way when it restarted counts as
the round's, or answers the oldest question to that conversation).
Re-review and findings messages have stable operation ids per round.

Reviews are kept with the workspace's comments (`<state>/reviews/<workspace>.json`),
so they and their findings survive Runner restarts. Not verified against real
providers: whether Claude's plan mode lets the reviewer run `lazybox review add`
without an approval prompt (it may ask once), and whether Codex's read-only
sandbox reaches the Runner (the brief points it at Lazybox's command tool when
it does not).

### Cancelling a review

A review under way — its reviewer working, or its loop waiting for the next
round — can be cancelled from:

| Where | How |
| --- | --- |
| The review's card in the author's conversation | **Cancel review**, or `Esc` while the card has the focus |
| The reviewer conversation's ⋯ menu (and the author's, for its review under way) | **Cancel review** |
| ⌘K | **Cancel review** (the page's conversation's review, else the workspace's newest under way) |
| The person's terminal | `lazybox review cancel [<review-id>] [--workspace W]` ([CLI](services/runner/code/cmd/lazybox/README.md)) |

It asks first only when the reviewer has already filed findings (*Cancel this
review?*, with **Keep reviewing** focused); with none, it cancels at once.

**What cancelling does** (`CancelReview`, owner only; an agent's credential is
refused). The Runner records the cancel on the review first — state and stop
reason `cancelled`, `stopped_by` (*You*) and `stopped_at_unix`, a `cancelled`
line in its timeline, the round under way closed, a question between the agents
still waiting left unanswered — so nothing the agents do next starts another
round: neither the turn it then interrupts nor the author's next fix. It then
interrupts the reviewer's running turn and ends its session as **End session**
does. Its conversation stays (read it, or continue it in a new session) and so
do the findings it filed, in the tray as they were. The author's conversation
records it (`review_finished` with the state, who and when), and a mission task
it was evidence on gets no verdict from it (a blocker it filed still fails it).
The card then reads **Review cancelled · 2 findings kept**, *You cancelled it in
round 2*, with **Review again**: the same scope by the same reviewer, with the
same focus, in a new read-only conversation.

**Ending the reviewer by hand** stops its review too, so a card never stays
*reviewing* on a reviewer that will not report: **Stop** while it reviews (an
interrupted turn is not taken for the round's end), **End session**, or
deleting its conversation, at any point of a loop under way. The review reads
`stopped` (stop reason `ended`), the card **Review stopped · N findings kept**
with what happened (*You ended Codex's conversation; the review stopped.*) and
**Review again**. A Stop while the reviewer only talks to you after its round
changes nothing.

## Start a workspace from an issue

The user guide is [Start from an issue](docs/guides/start-from-an-issue.md);
the design is [PROJECTS_AND_INTEGRATIONS.md](PROJECTS_AND_INTEGRATIONS.md#start-a-workspace-from-an-item).

- **Access.** Reading issues needs **Issues: read** on the repository: a
  classic token's `repo` scope, a fine-grained token's Issues read, or the
  App's Issues read-only permission (already part of the App setup above). The
  picker uses GitHub's issue search API (`/search/issues`, 30 per page, at most
  1,000 results), whose `@me` qualifiers need a user token: personal tokens
  and App user tokens both are. Nothing is ever written to an issue.
- **Provider surface.** `internal/sources/github` reads (`IssueReader`:
  `SearchIssues`, `Issue`, `IssueComments`, which reads only the tail of a long
  thread). `internal/runner/workitems` is the provider contract (`Search`,
  `Get`, `Resolve`) that extension providers will implement too (phase 3);
  GitHub is its first implementation, with no Runner or UI state.
- **Search.** The picker's scopes are `mine` (`is:issue is:open archived:false
  assignee:@me`, the default), `recent` (`involves:@me`) and `all` (only with
  words). A key (`owner/name#N`) or an issue or pull request URL reads that one
  item without a search.
- **Context.** `GetItem` returns the body and the last 10 comments (at most 50)
  with authors and dates, and the exact text the agent receives
  (`workitems.Quote`): framed by `[Lazybox] External content: …` and
  `[Lazybox] End of external content from KEY`, every line between them quoted
  with `> ` (after `\r`, U+2028, U+2029 and U+0085 become line breaks, so no
  line escapes the prefix), so the content cannot close the block. The description is cut at
  16 KiB and each comment at 4 KiB, pointing at the URL for the rest. The
  desktop recognises the frame to show it as a chip (`item-context.ts`).
- **Origin.** `PrepareWorkspace` with `item` stores `Collection.origin` (a
  snapshot: provider, key, kind, title, URL, closing reference, when) in the
  workspace record. Without repositories it uses the issue's; without a branch,
  `<number>-<slug of the title>`, with `-2`, `-3` when a workspace already
  holds it; without a name, the title. When GitHub cannot be read the start
  goes on from the reference alone. `FindItemWorkspaces` answers "already has a
  workspace".
- **Briefing.** A workspace with an origin adds one line to the agent's
  workspace context: "This work started from acme/api#812 (GitHub issue).
  Reference it as `Fixes #812` in your commits and pull request."
- **Safety.** Item text is untrusted: quoted as above, shown before sending,
  and the agent starts in Ask unless the person picks YOLO in the sheet (or
  `--permission` in the CLI), even when the Runner's default is YOLO.
- **Not yet:** other providers, a project's `items:` mapping, write-back
  actions (move, assign), and keeping an item's state fresh over time (links
  and refresh belong to the PR tracking work).

## Scope and evidence

Implemented: personal-token and App connection lifecycles, App-token refresh,
user-repository and installation/repository pagination,
selected imports, durable library registration, Workspace membership,
read-only issue/PR RPCs for imported repos, the PR loop (tested against a
local bare remote and a fake GitHub API; not yet exercised against github.com),
and starting a workspace from an issue (search, read and quote, tested against
the same fake API; not yet exercised against github.com).
The repository browser does not expose a second issue/PR browsing workflow. See [runtime evidence](RUNTIME_EVIDENCE.md).

Also implemented: [tracked pull requests](#tracked-pull-requests) (detection,
conditional tracking, events, one-click reactions) and starting a workspace
from an issue, tested against the fake GitHub API in `sources/github/githubtest`,
not yet against github.com.

Not yet implemented: automatic reactions, replying to review comments on
GitHub, tracking issues over time, background inbox synchronization,
incremental fetching of imported source copies, publishing review comments to
GitHub, auto-merge, or remote Hub coordination. Re-importing an existing repository is idempotent; it
does not fetch a newer base. Live App identity and repository listing have been verified. Live personal-token
authorization, organization-policy behavior and private cloning remain unverified;
no personal credential was borrowed from local CLI stores for tests. These limitations must stay explicit before claiming full GitHub
dogfooding readiness.

### Repository picker discovery and removal

The shared repository picker groups GitHub entries alphabetically by owner/org,
then repository, with local repositories last. The Runner catalog refresh walks
all pages using the Runner's existing Lazybox credential: personal-token discovery
includes owner, collaborator and organization-member repositories; GitHub App
connections remain limited to accessible installations. It lists metadata first
and clones only selected repositories. Token permissions and organization/SSO
policy still bound the result; it does not borrow or change local CLI login.

**Remove** removes an entry from saved discovery and the visible repository
library. Git checkouts, remotes, branches and workspace worktrees are untouched.
Internal source references needed to manage existing worktrees are retained.
Explicit folder scans may rediscover a removed entry, and it can be re-added.

Repository discovery explicitly labels GitHub App connections as installation-scoped. It does not imply these results include every repository the user can access personally. Connections & folders provides “Use personal token…” through the existing explicit disconnect/connect flow; it never changes system GitHub credentials.

The catalog stores complete GitHub results on the Runner, scoped by account and
connection method. All repository pickers consume it. Pagination and credential
access live behind the Runner source interface, not React component state.
See [catalog configuration and workflow](WORKSPACES.md#catalog-boundaries-and-configuration).

### Behavior verification

The Behavior verification template runs an automatic review and repair loop.
The reviewer registers acceptance criteria with `lazybox review verify` or the
`verify` command tool action. The Runner executes each frozen command against
independent base and candidate checkouts and retains revision-bound command
receipts. Failed findings go to the author. After repairs, the Runner captures
the new candidate and reruns the same checks. Missing or interrupted evidence
cannot produce a clean result. Acceptance checks and their output are available
in the review tray and the versioned report.

Every review's reviewer, whatever its template, runs tests, builds and UI
gates the same way: `lazybox review test --member <repository> -- <command>`
(the `test` command tool action) runs the command in a disposable, writable
checkout of the captured candidate, with loopback network for the servers a
gate starts, never in the author's worktree and never in the reviewer's own
read-only shell. The result is recorded in the review: the author's
conversation hears it with the review's outcome ("2 tests: 2 passed"), the
Review tray lists each test with its output, and the report keeps them. A test is evidence,
not an acceptance check: it never makes a verification review clean.

See [Review agent work](docs/guides/review-agent-work.md) for command recipes,
regression tests, loop controls, execution limits and checkout isolation.
