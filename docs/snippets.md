# Snippets

Short keystroke shortcuts that expand into pre-defined prompts and
**auto-submit** to the active agent. The whole point is to make
repeated, multi-sentence prompts a few keystrokes away — without
the classic snippet-system flaw of "expand but wait for me to
press Enter."

## Triggering

Inside an agent session, press `]` followed by a snippet key:

- `]rev` → expands the `rev` snippet body and immediately submits.
- `]<any-other-printable>` → opens the snippet picker with that
  char as the initial filter. Keep typing to narrow; press Enter
  to submit the selected row; press Esc to cancel.
- `]]` (still works) → exits the terminal pane back to the
  sidebar.

The terminal stays in focus throughout; the picker is a small
overlay that closes as soon as you submit or cancel.

## Configuration

Two files contribute, merged with the repo-local one winning on
key conflict.

### Global — `~/.pilot/snippets.yaml`

```yaml
snippets:
  rev:
    description: Review current diff
    body: |
      Please review the current diff for correctness bugs and
      obvious cleanups. Focus on the changes only, not the
      surrounding code.
  pr:
    description: Open a PR with summary + test plan
    body: |
      Please open a PR for the current branch. Use a concise
      title. Body should include a Summary section (1-3 bullets)
      and a Test plan section as a checklist.
```

### Repo-local — `<repo>/.pilot/snippets.yaml`

Checked into the repository so a project can ship its own review
prompts, deploy checks, etc. Same shape; the merge favors repo
entries on key collision.

```yaml
snippets:
  deploy:
    description: Pre-deploy checklist
    body: |
      Run the pre-deploy checks: confirm migrations are reversible,
      sample a few PROD queries to validate the new index, and
      verify the feature flag is OFF for the rollout phase.
  rev:
    description: Repo-specific review prompt
    body: |
      Review the diff focusing on changes to the auth layer.
      Flag anything that touches session token handling.
```

In this example, `]rev` will use the repo's snippet (it overrides
the global one), while `]pr` still resolves to the global entry.

## Fields

| Field         | Required | Description                                                     |
| ------------- | -------- | --------------------------------------------------------------- |
| `description` | no       | One-line label shown in the picker. Default: empty.             |
| `body`        | yes      | The text sent to the agent. Multi-line allowed.                 |

## Behaviour

- The snippet body is sent **verbatim** to the active terminal's
  PTY, followed by a single `\r` (carriage return) which submits
  on every agent pilot ships (Claude Code, Codex, Cursor, shell).
- No active terminal → the picker still opens, but Enter shows a
  hint instead of sending. Spawn a session first.
- Missing files (neither global nor repo) → the picker refuses to
  mount and surfaces a hint pointing at `~/.pilot/snippets.yaml`.
