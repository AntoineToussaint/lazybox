# Snippets

Snippets are short keystroke shortcuts that expand into pre-defined
prompts and **auto-submit** to the active agent. The point is to put
repeated, multi-sentence prompts ("review the diff", "open a PR",
"run the pre-deploy checks") a few keystrokes away — without the
classic snippet-system flaw of "expand, then wait for me to press
Enter."

Snippets are **plain YAML files** you own and keep in version control
or your dotfiles. There is no in-app editor by design: creating,
editing, and deleting a snippet all mean editing a file. Discovery,
though, lives in the app: a read-only **snippets browser** (press `]`
from the sidebar / activity panes, or pick **Browse snippets** from the
`,` Settings palette) lists every snippet — key, origin, description,
and body — so you can see what's available without already knowing a
key. Press `e` in the browser to jump to the YAML file.
This page documents that full lifecycle — create, browse, list/use,
edit, delete — plus the file format and the picker reference.

lazybox ships a **broad, categorized built-in library** (~41 prompts)
so a fresh install has plenty to expand out of the box — review
(`rev`, `deepreview`, `nit`, plus the dedicated suite `audit` for a
full pre-ship pass, `arch` for a staff-engineer design review, and
`hotpath` for a performance review), git & PR (`pr`, `ready`, `commit`,
`rebase`, `sync`, `resume`, `push`, `squash`), testing (`test`, `tdd`, `repro`), debugging
(`bug`, `bisect`, `trace`), refactor (`refac`, `rename`, `extract`),
performance (`perf`, `bench`), security (`sec`, `deps`, `leaks`),
docs (`doc`, `readme`, `adr`), and chores (`lint`, `ci`, `clean`).
Anything you define with the same key transparently overrides the
built-in; you never have to start from an empty library.

## Quick start

1. Create `~/.lazybox/snippets.yaml` (or run **Settings → Edit snippets**,
   which seeds a starter file):

   ```yaml
   snippets:
     rev:
       description: Review current diff
       body: |
         Please review the current diff for correctness bugs and
         obvious cleanups. Focus on the changes only, not the
         surrounding code.
   ```

2. (Re)start lazybox — snippet files are read once at launch.
3. Open a session, focus its terminal, and type `]]rev`. The body is
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

The outer key (`pr`) is the shortcut you type after the `]]` leader.
The `description` is an optional one-line label shown in the picker;
`body` is the text sent to the agent.

Snippets are loaded once, at startup. After editing a file, **restart
lazybox** (or relaunch it) to pick up the change.

### Browse

The **snippets browser** is the answer to "what's even available?" —
a read-only modal listing every merged snippet with its key, origin
tag, description, and full body. Open it with `]` from the sidebar or
activity pane, or via **Settings → Browse snippets**; it's also in the
`?` help under the global shortcuts. `↑`/`↓` scroll, `e` opens the
global YAML file in your editor, and any other key closes it. Unlike
the terminal `]]` leader it needs no focused session and you don't
have to know a key first.

### List & use

Snippets live under the **`]]` leader** — the double-bracket chord that
opens a small command menu from a focused terminal (a which-key popup
lists it). The commands:

- **`]]s`** — opens the **snippet picker**, a real, dwellable modal: it
  stays open until you send or cancel — no timer, PTY output, or focus
  change closes it. Keep typing to filter, use `↑`/`↓` to move, and
  press `Enter` to send the highlighted row. `Esc` (or `Ctrl-C`)
  cancels without sending.
- **`]]srev`** — the fast path: when what you type in the picker exactly
  equals a snippet key (here `rev`) and it's the only snippet whose key
  starts with that text, the body is **sent and submitted immediately,
  no `Enter`** — and no second look. If you author a snippet whose body
  drives something irreversible, keep that in mind: the exact-key path
  skips the preview dwell (it only fires on a unique, unambiguous key).
- **Recent first.** Snippets you've sent this session float into a
  **Recent** group at the top of the picker, most-recent first, with the
  cursor on the last one — so repeating a snippet is `]]s` then `Enter`.
  Start typing and the Recent group steps aside (a filter means "find",
  not "repeat"). The list is session-scoped.
