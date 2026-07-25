---
title: Use snippets
description: Put repeated, multi-sentence agent prompts a few keystrokes away — create, browse, and auto-submit snippets from any agent terminal.
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
repeating one is `]]s` then `Enter`.

## Browse what's available

Press `]` from the sidebar or activity pane (or **Browse snippets** in the `,`
settings palette) for a read-only catalog of every snippet — key, origin,
description, and full body. Press `e` there to open the YAML file. Unlike the
`]]s` picker, the browser needs no focused terminal.

## Write your own

Snippets are plain YAML you own — there's no in-app editor by design. Add an
entry under `snippets:` in `~/.lazybox/snippets.yaml`:

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
are optional (category adds a group header and colored tag). Files are read
**once at startup** — restart lazybox after editing.

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
