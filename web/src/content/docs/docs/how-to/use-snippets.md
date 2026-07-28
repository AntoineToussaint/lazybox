---
title: Use snippets
description: Reuse agent prompts, repeat recent instructions, track per-workspace progress, and create snippets safely through Ask Lazybox.
---

Snippets are short keys that expand into a pre-written prompt and **auto-submit**
it to the focused agent. They turn a paragraph of carefully-tuned instruction
("review the diff", "open a PR with a summary and test plan") into a chord like
`]]srev`. lazybox ships a broad categorized built-in library (~41 prompts), so
the picker is useful before you write a single one of your own.

## Send a built-in snippet

From a focused agent terminal, type `]]s` to open the snippet picker, then start
typing a key. As soon as what you type **uniquely matches** a key, the body is
sent and submitted immediately — no `Enter`. The whole chord reads `]]srev`.

In the picker: keep typing to filter (it matches key, description, and
category), `↑`/`↓` to move, `Enter` to send the highlighted row, `Esc` to
cancel. Snippets you've sent float into a **Recent** group at the top, so
repeating one is `]]s` then `Enter`. Recent order persists in the state
database across lazybox restarts.

## Read workspace progress at a glance

Each workspace remembers the distinct snippet keys sent to it. Its sidebar row
shows a dim `]N` badge, where `N` is the number of distinct snippets applied:

1. Send `]]srev`; the row shows `]1`.
2. Send `]]stest`; it becomes `]2`.
3. Send `rev` again; it stays `]2` because the badge tracks workflows started,
   not total invocations.

This makes it easy to return later and see that review or testing has already
begun. The snippet history moves with the workspace when sessions are adopted
or an issue session continues in its PR.

## Browse what's available

Press `]` from the sidebar or activity pane (or **Browse snippets** in the `,`
settings palette) for a read-only catalog of every snippet — key, origin,
description, and full body. Press `e` there to open the YAML file. Unlike the
`]]s` picker, the browser needs no focused terminal.

## Write your own in YAML

Snippets remain plain YAML you own. For the manual path, add an entry under
`snippets:` in `~/.lazybox/snippets.yaml`:

```yaml
snippets:
  pr:
    description: Open a PR with summary + test plan
    category: Git & PR
    body: |
      Open a PR for the current branch with a concise title. The body must
      have a Summary section (1-3 bullets) and a Test plan checklist.
```

The outer key (`pr`) is what you type after `]]s`. `description` and `category`
are optional (category adds a group header and colored tag). Manual file edits
are read at startup, so restart lazybox after changing YAML by hand.

## Create or improve a snippet with Ask Lazybox

Ask Lazybox closes the loop from product guidance to a reusable instruction:
Enable Claude Code or Codex for the conversational assistant and action
proposals. The same `?` surface still searches your effective keybindings when
neither structured agent is enabled.

1. Press `?` and ask how to run the workflow you have in mind.
2. Ask it to create a new snippet or improve an existing key.
3. Inspect the proposed key, category, description, and full body in the
   confirm-with-preview.
4. Confirm. lazybox validates and writes the global YAML entry itself, then
   hot-reloads the merged catalog.
5. Use the new key immediately through `]]s<key>`—no restart required.

The safety boundary is intentionally small. The assistant can only propose
allowlisted structured actions and config keys; lazybox validates values
against live state, shows the preview, and applies nothing until you confirm.
lazybox—not the agent—owns the filesystem write.

After you send the new snippet it joins **Recent**, persists across restarts,
and increments that workspace's `]N` badge if the key is new there. This turns
one conversation into a repeatable workflow with visible progress.

## Layer global and per-repo libraries

Two files stack on top of the built-in set, merged lowest-to-highest —
**built-in → global → repo** — so the most specific definition of a key wins:

| Scope | Path |
| --- | --- |
| Global | `~/.lazybox/snippets.yaml` |
| Repo-local | `<repo>/.lazybox/snippets.yaml` |

That lets a project redefine a shared key (say a project-specific `rev`) without
touching your personal library, and either file can override a built-in.

## See also

The [full snippets reference](https://github.com/AntoineToussaint/lazybox/blob/main/docs/snippets.md)
covers the complete lifecycle (create / browse / edit / delete), the file
format, house style for writing effective bodies, and submission behavior for
agents versus shells. Broadcasting a snippet to many workspaces at once is
covered in [Orchestrate multiple agents](/docs/how-to/orchestrate-multiple-agents/).