- **`]]q`** — exits the terminal pane back to the sidebar. **`]]f`**
  toggles focus mode, **`]]<digit>`** jumps to the Nth agent workspace,
  and **`` ]]` ``** opens the fuzzy workspace switcher.
- The leader is *non-timed* (#252): after `]]` it waits for the command
  key rather than leaving on an idle timer, so pausing to read the popup
  never drops you to the sidebar mid-decision. `Esc` or any unbound key
  cancels back to the terminal.
- **A lone `]`** — a single `]` followed by any non-`]` key is sent to
  the agent verbatim, so `]` is typeable in code, arrays, and
  markdown. Only the doubled `]]` is intercepted. To type a literal
  `]]` into an agent (nested arrays, some markdown), remap the prefix
  with `ui.terminal_escape_char` to a character you don't type, e.g. `}`.

The picker is a small overlay; the terminal stays focused underneath.
Rows are **grouped under category headers** (Review, Git & PR,
Testing, …), each row showing a category-colored tag, the snippet key,
and its description; the header line shows the visible/total count. A
**live preview pane** on the right renders the highlighted snippet's
full (wrapped) body, its category, and its `origin`
(`built-in` / `global` / `repo` — see
[precedence](#file-locations--precedence)) so you see exactly what
will be sent before it auto-submits. The list **scrolls** to keep the
cursor in view as you move.

Filtering is case-insensitive and matches the snippet **key**, its
**description**, and its **category** — so you can find a snippet by
what it *does*, not only by a key you already know. The `]]rev`
exact-key fast path is preserved: when what you type after the leader
is the *only* snippet key that starts with it and equals it exactly,
the body auto-submits immediately, regardless of any description-only
matches.

### Edit

Edit the entry in the YAML file and restart lazybox. Because repo-local
snippets override global ones on key conflict, you can also "edit" a
shared global snippet for one project by redefining the same key in
that repo's `.lazybox/snippets.yaml`.

### Delete

Remove the entry from the YAML file (or delete the file to clear all
of its snippets) and restart lazybox. Deleting a repo-local entry that
shadowed a global one re-exposes the global snippet under that key.

## File locations & precedence

Two files contribute, layered on top of the built-in set. Both files
are optional; a missing file simply contributes nothing.

| Scope          | Path                          | Use it for                                        |
| -------------- | ----------------------------- | ------------------------------------------------- |
| **Built-in**   | _(shipped with lazybox)_        | A broad, categorized starter library (~41 prompts). |
| **Global**     | `~/.lazybox/snippets.yaml`      | Your personal library, shared across all repos.   |
| **Repo-local** | `<repo>/.lazybox/snippets.yaml` | Project-specific prompts, checked into the repo.   |

The repo-local file is resolved relative to the directory lazybox was
launched from. The sets are **merged** with precedence, lowest to
highest, **built-in → global → repo** — so a key conflict resolves to
the most specific definition. A project can override a shared shortcut
(e.g. a project-specific `rev`) without touching your personal
library, and either file can override a built-in.

## House style for bodies

The point of a snippet is that `]]rev` should produce a *noticeably
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
| `<key>`       | yes      | The shortcut typed after the `]]` leader. Case-sensitive.      |
| `description` | no       | One-line label shown in the picker. Defaults to empty.         |
| `category`    | no       | Group header + colored tag in the picker (e.g. `Review`, `Git & PR`). Free-form; defaults to empty, which files under a trailing **Other** group. |
| `body`        | yes      | Sent verbatim to the agent. May span multiple lines.           |

## Behaviour & gotchas

- **Submission.** The body is sent **verbatim** to the active terminal
  and submitted in one action — no extra keystroke. How the submit is
  delivered depends on the terminal:
  - **Agent terminals** (Claude Code, Codex, Cursor) go through the same
    settle-gated inject path `w` uses: the body is pasted, then Enter is
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
- **The built-in library is always present**, so the `]]` leader
  always has a broad, categorized set to offer even before you write
  your own. (If you somehow have no snippets at all, `]]` simply
  leaves the pane immediately, with no idle wait.)
- **Reload.** Files are read once at startup. Restart lazybox after
  creating, editing, or deleting a snippet.
- **Malformed YAML.** A file that fails to parse is skipped with a
  warning in the log (`/tmp/lazybox.log`) rather than crashing lazybox —
  if a snippet "disappears," check the log for a parse error.
