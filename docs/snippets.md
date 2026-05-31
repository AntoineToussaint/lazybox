# Snippets

Snippets are short keystroke shortcuts that expand into pre-defined
prompts and **auto-submit** to the active agent. The point is to put
repeated, multi-sentence prompts ("review the diff", "open a PR",
"run the pre-deploy checks") a few keystrokes away — without the
classic snippet-system flaw of "expand, then wait for me to press
Enter."

Snippets are **plain YAML files** you own and keep in version control
or your dotfiles. There is no in-app editor by design: creating,
editing, and deleting a snippet all mean editing a file. This page
documents that full lifecycle — create, list/use, edit, delete — plus
the file format and the picker reference.

## Quick start

1. Create `~/.pilot/snippets.yaml`:

   ```yaml
   snippets:
     rev:
       description: Review current diff
       body: |
         Please review the current diff for correctness bugs and
         obvious cleanups. Focus on the changes only, not the
         surrounding code.
   ```

2. (Re)start pilot — snippet files are read once at launch.
3. Open a session, focus its terminal, and type `]rev`. The body is
   sent to the agent and submitted immediately.

## The lifecycle

### Create

Add an entry under the `snippets:` map in either file (see
[File locations](#file-locations--precedence)):

```yaml
snippets:
  pr:
    description: Open a PR with summary + test plan
    body: |
      Please open a PR for the current branch. Use a concise title.
      The body should include a Summary section (1-3 bullets) and a
      Test plan section as a checklist.
```

The outer key (`pr`) is the shortcut you type after `]`. The
`description` is an optional one-line label shown in the picker;
`body` is the text sent to the agent.

Snippets are loaded once, at startup. After editing a file, **restart
pilot** (or relaunch it) to pick up the change.

### List & use

Snippets are used from inside an agent session's terminal. With the
terminal focused, press `]` followed by the snippet key:

- **`]rev`** — when what you type exactly equals a snippet key (here
  `rev`) and it's the only snippet whose key starts with that text,
  the body is sent and submitted immediately. This is the fast path
  for snippets you know by name.
- **`]<text>`** — otherwise (the text is a partial key, or more than
  one snippet key starts with it) the **snippet picker** opens,
  pre-filtered by what you typed. Keep typing to narrow, use `↑`/`↓`
  to move, and press `Enter` to send the highlighted row. `Esc` (or
  `Ctrl-C`) cancels without sending.
- **`]]`** — the double-bracket escape still exits the terminal pane
  back to the sidebar; it does not open the picker.

The picker is a small overlay; the terminal stays focused underneath.
Each row shows the snippet key, its description, an `origin` tag
(`global` or `repo` — see [precedence](#file-locations--precedence)),
and a preview of the body. Filtering matches snippet **keys** (the
text after `]`), case-insensitively.

### Edit

Edit the entry in the YAML file and restart pilot. Because repo-local
snippets override global ones on key conflict, you can also "edit" a
shared global snippet for one project by redefining the same key in
that repo's `.pilot/snippets.yaml`.

### Delete

Remove the entry from the YAML file (or delete the file to clear all
of its snippets) and restart pilot. Deleting a repo-local entry that
shadowed a global one re-exposes the global snippet under that key.

## File locations & precedence

Two files contribute. Both are optional; a missing file simply
contributes nothing.

| Scope          | Path                          | Use it for                                        |
| -------------- | ----------------------------- | ------------------------------------------------- |
| **Global**     | `~/.pilot/snippets.yaml`      | Your personal library, shared across all repos.   |
| **Repo-local** | `<repo>/.pilot/snippets.yaml` | Project-specific prompts, checked into the repo.   |

The repo-local file is resolved relative to the directory pilot was
launched from. The two sets are **merged**, and on a key conflict the
**repo-local entry wins** — so a project can override a shared
shortcut (e.g. a project-specific `rev`) without touching your
personal library.

## Snippet format

```yaml
snippets:
  <key>:
    description: <optional one-line label>
    body: |
      <text sent to the agent>
```

| Field         | Required | Notes                                                          |
| ------------- | -------- | -------------------------------------------------------------- |
| `<key>`       | yes      | The shortcut typed after `]`. Case-sensitive.                  |
| `description` | no       | One-line label shown in the picker. Defaults to empty.         |
| `body`        | yes      | Sent verbatim to the agent. May span multiple lines.           |

## Behaviour & gotchas

- **Submission.** The body is sent **verbatim** to the active
  terminal's PTY, followed by a single carriage return (`\r`) that
  submits it. This works for every agent pilot ships (Claude Code,
  Codex, Cursor) and for a plain shell. A multi-line `body` is sent as
  one block; only the trailing `\r` submits, so embedded newlines stay
  part of the prompt.
- **No active terminal.** If you trigger a snippet with no session
  terminal focused, nothing is sent and pilot flashes a hint to open a
  session first.
- **No snippets configured.** If neither file defines any snippets,
  the picker does not open and pilot flashes a hint that no snippets
  are configured yet.
- **Reload.** Files are read once at startup. Restart pilot after
  creating, editing, or deleting a snippet.
- **Malformed YAML.** A file that fails to parse is skipped with a
  warning in the log (`/tmp/pilot.log`) rather than crashing pilot —
  if a snippet "disappears," check the log for a parse error.
