# Linear "work on" → repo routing, branch/PR conventions

Status: **partially shipped.** The dogfoodable core (repo routing +
branch template + prompt injection) landed in **#905/#918**; this doc is
kept as the design record, with the multi-repo router (§2/§3, phase 5)
still pre-implementation. Owner: TBD.

> **Update (#905/#918).** Problems #1–#3 below are the *original*
> pre-implementation state — they were fixed by the merge that precedes
> this doc, not still-open bugs. What shipped: team→repo resolution in
> `repo_for_workspace_provision` (errors loudly on an unmapped ticket
> instead of cloning `linear/<team>`), the `{handle}/{type}/{id}-{slug}`
> branch-template engine, and a real Linear work prompt via a new
> `ImplementLinear` classification. Still deferred: multi-repo fan-out,
> the LLM router, and auto-`about` ingestion (phases 4–5). See "Phased
> implementation" for the shipped/deferred split.

## Problem (original, pre-#905)

Before #905, working on a **Linear** issue (`w`) was broken and
convention-less:

1. **No repo mapping.** A Linear task's `repo` is the synthetic string
   `linear/<team>` (e.g. `linear/OBI`) — not a real GitHub repo. So
   `provision_worktree` would try to clone `git@github.com:linear/OBI.git`
   (`spawn_handler.rs`, `git-ops/src/lib.rs`). A Linear ticket has no idea
   which GitHub repo the work belongs in. *(Fixed: `repo_for_workspace_provision`
   now maps `providers.linear.teams` → repo and errors on an unmapped ticket.)*
2. **No naming convention.** For a branchless task, lazybox derives a
   generic `linear-<identifier>-<slug>` (`derive_branch_for_branchless`,
   `spawn_handler.rs`). It ignores Linear's own `gitBranchName` and
   does not match the obin house convention (below). *(Fixed: a
   `branch_template` engine renders the house convention below.)*
3. **No prompt.** A Linear-only workspace classified as `StartHere`
   (`tui-core/src/intent.rs`), so `w` spawned a **bare agent with no
   prompt** — none of the GitHub `Closes #N` / PR-title guidance was
   injected for Linear. *(Fixed: Linear tickets now classify
   `ImplementLinear` and get a real prompt.)*

## Resolution order & the unmapped-team picker (#1041)

`repo_for_workspace_provision` → `linear_repo_for_task`
(`spawn_handler.rs`) resolves a Linear ticket's repo in this order:

1. **Linked GitHub PR** under the team mapping's owner *refines* the
   mapping (the precise repo for a multi-repo team).
2. **`providers.linear.teams` mapping** (team key → `owner/repo`) — a
   trusted, explicit signal; a foreign-org linked PR never overrides it.
3. **Linked GitHub PR** alone resolves an unmapped or teamless ticket.
4. Otherwise: a **hard error** naming the missing config key.

Step 4 used to dead-end `w w` for a fresh user or a new team: the only
way forward was hand-editing `config.yaml`. It no longer does — and it
never surfaces as a failure to the user either. The daemon still errors
loudly (classified `WorktreeRecovery::LinearUnmapped`, `crates/ipc`), but
the client treats that class as a *missing choice*, not a breakage: `w w`
on an unmapped team opens the **repo picker directly** — no "× spawn
aborted / retry once fixed" modal, no manual `r`. Both surfaces of the
failure (the `WorktreeProgress::Failed` step and the `spawn:worktree`
provider error) route to the same idempotent
`open_linear_team_repo_picker` (`crates/tui`), tearing down the in-flight
spinner in favor of the picker. The pick persists the choice as
`providers.linear.teams.<team>` via `Config::save_with` and immediately
re-issues the spawn; because the daemon reloads `config.yaml` on the next
provision, the re-spawn resolves through step 2 with no manual retry. The
mapping is asked **once** per team. The classified failure modal (with its
`r` pick affordance) survives only as the genuine last resort — reached
when there is no tracked GitHub repo to propose at all.

The picker's repo list is **ranked**, not blank: repos that other tickets
in the *same* team already link a GitHub PR to (learned from their
`linked_tasks`) float to the top, so the common case is one keystroke.

### Inference: what auto-resolves today, and what doesn't (#1041 investigation)

Only **one** inference path is wired: `linked_github_repo(task)` reads the
ticket's *own* `linked_tasks` attachment (a `github.com/<owner>/<repo>/pull/<n>`
URL surfaced by #922) and routes to that repo. That is the entirety of
"inference" in the resolution order — steps 1 and 3 above.

Why `OBI` isn't auto-resolved: the ticket the user pressed `w w` on has
**no linked GitHub PR** (the work hasn't started, so no PR exists to
attach), and lazybox learns nothing about a team's repos from *other*
tickets. Neither of the doc's speculative sources is implemented:

- **Learning from sibling tickets** — scanning the same team's other
  tickets for their linked PRs to derive the team's repo set — does *not*
  auto-resolve the clone target. Each ticket is still resolved in isolation
  in `linear_repo_for_task`. It *does* now feed the picker's ranking
  (`Sidebar::github_repos_ranked_for_linear_team`): a repo a sibling ticket
  links floats to the top of the choice, so the likely answer is one
  keystroke — a *proposal*, not a silent auto-route.
- **Org-repo inference** — deriving the repo from the org's repo list plus
  the ticket identifier/branch — is *not* wired.

Auto-routing from either source is deliberately not done: both are
heuristic (a team legitimately spans several repos), and a wrong silent
guess is worse than one deterministic pick. The persisted picker teaches
the mapping for good; sibling-ticket signal only *ranks* the proposal.

## What obin actually does (the convention to honor)

Observed across `obin-ai/obin-platform` + `obin-ai/obin-infra` (30 recent
PRs each). There are **very few** patterns:

**Branch — one pattern:**

```
{handle}/{type}/{id}-{slug}
   luka/feat/obi-1749-template-sa-seam
   gm/fix/obi-1967-robin-workshop-dev-deploy
   antoine/fix/robin-workshop-ui-polish     # {id} optional (ad-hoc work)
```

- `{handle}` — short personal handle (`antoine`, `luka`, `owenwhite`,
  `gm`, `cris`, …). **Not** the GitHub login (`antoinetoussaint-byte`)
  and **not** Linear's username (`antoinetoussaint`).
- `{type}` — conventional-commit type: `feat` / `fix` / `chore` / `docs`.
- `{id}` — Linear identifier lowercased (`obi-1749`), **optional**.
- `{slug}` — kebab-cased title.

Linear's own `gitBranchName` is `antoinetoussaint/obi-1964-<slug>` — it
**does not match** (no `/type/` segment, longer handle), so we template
rather than use it verbatim.

**PR title — two patterns:**

1. Dominant: `[{area}] {human sentence}` — `[platform] …`, `[oncall] …`,
   `[deal-intelligence] …`. `{area}` is a component, **not** in Linear
   data — a human/agent judgment.
2. Bots/infra: `{type}({scope}): {desc}` conventional-commits.

**Mechanical vs judgment tokens:**

| token           | source                                                        | mechanical |
| --------------- | ------------------------------------------------------------- | ---------- |
| `{handle}`      | per-user config                                               | yes        |
| `{id}` `{slug}` | Linear identifier + title                                     | yes        |
| `{type}`        | Linear label → `Bug=fix, Feature/Improvement=feat, Tech Debt=chore` (fallback `feat`) | mappable   |
| `{area}` (PR)   | not in Linear — the component touched                         | agent      |

## OBIN mapping reality

- **Team is the natural repo key**, not labels or projects. OBIN labels
  are type-only (`Bug`/`Feature`/`Improvement`/`Tech Debt`/`m:…`) — no
  repo/component taxonomy. Projects are team-scoped scope buckets.
- Teams: `Obin` (`OBI`), `NYL360`, `IT Support`. A team maps to a *set*
  of repos (a monorepo-ish product line can still span platform + infra),
  so team → **repo set**, refined per-ticket by inference.

## Design

### 1. Config: Linear team → repo set (+ per-repo `about`)

This is the **target** multi-repo shape. What shipped in #905/#918 is
the single-repo subset: `providers.linear.teams` is a flat
`team → owner/repo` map (`BTreeMap<String, String>`), and the label→type
map is keyed `label_types` (not `type_from_label`). The `repos:`-list
form and per-repo `about` below are the deferred multi-repo design, not
yet deserializable — see the phased split.

Shipped today:

```yaml
providers:
  linear:
    handle: antoine
    branch_template: "{handle}/{type}/{id}-{slug}"
    teams:
      OBI: obin-ai/obin-platform      # one repo per team
    label_types:
      Bug: fix
      Feature: feat
      Improvement: feat
      Tech Debt: chore
```

Deferred multi-repo target:

```yaml
providers:
  linear:
    handle: antoine                     # {handle} for branch names
    label_types:                        # {type} mapping, optional
      Bug: fix
      Feature: feat
      Improvement: feat
      Tech Debt: chore
    branch_template: "{handle}/{type}/{id}-{slug}"   # default = obin's
    teams:
      OBI:
        repos:
          - obin-ai/obin-platform
          - obin-ai/obin-infra
      NYL360:
        repos:
          - obin-ai/<nyl-fork>
```

`about` (what "Linear knows about each repo") is **auto-derived**, not
hand-maintained: `CLAUDE.md` header if present, else a short `README`
ingest, cached per repo. A repo appears in the routing candidate set
either via a team's `repos` or (fallback) the user's accessible repos.

### 2. `w` on a Linear ticket — the flow

```
w on OBI-1964
  ├─ 1. resolve candidate repos  (team.repos, else accessible repos)
  ├─ 2. ROUTER: headless structured run (Claude stream-json, -p)
  │        prompt = ticket (title+desc+labels) + each repo's `about`
  │        output = { repos: [{repo, type, confidence, why}], ... }   (structured JSON)
  ├─ 3. MODAL: multi-select, pre-checked to the router's pick(s)
  │        (your "always-show, pre-picked" choice; high confidence may auto-accept)
  ├─ 4. PROVISION worktree(s), one per chosen repo
  │        branch = branch_template filled (id, slug, handle, type)
  └─ 5. WORK AGENT prompt (shipped single-repo; sibling-repo lines deferred) per worktree:
           ticket body + this repo's `about` + sibling repos
           + convention: "branch <…>; PR title `[<area>] <summary>`;
             body starts `Fixes OBI-1964`; link <issue url>."
```

- **No MCP.** The router reuses the existing headless
  `StructuredAgentProtocol::ClaudeStreamJson` path (`agents/src/agent.rs:64`)
  — a one-shot classification, not a live session, so nothing hooks back
  into lazybox. (MCP would only be needed if a *live* agent had to create
  worktrees mid-run, which lazybox intentionally avoids.)
- **One ticket → N repos** falls out of the multi-select + per-repo
  worktree provisioning. Same branch stem in each repo; all PR bodies say
  `Fixes OBI-1964`, so Linear links every branch/PR to the one ticket.

### 3. Modeling: one workspace or many? (OPEN)

A two-repo ticket is either (a) **one workspace with N agent sessions**
(N worktrees under one sidebar row) or (b) **N sibling workspaces** keyed
by ticket+repo. Leaning (a) — keeps the ticket a single inbox row —
but needs a workspace whose sessions span repos. **Decide before Phase 3.**

## Generalizability

Everything above is data: team→repo-set, `branch_template`, label→type
map, and the injected PR-convention text. A different org fills in
different repos + template + prompt; nothing obin-specific is hard-coded.
Default `branch_template` and label map ship as obin's so it works out of
the box for this repo, overridable everywhere.

## Phased implementation

1. **Config + repo resolution.** ✅ Shipped (#905/#918). `LinearConfig`
   (handle, teams→repo, template, label map). Replaced the `linear/<team>`
   repo with the mapped GitHub repo in `repo_for_workspace_provision`.
   Single-repo, no router yet. *Dogfoodable: `w` a Linear ticket → correct
   repo, correct branch name.*
2. **Naming.** ✅ Shipped (#905/#918). Branch-template engine (tokens) +
   label→type, with regression tests against the obin examples in this doc.
3. **Prompt injection.** ✅ Shipped (#905/#918). Linear `w` gets a real
   prompt (ticket + convention) via a new `ImplementLinear` classification
   instead of `StartHere`/None. (Repo `about` not yet folded in — see 4.)
4. **Repo `about` ingestion.** ⏳ Deferred. CLAUDE.md→README digest, cached.
5. **Router + multi-select modal.** ⏳ Deferred. Headless structured run →
   pre-checked multi-select. The last, highest-risk phase; 1–3 shipped
   first so the feature is useful even before inference lands.

## Open questions

- Workspace modeling for N-repo tickets (§3).
- `{handle}` source — config only, or derive from `git config user.name`?
- `{type}` — trust the label map, or let the router/agent set it?
- Router auto-accept threshold vs. always-confirm.
- Where a repo has no CLAUDE.md **and** no README — skip from `about`?
- ~~Non-obin fallback when no team mapping exists at all.~~ **Resolved
  (#1041):** the unmapped-team repo picker resolves and persists a mapping
  in-app on the first `w w`, so no hand-edit is needed.

## Non-goals (for now)

- Per-label / per-project repo mapping (obin labels can't support it).
- Live in-agent repo switching via MCP.
- Rewriting Linear's `gitBranchName` back to Linear.
