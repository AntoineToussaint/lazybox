# Snippets: reusable workflows with memory

Turn the instructions you use every day — review this change, fix CI,
integrate feedback, run the final checks — into one fast action.
lazybox remembers what you used recently and which workflows each
workspace has already received.

A snippet is a short key backed by a complete agent instruction. It is
sent and **submitted** to the focused agent in one action, not merely
pasted for you to finish. lazybox ships 41 categorized workflows, so
the system is useful before you create anything: review (`rev`,
`deepreview`, `audit`), Git and PR (`pr`, `rebase`, `push`), testing
(`test`, `tdd`, `repro`), debugging (`bug`, `bisect`, `trace`),
security (`sec`, `deps`, `leaks`), and more.

Snippets are the *human-triggered* half of a pair: agents lazybox
spawns also load their own model-triggered **Skills**
(`SKILL.md`). For which one to reach for, see
[Snippets vs Agent Skills](snippets-vs-skills.md).

## Send and repeat a workflow

1. Focus an agent terminal and type `]]s`. The categorized picker opens
   with a live preview of the selected workflow.
2. Type `rev`. Because that key is a unique match, lazybox immediately
   sends and submits the built-in review workflow. The full fast path is
   `]]srev`; it needs no extra `Enter`.
3. Return later and open `]]s` again. The workflow you just used is
   selected in the **Recent** group, so repeating it is `]]s` then
   `Enter`.

The picker stays open until you send or cancel; there is no timer.
`↑`/`↓` moves the selection, `Enter` sends the highlighted workflow,
and `Esc` or `Ctrl-C` cancels. Typing filters case-insensitively across
the key, description, and category. An exact key auto-submits only when
it is the sole key with that prefix, so ambiguous shortcuts remain in
the picker for you to resolve.

The `]]` terminal leader also holds `]]q` (return to the sidebar),
`]]f` (focus mode), `]]<digit>` (jump to an agent workspace), and
`` ]]` `` (workspace switcher). It is non-timed: after `]]`, lazybox
waits for a command instead of racing your next key. A lone `]` followed
by any non-`]` key is sent to the terminal verbatim. You can change the
prefix with `terminal.escape_char`.

## Discover and preview workflows

The picker is organized for a catalog larger than a handful of keys:

- Category headers group Review, Git & PR, Testing, Debugging, and other
  workflows.
- Live filtering matches the key, description, and category.
- The right pane previews the full wrapped body, category, and origin
  (`built-in`, `global`, or `repo`) before you send it. `repo` means the
  launch-directory layer described below; it does not follow workspace
  selection.
- A visible/total count and scrolling list keep filtered results
  legible.

To browse without a focused terminal, press `]` from the sidebar or
activity pane, or choose **Browse snippets** from the `,` Settings
palette. This read-only catalog shows every merged workflow with its
key, origin, description, and full body. `↑`/`↓` scrolls and `e` opens
the global YAML file in your editor.

## Understand Recent and the `]N` workspace badge

lazybox keeps two complementary memories:

- **Recent is your global fast lane.** The five most recently sent
  snippet keys float to the top of every picker, newest first, with the
  latest selected. The MRU is de-duplicated and persisted in
  `~/.lazybox/v2/state.db`, so `]]s` then `Enter` still repeats the last
  workflow after a restart. Start typing to leave Recent and search the
  full catalog.
- **`]N` is per-workspace progress.** Each workspace persists an MRU of
  the 12 most recently distinct snippet keys delivered to its agents.
  The sidebar badge is that bounded count: `]2` means two different
  workflows are in recent history there, while `]12` means the history
  is full and older keys have fallen out. Re-sending one moves it to the
  front without increasing the count. This is a quick cue that review,
  testing, or another repeatable process has recently started.

Only successfully delivered snippets enter either history; opening or
cancelling the picker records nothing.

## Create or improve a workflow with Ask Lazybox

Press `?` and ask in plain language:

> Add a snippet called `feedback` that integrates review feedback,
> runs the relevant tests, and commits the result.

Ask Lazybox proposes a global snippet as a **confirm-with-preview**,
including its key, category, description, body, and destination file.
If the key already exists, the preview says it will replace the
workflow and defaults to the safer decline choice. Accepting makes
lazybox validate and write `~/.lazybox/snippets.yaml`, then hot-reload
the merged catalog. The new or improved workflow is available through
`]]s<key>` immediately, with no restart. Declining writes nothing.

This is an allowlisted action: lazybox owns and validates the write.
Ask Lazybox creates and updates the **global** layer; edit the
launch-directory file directly when that client should use an override. See
[TUI & UX → Ask Lazybox](features/tui-and-ux.md#ask-lazybox--shortcut-index).

## Create a workflow in YAML

Add an entry under `snippets:` in the global or launch-directory file:

```yaml
snippets:
  feedback:
    description: Integrate review feedback and verify it
    category: Review
    body: |
      Read the unresolved review comments, implement each requested
      change that is still applicable, run the relevant tests, and
      commit the result. Report any comment you did not address and why.
