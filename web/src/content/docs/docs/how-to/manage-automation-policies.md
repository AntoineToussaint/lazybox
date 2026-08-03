---
title: Manage automation policies
description: Control merge-on-green and per-workspace auto-fix behavior from one visible menu.
---

Goal: see and change every automation policy attached to a GitHub PR or issue
without hunting through unrelated shortcuts or hidden labels.

Select the workspace and press `g p`. The **Automation policies** menu shows
the effective state of:

- **merge on green** — lazybox's client-side arm (toggled with `g g`, shown on
  the row as the green ` ARM ` pill), active **only while lazybox is running**;
- **GitHub auto-merge** — GitHub's server-side state (the accent ` AUTO ` row
  pill), shown read-only because it is managed on github.com and **merges even
  when lazybox is closed**;
- **auto-fix CI** — whether lazybox may launch an agent for failing CI;
- **auto-fix conflict** — whether lazybox may launch an agent for a merge
  conflict.

`●` means active and `○` means off. Move with `j` / `k` or the arrow keys and
press `Enter` to toggle a lazybox-owned policy. On an issue-only workspace the
PR-only rows remain visible but unavailable, so the absence of automation is
explicit rather than silent.

## `ARM` vs `AUTO`: two ways to merge on green

Both pills say "this PR merges itself once CI is green," but they are different
mechanisms with a different guarantee — the one thing worth knowing is *does it
still merge if I close lazybox?*

|                          | ` ARM ` — merge on green (`g g`) | ` AUTO ` — GitHub auto-merge |
| ------------------------ | -------------------------------- | ---------------------------- |
| Who merges               | lazybox, client-side             | GitHub, server-side          |
| Survives closing lazybox | **No** — nothing merges          | **Yes**                      |
| Gate                     | your own PR, no conflicts, no changes-requested (lazybox's rules) | GitHub branch-protection required checks |
| Toggle                   | `g g` in lazybox                 | GitHub UI (lazybox shows it read-only) |

Use ` ARM ` for the common case: you are at the keyboard, want your own green PR
to land without babysitting it, and lazybox is running anyway. Use GitHub's
` AUTO ` when the merge must happen regardless of whether lazybox is open — you
are stepping away, or the PR is waiting on a slow required check.

## How auto-fix overrides resolve

Auto-fix remains globally opt-in through the [`auto_fix` configuration
block](/docs/reference/configuration/#auto_fix). The policy menu then controls
the selected workspace:

- **follows config** uses the global switch and respects `opt_out_labels`;
- **armed here** enables that failure kind for the workspace and overrides an
  opt-out label, but it does not bypass a globally disabled `auto_fix` feature;
- **disarmed here** always blocks that failure kind for the workspace.

The CI-failure and merge-conflict arms are independent. Attempt budgets,
cooldown, author-only scope, draft/review guards, and the other global safety
checks still apply.

## Merge precedence

When GitHub-native auto-merge is already enabled, lazybox's client-side
merge-on-green stands down. GitHub owns the server-side merge, avoiding two
actors racing to land the same PR.

The workspace stores lazybox policy choices, so they survive restart. Use
`g p` again at any time to inspect the effective state instead of inferring it
from behavior.

## Related

- [Configuration reference: `auto_fix`](/docs/reference/configuration/#auto_fix)
- [Keybindings reference](/docs/reference/keybindings/)
- [Run an agent per workspace](/docs/how-to/run-an-agent-per-workspace/)
