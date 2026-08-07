# Linear "work on" → repo routing, branch/PR conventions

Status: **draft spec** (pre-implementation). Owner: TBD.

## Problem

Working on a **Linear** issue (`w`) is currently broken and convention-less:

1. **No repo mapping.** A Linear task's `repo` is the synthetic string
   `linear/<team>` (e.g. `linear/OBI`) — not a real GitHub repo. So
   `provision_worktree` would try to clone `git@github.com:linear/OBI.git`
   (`spawn_handler.rs`, `git-ops/src/lib.rs`). A Linear ticket has no idea
   which GitHub repo the work belongs in.
2. **No naming convention.** For a branchless task, lazybox derives a
   generic `linear-<identifier>-<slug>` (`derive_branch_for_branchless`,
   `spawn_handler.rs:2646`). It ignores Linear's own `gitBranchName` and
   does not match the obin house convention (below).
3. **No prompt.** A Linear-only workspace classifies as `StartHere`
   (`tui-core/src/intent.rs:291`), so `w` spawns a **bare agent with no
   prompt** — none of the GitHub `Closes #N` / PR-title guidance is
   injected for Linear.

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

```yaml
providers:
  linear:
    handle: antoine                     # {handle} for branch names
    type_from_label:                    # {type} mapping, optional
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
  └─ 5. WORK AGENT prompt (today: nothing) per worktree:
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

1. **Config + repo resolution.** `LinearConfig` (handle, teams→repos,
   template, label map). Replace the `linear/<team>` repo with the mapped
   GitHub repo in `repo_for_workspace_provision`. Single-repo, default
   repo, no router yet. *Dogfoodable: `w` a Linear ticket → correct repo,
   correct branch name.*
2. **Naming.** Branch-template engine (tokens) + label→type. Regression
   tests against the obin examples in this doc.
3. **Prompt injection.** Give Linear `w` a real prompt (ticket + repo
   `about` + convention). Route Linear through a Linear-specific prompt
   builder instead of `StartHere`/None.
4. **Repo `about` ingestion.** CLAUDE.md→README digest, cached.
5. **Router + multi-select modal.** Headless structured run → pre-checked
   multi-select. This is the last, highest-risk phase; ship 1–4 first so
   the feature is useful even before inference lands.

## Open questions

- Workspace modeling for N-repo tickets (§3).
- `{handle}` source — config only, or derive from `git config user.name`?
- `{type}` — trust the label map, or let the router/agent set it?
- Router auto-accept threshold vs. always-confirm.
- Where a repo has no CLAUDE.md **and** no README — skip from `about`?
- Non-obin fallback when no team mapping exists at all.

## Non-goals (for now)

- Per-label / per-project repo mapping (obin labels can't support it).
- Live in-agent repo switching via MCP.
- Rewriting Linear's `gitBranchName` back to Linear.