```

The outer key (`feedback`) is what you type after `]]s`.
`description` and `category` make the workflow easier to discover;
`body` is the complete instruction sent to the agent. Hand-edited files
are loaded at startup, so restart lazybox after changing them.

## Choose the right scope

The three layers merge from least to most specific:

| Scope          | Path                          | Use it for                                      |
| -------------- | ----------------------------- | ----------------------------------------------- |
| **Built-in**   | _(shipped with lazybox)_       | 41 categorized daily engineering workflows.    |
| **Global**     | `~/.lazybox/snippets.yaml`     | Personal habits reused across every repository. |
| **Launch directory** | `<launch-dir>/.lazybox/snippets.yaml` | Overrides for this lazybox client catalog. |

Precedence is **built-in → global → launch directory**, so the last
definition of a key wins. Launching lazybox from a project can redefine
`test` with that checkout's command or tighten `rev` around its conventions.
Origin labels in the picker show `repo` for this winning directory layer.

The directory layer is resolved once, relative to where lazybox was launched,
and the merged catalog is shared by every workspace in that client. Selecting
another workspace does not load that workspace's `.lazybox/snippets.yaml`;
restart lazybox from a different directory to select a different directory
layer. Both user-owned files are optional and work well in dotfiles or version
control.

## Broadcast one workflow to several workspaces

Use snippets to keep a fleet of agents on the same process:

1. In the sidebar, press `v` on each target workspace.
2. Press `Shift-B` and choose a snippet. `Ctrl-F` skips directly to
   free text when you do not want one.
3. Review or extend the pre-filled body in the compose textarea, then
   submit.

Running agents receive the instruction through the normal agent inject
path; plain shells receive a direct write. Workspaces without a session
are skipped and named in the summary. A snippet-seeded broadcast records
the snippet once in Recent and on every workspace that actually received
it, so the `]N` badges reflect the rollout.

The
[multi-agent orchestration guide](https://lazybox.ai/docs/how-to/orchestrate-multiple-agents/)
covers free-text mode, mixed agent/shell delivery, skips, retries, and
per-workspace history.

## Update or remove a workflow

Ask Lazybox can replace a global key with a confirmed, hot-reloaded
version. For direct edits, change the YAML entry and restart lazybox.
Redefining a global key in the launch-directory file updates that workflow for
the whole client started from that directory.

To remove a workflow, delete its YAML entry and restart. Removing a
launch-directory override reveals the global or built-in definition beneath
it; removing a global override does the same for the built-in.

## House style for bodies

The point of a snippet is that `]]srev` should produce a *noticeably
better* agent run than typing "please review the diff." A body is a
carefully-tuned instruction, not a label. The shipped built-in library
follows one deliberate style, and your own snippets will be sharper if
they do too:

- **Imperative, addressed to the agent.** "Review the current diff…",
  "Reproduce the bug…" — not "Can you please…". One voice across the
  whole set.
- **State the deliverable up front, and make it checkable.** Ask for
  ranked findings with `file:line` anchors, a failing test, a PR URL —
  something concrete you can verify — rather than free prose.
- **Encode the discipline, not just the task.** Root-cause before
  fixing, no symptom-masking, no behavior change on a refactor, prove
  it with tests. The prompt is where you bake in how a strong engineer
  would actually approach the work.
- **Lean on the worktree.** Snippets run inside a git worktree with
  `git`, `gh`, and the project's checks available. Tell the agent to
  run the tests, pull the CI logs (`gh run view --log-failed`), open
  the PR — don't describe the work abstractly.
- **Give an escape hatch.** "If the diff is clean, say so" beats an
  agent that invents nits to look busy.
- **Keep it to a tight paragraph.** A few sentences of dense
  instruction. Long enough to be specific, short enough to read at a
  glance in the preview pane.

A body that follows the style, for reference — the built-in `rev`:

```yaml
snippets:
  rev:
    description: Review the current diff
    category: Review
    body: |
      Review the current diff (`git diff` against the base branch) for
      correctness bugs: logic errors, off-by-one mistakes, missing error
      handling, broken edge cases, and anything that wouldn't survive a
      careful review. Report findings as a list ranked by severity, each
      with a `file:line` anchor and a one-line explanation of what breaks
      and when. Look only at the changed lines and the code they directly
      touch, not the whole file. If the diff is clean, say so plainly
      rather than inventing nits.
