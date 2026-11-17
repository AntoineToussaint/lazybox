# Start from an issue

Most work starts from an issue. Give Lazybox the issue and it makes the
workspace in the issue's repository, on a branch named after it, and starts the
agent with the issue's text and its recent comments. The workspace remembers
where it came from, and the agent is told how to reference it so GitHub links
the pull request back.

- [Paste the link](#paste-the-link)
- [Pick an issue](#pick-an-issue)
- [What the agent gets](#what-the-agent-gets)
- [Already started?](#already-started)
- [Where the work came from](#where-the-work-came-from)
- [From the terminal](#from-the-terminal)
- [What it needs](#what-it-needs)

## Paste the link

Open **New task** (`⌘N`) and paste an issue's link into the message box:
`https://github.com/acme/api/issues/812`, or just `acme/api#812`. It becomes a
chip instead of text:

- the issue's key, title, state and provider;
- the issue's repository, chosen for you;
- the workspace named after the issue's title, on the branch
  `812-fix-refund-rounding` (**Edit** to change either).

Type anything you want to add in the message box (optional), and press
**Enter**. A pull request's link works too. Text that only *mentions* a link
("see https://github.com/… for details") stays text.

If the issue's repository is not on this Runner yet, the sheet says so, with
**Find it on GitHub** (refreshes the list of repositories your GitHub
connection can see; it is cloned when the task starts) and **Add or clone
it…** (Settings → Repositories).

## Pick an issue

**From an issue…** under the message box, or **Start from an issue…** in the
command palette (`⌘K`), opens a picker on the issues **assigned to you** that
are still open, most recently updated first. Each row shows the repository,
number, title, labels and when it last changed.

| Key | What it does |
| --- | --- |
| type | Search as you type. A link or `owner/name#N` finds that one issue. |
| `↑` `↓` | Choose a row. |
| `Enter` | Start from it. |
| `Tab` / `⇧Tab` | Widen the search: **Involving you** (you opened, are assigned or commented), then **All of GitHub** (any open issue you can read; type something first). |
| `Esc` | Close the picker; the sheet stays. |

## What the agent gets

The issue goes to the agent attached to your first message, not pasted into
it. Before you start, **What the agent gets** shows exactly what will be sent:
the title, state, author, labels, the description, and the last ten comments
with their authors and dates. Untick **Include the comments** to send only the
issue itself.

Issue text is written by other people, so Lazybox treats it as data, not as
instructions:

- it is sent as quoted external content, every line quoted, labelled with the
  issue's link, with a note telling the agent that only your own words say what
  to do;
- the agent starts in **Ask** even when the Runner's default is YOLO, unless
  you pick YOLO in the sheet yourself;
- in the conversation, your first message shows the issue as a chip; open it to
  read exactly what the agent received.

Lazybox never changes the issue: it does not comment, assign or close
anything.

## Already started?

When an issue is already some workspace's starting point, the sheet says
"acme/api#812 already has a workspace, *Fix refund rounding*", with **Open it**
(goes there) and **Start another** (a second workspace, on its own branch,
`812-fix-refund-rounding-2`).

## Where the work came from

A workspace started from an issue shows it in the conversation header: the
key, title and state, as a chip. Click it to open the issue in your browser,
or `⌥`-click to open it in Lazybox's Web panel (the same as any link; see
[Web & links settings](appearance.md#everything-you-can-adjust)).

The agent's briefing gains one line, for example: "This work started from
acme/api#812 (GitHub issue). Reference it as `Fixes #812` in your commits and
pull request." GitHub then closes the issue when that pull request merges.

## From the terminal

```sh
lazybox new --from https://github.com/acme/api/issues/812 "Start with the ledger"
lazybox new --from acme/api#812                    # the issue alone
lazybox new --from acme/api#812 --another          # it already has a workspace
```

It makes the workspace and starts the agent the same way (in Ask; `--permission`
to choose), and prints the workspace's id. See the
[CLI reference](../reference/cli.md#work-with-the-window) for every option.

## What it needs

GitHub connected in **Settings → Repositories**, with read access to the
issues: a classic token's `repo` scope, a fine-grained token's **Issues: read**
on the repository, or the Lazybox GitHub App installed on it. Without it the
picker says to connect GitHub. If GitHub cannot be reached when you start, the
task still starts, from the issue's link alone.
