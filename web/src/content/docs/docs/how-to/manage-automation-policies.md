---
title: Manage automation policies
description: Control merge-on-green and per-workspace auto-fix behavior from one visible menu.
---

Goal: see and change every automation policy attached to a GitHub PR or issue
without hunting through unrelated shortcuts or hidden labels.

Select the workspace and press `g p`. The **Automation policies** menu shows
the effective state of:

- **merge on green** — lazybox's client-side arm, active only while lazybox is
  running;
- **GitHub auto-merge** — GitHub's server-side state, shown read-only because
  it is managed on github.com;
- **auto-fix CI** — whether lazybox may launch an agent for failing CI;
- **auto-fix conflict** — whether lazybox may launch an agent for a merge
  conflict.

`●` means active and `○` means off. Move with `j` / `k` or the arrow keys and
press `Enter` to toggle a lazybox-owned policy. On an issue-only workspace the
PR-only rows remain visible but unavailable, so the absence of automation is
explicit rather than silent.

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