```

> **Not yet supported:** placeholder / variable interpolation in bodies
> (e.g. injecting the selected file or a typed argument). Bodies are
> sent verbatim today; parameterized snippets are a possible future
> feature, tracked separately.

## Snippet format

```yaml
snippets:
  <key>:
    description: <optional one-line label>
    category: <optional grouping label>
    body: |
      <text sent to the agent>
```

| Field         | Required | Notes                                                          |
| ------------- | -------- | -------------------------------------------------------------- |
| `<key>`       | yes      | The shortcut typed in the picker after `]]s`. Matching (filter + auto-submit) is case-insensitive. |
| `description` | no       | One-line label shown in the picker. Defaults to empty.         |
| `category`    | no       | Group header + colored tag in the picker (e.g. `Review`, `Git & PR`). Free-form; defaults to empty, which files under a trailing **Other** group. |
| `body`        | yes      | Sent verbatim to the agent. May span multiple lines.           |

## Behaviour & gotchas

- **Submission.** The body is sent **verbatim** to the active terminal
  and submitted in one action — no extra keystroke. How the submit is
  delivered depends on the terminal:
  - **Agent terminals** (Claude Code, Codex, Cursor) go through the same
    settle-gated inject path `w w` uses: the body is pasted, then Enter is
    sent as a **separate** keystroke once the paste's repaint quiesces.
    A single write with a trailing `\r` is unreliable here — an agent
    that debounces the pasted burst can swallow the `\r` as a soft
    newline, so the prompt expands but never submits. Splitting the
    submit off makes it land cleanly for every shipped agent.
  - **Plain shells** get the body plus a trailing carriage return (`\r`)
    written directly; a shell has no paste debounce, so that submits
    immediately. Multi-line bodies are wrapped in a bracketed paste so
    the submit `\r` lands outside the paste, never buffered as a literal
    newline.
- **No active terminal.** If you trigger a snippet with no session
  terminal focused, nothing is sent and lazybox flashes a hint to open a
  session first.
- **Typing a literal `]`.** A single `]` is only the start of the
  leader if a second `]` immediately follows. `]` + any other key is
  sent to the agent verbatim, so brackets in code and markdown reach
  the agent unharmed. The doubled `]]` is the only intercepted form.
- **The built-in library is always present**, so the `]]s` picker
  always has a broad, categorized set to offer even before you write
  your own. (If you somehow have no snippets at all, `]]s` flashes
  "no snippets configured" and points you at
  `~/.lazybox/snippets.yaml`.)
- **Reload.** Files are read once at startup. Restart lazybox after
  creating, editing, or deleting a snippet.
- **Malformed YAML.** A file that fails to parse is skipped with a
  warning in the log (`/tmp/lazybox.log`) rather than crashing lazybox —
  if a snippet "disappears," check the log for a parse error.
