# Projects, work items and integrations

**Status: accepted 2026-11-17.** Phase 2 is implemented: tracking the pull
requests agents open, their events and one-click reactions
([GITHUB.md](GITHUB.md#tracked-pull-requests)), and starting a workspace from a
GitHub issue. Projects (phase 1) and provider extensions (phase 3) are not. It
answers three requests:

1. "I often create a workspace from the same set of repositories. Can I save that
   set and start it again and again, save a workspace as one, and edit it?"
2. "When an agent opens a PR, Lazybox should track it."
3. "Make that generic, a plugin system, so that a workspace can start from a
   GitHub issue or a Linear ticket."

All three are one design. A **project** says *where* work happens: which
repositories, with which defaults. A **work item** says *what* the work is: an
issue, a ticket, or a PR. A **provider** is the plugin that brings items in from
outside and keeps them up to date. GitHub is the first provider and is built in;
Linear is the first provider written as an extension.

## What exists today (and what it lacks)

| Today | Where | Gap |
| --- | --- | --- |
| A workspace (internally a *Collection*) is a set of repositories, with one worktree per repository on the workspace's branch. | [COLLECTIONS.md](COLLECTIONS.md) | Each new workspace means picking the repositories again. The sheet remembers recent repositories one by one, never as a set. |
| COLLECTIONS.md already anticipates a "Hub project" as the portable grouping of repositories and objectives, with a Collection as its concrete environment. | [COLLECTIONS.md](COLLECTIONS.md) | It was never built. This proposal builds it. |
| The PR loop pushes, opens, reads and merges **the workspace branch's** PR, one per repository. The sidebar shows a PR chip from an in-memory cache (`pullRequestCache`, polled). | [GITHUB.md](GITHUB.md), `transport/git_summaries.go` | It misses a PR an agent opens on another branch, a stacked PR, or a PR in a repository outside the workspace. It forgets everything when the Runner restarts. There is no list of "my PRs in flight", no history after a merge, and no "send the CI failure to the agent". |
| GitHub issues and PRs can be listed read-only (`ListGitHubItems`). | `transport/github.go` | No UI starts work from an issue. |
| Hooks fire on `pr.checks-failed` among other events. | [automate.md](docs/guides/automate.md) | Only for the cached branch PR. No events for a review, a merge or an issue. |
| Extensions: sandboxed panels with grants and approved network hosts, behind an approval sheet. | [EXTENSIONS.md](EXTENSIONS.md) | UI only: nothing runs while no window is open, and nothing can bring data into Lazybox's own model. |
| The roadmap's *V2 801 Provider expansion* names Linear, Jira and Slack, behind "audited provider and credential boundaries, with shared mutation/reconciliation contracts". | [ROADMAP.md](ROADMAP.md) | This proposal is that contract. |

---

## 1. Projects: the set of repositories you start again and again

### The name

Everything inside the code calls a workspace a *Collection*, so "repository
collection" would collide with it. Two candidates:

- **Project** (recommended). It is what COLLECTIONS.md already planned, it reads
  naturally ("new workspace in *Payments*"), and the sidebar can group workspaces
  under it. It also gives tickets a home: a Linear team or a GitHub repository maps
  to a project.
- **Template.** It is the most literal name, but a template is something you copy
  and forget, so it suits grouping less well.

The rest of this document uses *project*.

### What a project holds

A project is a file in the config folder, `projects/<id>.yaml`. It sits next to
`layouts/*.yaml`, so it gets the same treatment ([POWER.md](POWER.md)):

- it is reread every second, with the last good version kept when a save breaks it;
- problems are reported by line;
- it has a JSON Schema for editors and an editor in Settings.

```yaml
# yaml-language-server: $schema=../project.schema.json
lazybox: 1
name: Payments
icon: credit-card
repositories:
  - github.com/acme/api                 # base: its default branch
  - id: github.com/acme/web
    base: release/2026.11               # a pinned base
  - id: github.com/acme/infra
    optional: true                      # offered, unticked by default
  - id: github.com/acme/api             # the same repository twice
    as: api-baseline
    base: main
branch: "{user}/{slug}"                 # {item.key}, {item.branch} when started from an item
agent:                                  # the new-task defaults; all optional
  provider: claude
  profile: work
  mode: plan
  permission: ask
layout: review                          # one of layouts/*.yaml
prompt: |                               # optional; the first message, or its preamble
  Work in the Payments stack. Run `make check` before you say you are done.
items:                                  # which work items belong here (section 2)
  - github: { repository: acme/api }
  - linear: { team: PAY }
```

**Repositories are identities, not paths.** An identity is the repository catalog's
`github.com/owner/name` or its git identity, so a project works on any Runner. A
repository that is not on this Runner shows as *Not on this Runner* with **Clone**,
using the `ImportGitHubRepository` that already exists. It does not fail the start.

**A project never changes a workspace.** A workspace records the project and the
project revision it came from (`Collection.project_id`, `project_revision`).
Editing the project changes the next workspace only. The relation stays useful in
both directions:

- *Workspaces of Payments* lists the workspaces started from it;
- when a workspace has drifted (an agent added `web` through
  `request-repo`), its ⋯ menu offers **Update Payments from this workspace…**.

### Using it: UX

**New task sheet.** The projects come first, as one row of chips with the most
recent first. A chip fills in the repositories, branch pattern, agent, mode and
layout. Everything stays editable for this task without touching the project.

```
┌ New task ─────────────────────────────────────────────────────────────┐
│ Project  [● Payments] [Mobile] [Docs site] [+ New project]            │
│                                                                       │
│ Repos    ☑ api  ☑ web (release/2026.11)  ☐ infra   + Add repository   │
│ Branch   antoine/fix-refund-rounding                    Payments ▾    │
│ Agent    Claude · work · Plan · Ask                                   │
│ ┌───────────────────────────────────────────────────────────────────┐ │
│ │ Fix the refund rounding bug in the ledger…                        │ │
│ └───────────────────────────────────────────────────────────────────┘ │
│ ◇ Save these repositories as a project                    [Start ⌘↵]  │
└───────────────────────────────────────────────────────────────────────┘
```

- `⌘1`…`⌘9` pick a project chip. **Esc** clears it back to ad hoc.
- When the repositories you ticked match no project, a quiet **Save these
  repositories as a project** link offers to keep them. The third time you start
  the same set, the link becomes one line: "You started api + web + infra 3 times.
  Save it as a project?" It shows once and is dismissible, never a dialog.
- From the palette: **New task in Payments** for each project, and **New task like
  this one** on any workspace, which reuses its repositories and agent without
  saving anything.

**Save a workspace as a project.** In the workspace's context menu and its header ⋯,
**Save as project…** opens a small sheet:

- the name;
- the repositories, all ticked, with their bases;
- three "also keep" checkboxes: this agent and mode, this layout, the branch pattern.

The sheet shows the YAML it will write, folded. Save writes the file.

**Edit.** There are three ways in:

- **Settings → Projects** lists every project and its workspaces, with an editor
  form (repositories with drag to reorder, base, optional) and **Open file**;
- a chip's context menu has **Edit**, **Duplicate** and **Delete**;
- in the workspace's ⋯ menu, **Update Payments from this workspace…** shows a diff
  (`+ web`, `− infra`, base `main → release/2026.11`) before saving.

Deleting a project leaves its workspaces as they are, ungrouped.

**Sidebar.** Grouping by project is optional and on by default once a project
exists. Workspaces sit under their project heading, ad hoc ones under *Other*.
Headings fold, and keep the sidebar width: no resize on fold, which is the
existing rule. Right-clicking a heading offers **New task in Payments**.

**CLI.**

```sh
lazybox project list
lazybox project save "Alpha" --name Payments     # from a workspace
lazybox new --project payments "Fix the refund rounding bug"
lazybox new --project payments --from PAY-142    # section 2
```

### Edge cases decided

- **The same repository twice** (`as:`) is already supported by `CollectionSpec`.
  The project only names it.
- **A missing optional repository** is skipped silently. A missing required one
  asks: *Clone it*, *Start without it*, or *Cancel*.
- **Multiple Runners**: projects are per Runner, like layouts. **Copy to Runner…**
  comes later; the identities make it a plain file copy.
- **Team projects** in a repository (`.lazybox/project.yaml`) are later work. They
  need the same approval as committed workspace scripts, since a project can carry
  a prompt.

---

## 2. Work items: what the work is

A **work item** is something outside Lazybox that work starts from or produces: an
issue, a ticket, a pull or merge request, an incident. Lazybox does not copy
trackers. It keeps a **reference**, a small **snapshot** of the fields it shows,
and **links**.

```text
ItemRef   { provider: "github" | "linear" | <extension>, key: "acme/api#812" | "PAY-142" }
Item      { ref, kind: issue | pull_request | ticket | …, title, url,
            state: { category: open | started | review | done | closed | merged, label: "In Review" },
            repository?: identity, branch_hint?, assignees, labels, updated_at,
            closing_reference?: "Fixes PAY-142",
            pr?: { head, base, draft, checks: success|failure|pending|none,
                   review: approved|changes_requested|review_required|"",
                   mergeable, failing_checks: [{name, url}] } }
Link      { item: ItemRef, workspace, conversation?, role: origin | delivers | mentions,
            source: started-from | branch | agent-output | cli | person, at }
```

The state is normalized into categories, so the UI and the events are generic: a
Linear "In Review" and a GitHub open PR awaiting review are both `review`. The
provider's own label is kept for display. The `pr` block is optional, and any
provider whose items are code reviews fills it (GitHub, later GitLab).

### Start a workspace from an item

> **Implemented for GitHub issues** (phase 2b): paste, the picker, the CLI, the
> quoted context, "already started", the origin chip and the briefing line. The
> project home (3) and write-back wait for projects and actions. Details in
> [GITHUB.md](GITHUB.md#start-a-workspace-from-an-issue); the guide is
> [Start from an issue](docs/guides/start-from-an-issue.md).

There are four ways in, and all of them land in the same New task sheet:

1. **Paste a link into the message box.** A Linear or GitHub URL becomes an item
   chip (`◆ PAY-142 · Fix refund rounding · Linear`). The project is chosen from the
   project's `items:` mapping, or the issue's own repository for GitHub. The branch
   comes from the provider's suggestion (Linear's `branchName`) or the project's
   pattern with `{item.key}`. The title comes from the item.
2. **From an issue…** in the sheet, or **Start from an issue** in the palette. A
   picker searches every connected provider, opening on *Assigned to me, not done*.
   It is keyboard-first, with the provider shown as an icon.
3. **The project's home**, the empty state of a project heading, lists its open
   items assigned to you. One click starts one.
4. **CLI**: `lazybox new --from PAY-142` or `--from https://github.com/acme/api/issues/812`.

**What the agent gets.** The item's title, body and recent comments go in as an
**attached source** (a chip on the first message, as for console runs and
snippets), not as pasted text. The sheet shows exactly what will be sent and lets
you untick the comments. The text is wrapped as quoted external content, with
author and URL (see *Safety*). The workspace briefing gains one line: "This work
started from PAY-142 (Linear). Reference it as `Fixes PAY-142` in your commits and
pull request." Linear and GitHub then link the PR on their own side.

**Already started?** If an item is already a workspace's `origin`, the sheet says
"PAY-142 already has a workspace, *Payments · fix-refund*", with **Open it** or
**Start another**.

**Write-back is opt-in, per action, and visible.** The sheet can show a remembered
checkbox: ☐ *Move PAY-142 to In Progress and assign me*. Lazybox never changes a
tracker on its own. Every write it makes is logged in the workspace activity, for
example "Moved PAY-142 to In Progress".

### Track the pull requests agents open

Whatever opened a PR, Lazybox should know about it. The workspace gets
`delivers` links from four detectors, in order of confidence:

| Detector | How | Confidence |
| --- | --- | --- |
| **Person** | The PR loop's **Create PR**, as today. | certain |
| **Branch** | Any branch a member worktree has been on during the workspace, not only the current one. The Runner records the branches it sees, and the existing summaries poll already reads HEAD. | high |
| **Agent output** | The Runner already sees every tool call and result in the transcript, and every `lazybox run` capture. A provider declares URL patterns. A PR URL printed by a command that creates one (`gh pr create`, `glab mr create`, `git push` hints don't count) is linked as `delivers`. Any other recognized URL is `mentions`: listed, not tracked, never notified. | high |
| **Explicit** | `lazybox link <url>` (`--role delivers`) from an agent or a person, or a URL dropped onto a workspace. The briefing tells agents: "If you open a PR some other way, run `lazybox link URL`." | certain |

When the `gh pr create` output is missed, the branch detector catches the PR on
the next poll. The two together cover agents in practice. The explicit command is
the fallback that never depends on output parsing.

**Tracking.** Linked items are kept fresh by their provider:

- open PRs every ~60 s while the app has focus and ~5 min otherwise;
- open tickets every few minutes;
- done items once a day, until 7 days after merge or close.

GitHub uses conditional requests (ETag, so an unchanged item costs no rate
limit), batched by repository. Snapshots and links persist in the Runner's state,
so a restart loses nothing and history keeps a deleted workspace's PRs.

**Events.** `pr.checks-failed` stays. New events: `pr.opened`, `pr.checks-passed`,
`pr.changes-requested`, `pr.approved`, `pr.merged`, `pr.closed`, `pr.conflicts`,
`item.state-changed` (with `from` and `to` categories) and `item.commented`. All
of them reach hooks ([automate.md](docs/guides/automate.md)), the journal
(`WatchEvents`), the Inbox and phone push, under the existing push settings.

### Reactions: where Lazybox gets ahead

A tracked PR is only useful if what happens to it goes back to the agent with one
click. Each PR event becomes an Inbox row, with the action already prepared:

```
┌ Inbox ────────────────────────────────────────────────────────────────┐
│ ✕ acme/api#812 checks failed · lint, unit-api (2 of 14)    Payments   │
│   [Send the failures to Claude]  [Open checks ↗]  [Dismiss]           │
│ ◐ acme/web#77 changes requested by a reviewer · 3 comments            │
│   [Address the review]  [Open review ↗]                               │
│ ✓ acme/api#809 approved · checks green                                │
│   [Merge ▾]  [Open ↗]                                                 │
└───────────────────────────────────────────────────────────────────────┘
```

- **Send the failures.** Lazybox fetches the failing jobs' logs (the GitHub Actions
  logs API needs *Actions: read*) and compacts them with **the same filter as
  `lazybox run`**: failures kept, progress dropped, the full log saved to a file.
  It sends them to the conversation that delivered the PR, with a *PR checks*
  source chip. The agent receives what it would have seen running the suite
  itself.
- **Address the review.** GitHub review comments are imported into Lazybox's
  **Review** panel as comments on their lines, using the existing review model
  (`AddReviewComment`, dispositions), and the review loop's brief goes to the
  agent. When the agent resolves them, the panel offers **Reply on GitHub**, a
  person-initiated write-back.
- **Merge** is the existing `MergePullRequest`.

**Automatic reactions are per project and off by default.** For example:

```yaml
reactions:
  checks-failed: send
  changes-requested: ask
```

Guards keep them from looping:

- at most 3 automatic sends per PR per day;
- an identical failure twice in a row stops and asks;
- never while the conversation is mid-turn: the send is queued as a queued message.

**Where it shows.**

- **Workspace header:** PR pills, more than one if needed, each with its checks
  dot.
- **Sidebar row:** the most urgent state as one badge (✕ checks, ◐ review, ✓ ready).
- **A new *Pull requests* panel and palette view** shows every tracked PR across
  workspaces, in three groups:
  - **Needs you:** changes requested, checks failed, approved and ready to merge.
  - **Waiting on others.**
  - **Merged this week.**

  Keyboard: `j`/`k`, `↵` opens the workspace, `s` sends failures, `m` merges.
- **Phone:** Inbox rows and push. "Checks failed on #812, *Send to Claude*" works
  from the lock screen with the `respond` grant.

---

## 3. Providers: the plugin system

### One extension concept, two kinds of parts

Lazybox already has extensions, with an approval sheet, grants, approved hosts,
validation by line, hot reload and `lazybox extension new`. Starting a second
plugin system would split all of that. Instead, **an extension can contribute
providers as well as panels**: one manifest, one approval, one place in Settings.

```yaml
# extensions/linear/extension.yaml
lazybox: 2
name: Linear
version: 1.0.0
providers:
  - id: linear
    entry: provider.js
    items:
      - {kind: ticket, noun: Linear issue, icon: ticket}
    urls: ["https://linear.app/{workspace}/issue/{key}/*"]
    auth:
      kind: token                           # later: oauth2 (PKCE), run by the Runner
      label: Personal API key
      help: https://linear.app/settings/account/security
panels:
  - {id: board, title: Linear, icon: ticket, entry: board.html}   # optional
grants: [items:read, items:act]
network: [https://api.linear.app]
```

### Where provider code runs

Providers must run **in the Runner**, not in a panel. The Runner is the always-on
part, and it is what the phone talks to and what tracks PRs while no window is
open. There are three ways to do that:

| Option | Gains | Costs |
| --- | --- | --- |
| **A. JavaScript in the Runner, in an embedded engine (goja, as k6 uses), with a bridge.** Recommended. | The same security model as panels: the code can do only what the bridge exposes, and fetch only from approved hosts. Nothing to install (no Node). Hot reload. The same language and `lazybox.d.ts` as panels, so an agent can write a provider for you. | Needs a bundling step for npm SDKs (`lazybox extension build` with esbuild). No Node APIs. goja is slower than V8, which does not matter for API calls. |
| B. Any program over JSON-RPC on stdio, the way LSP, MCP and ACP do it. | Any language and official SDKs; the simplest to build. | Runs with your full permissions, so grants and hosts cannot be enforced. That is acceptable for your own code (hooks already work like that) but wrong for anything shared. |
| C. MCP servers as providers. | Linear and GitHub already ship them. | Tool results are free-form text whose shape changes between server versions. Mapping them into items is brittle. MCP stays what agents use; Lazybox's model needs structured items. |

**Recommendation: A, and define the contract as JSON messages,** the way LSP
defines its own. B can then be added later as an `exec:` provider for trusted
local code (*runs as you, unsandboxed*, like a hook's `run:`) without changing
anything else. The **built-in GitHub provider is Go code implementing the same
contract**. It proves the contract, and the current PR cache and loop move behind
it with no protocol break.

### The provider contract

```ts
export default lazybox.provider({
  // Search for the picker; "mine" is "assigned to me, not done".
  async search({ query, scope /* 'mine' | 'recent' | 'all' */, cursor }) { return { items, next }; },
  // One item with its context for the agent: body and recent comments as Markdown.
  async get(key) { return { item, context }; },
  // Batched snapshots for tracking; return only what changed (ETags kept per key).
  async refresh(keys, { etags }) { return { items, etags }; },
  // A URL the manifest's patterns matched, to a key (when the pattern alone cannot say).
  async resolve(url) { return key; },
  // What a person may do to this item, in words; Lazybox shows them, never runs them by itself.
  async actions(item) { return [{ id: 'start', label: 'Move to In Progress and assign me' }]; },
  async run(actionId, key, input) { return { item, message: 'Moved PAY-142 to In Progress' }; },
});
```

**The bridge a provider gets** is deliberately small:

- `lazybox.fetch(url, init)` reaches HTTPS on approved hosts only. `{ auth: true }`
  makes the Runner add the stored credential's header, so **the code never sees the
  secret** and an update cannot carry it to another host.
- `lazybox.cache`: a small key-value store per extension.
- `lazybox.log`.

There is nothing else: no files, no Runner APIs, no agents. A provider produces
data, and Lazybox decides what to do with it.

**Limits:**

- 10 s per call, 4 concurrent calls per provider, a bounded heap;
- a crashing or slow provider degrades to a quiet chip, "Linear unavailable:
  timed out", with **Retry**;
- a failure never blocks a workspace start, which can still go ahead without the item.

**New grants**, written in the approval sheet as sentences:

| Grant | In the sheet |
| --- | --- |
| `items:read` | Brings Linear issues into Lazybox: search, show and keep them up to date |
| `items:act` | Changes issues in Linear **when you choose an action** (for example *Move to In Progress*) |

When `items:act` is combined with a network host, the existing warning applies
("what it can see, it could send to these sites").

**Credentials** are entered in Lazybox (**Settings → Connections**, next to
GitHub) and stored the way the GitHub credential is (`githubconnection`,
`WithCredential`). Providers never see them, panels never see them, and agents
never get them.

**Agents read items through the CLI**, with the scoped agent credential they
already hold:

- `lazybox item show PAY-142`, `lazybox items` (this workspace's links and their
  states), `lazybox link URL`;
- `lazybox pr checks` prints the compacted failure logs.

An agent can read only the items linked to its own workspace, and can never write
to a tracker.

### What ships when

| Provider | Kind | When |
| --- | --- | --- |
| GitHub issues and PRs | built in (Go) | with the item model (phase 2) |
| Linear | bundled example extension, like the *Agent board* | phase 3, as the proof of the extension API |
| Jira, GitLab, Sentry, Slack threads | extensions, written by anyone or by an agent | after phase 3 |

---

## Safety

- **Item text is untrusted input to an agent.** An issue body can carry a prompt
  injection. Lazybox:
  - sends it as quoted external content, labelled with its author and URL;
  - shows it before sending;
  - **never starts an agent from an item in bypass or auto-approve permissions
    unless the person picked that mode in the sheet**.

  The phase-4 triggers below always start in *Ask*, with the first message held
  for the person unless the rule says otherwise. The rule, approved once, shows
  its exact scope.
- **No inbound webhooks.** The Runner stays on loopback and the tailnet
  ([REMOTE.md](REMOTE.md)), so tracking polls with conditional requests. Webhooks
  would need a relay, which is a separate decision.
- **Writes to trackers** happen only through a provider's declared action, chosen
  by a person, or through an automatic reaction the person turned on for that
  project. Every write is logged in the workspace.
- **Rate limits.** Only linked items are polled, batched, with ETags, and with
  back-off on 403 or 429. The current interval and the last error are shown in
  Settings → Connections.

## API sketch (Runner protocol)

The protocol only grows; existing RPCs and fields stay, and `buf breaking` must stay green.

```proto
// Projects are config files: GetConfigFiles / UpdateConfigFile cover read and write.
rpc ProjectFromWorkspace(CollectionRequest) returns (ProjectDraft);      // "Save as project…"
PrepareWorkspaceRequest { … string project_id = 7; ItemRef item = 8; }
Collection { … string project_id = 11; uint64 project_revision = 12; ItemRef origin = 13; }

rpc ListProviders(ListProvidersRequest) returns (Providers);             // state, auth, last error
rpc ConnectProvider(ConnectProviderRequest) returns (Providers);
rpc SearchItems(SearchItemsRequest) returns (ItemPage);
rpc GetItem(ItemRequest) returns (ItemDetail);                           // with context for the agent
rpc ResolveItemUrl(ResolveItemUrlRequest) returns (ItemRef);
rpc ListLinks(ListLinksRequest) returns (Links);                         // by workspace, conversation, or all tracked
rpc LinkItem(LinkItemRequest) returns (Links);
rpc UnlinkItem(UnlinkItemRequest) returns (Links);
rpc RunItemAction(RunItemActionRequest) returns (ItemActionResult);
rpc SendPullRequestFailures(PullRequestRequest) returns (Receipt);
rpc ImportPullRequestReview(PullRequestRequest) returns (WorkspaceReview);
// Journal: item_linked, item_changed (with old and new state category), reaction_sent.
```

## Phases

| Phase | Scope | Done when |
| --- | --- | --- |
| **1. Projects** | `projects/*.yaml` with schema and hot reload; chips in New task; Save as project; Update from workspace; Settings → Projects; sidebar grouping; palette and CLI; "New task like this one". | Two clicks from the project to a running agent. Release gates: save, edit, start, drift and update, missing repository. |
| **2. Items and PRs (GitHub, built in)** | Item and link model, persisted; start from an issue (paste, picker, CLI); `delivers` detection (branch, agent output, `lazybox link`); tracking with ETags; the new PR events; Inbox reactions (send failures, address review, merge); Pull requests panel; push. The PR cache and loop move behind the provider contract. | An agent's `gh pr create` on any branch shows in the header within one poll. A failing check reaches the agent in one click, compacted. Nothing is lost on a Runner restart. |
| **3. Provider extensions** | `providers:` in the manifest; the goja bridge (`fetch` with injected auth, cache, limits); new grants in the approval sheet; Settings → Connections; Linear as a bundled extension; the projects' `items:` mapping. | Linear works end to end: paste a URL, start, PR linked back, state tracked, with the provider code never holding the key. |
| **4. Triggers** (later) | Watch rules ("issues in PAY labelled `agent` start Payments in Plan mode, first message held for me"); `exec:` providers for trusted local code; possibly Linear's agent sessions ("assign to Lazybox"), which need a relay. | Explicit rules only; every automatic start is visible and undoable. |

Phases 1 and 2 do not depend on each other and can run in parallel. Phase 3 needs phase 2's model.

## Decisions (user, 2026-11-17)

1. **The name is *Project*.** Workspaces are grouped under their project in the sidebar.
2. **Providers are TypeScript run in the Runner** (option A). Option B, an `exec:` provider, stays a later option for trusted local code.
3. **Automatic reactions are off by default.** Reactions are one click from the Inbox; automation is a per-project opt-in.
4. **PR tracking comes first.** Phase 2 (GitHub items and PRs) starts before phase 1 (projects) and before Linear.
