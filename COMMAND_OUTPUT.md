# Command output for agents and humans

A command an agent runs has two readers. The human wants every byte, live,
in a console window. The agent wants the outcome and the failures, in as few
tokens as possible, and a way to get the rest when it needs it. V1's
`lazybox log` served only the human: it kept output out of the agent's
context entirely. V2 serves both from one run.

This page is the design and the roadmap. Pipe capture and Runner-owned
commands are specified in [execution](EXECUTION.md); this page covers what
each reader receives.

## What we know (November 2026)

- **Compression helps less than it claims, and can hurt.** rtk reports 60–90%
  fewer output tokens. Independent Terminal-Bench runs measured −5% to +5%
  cost and a 1–2 point lower pass rate, because rtk rewrote only 31% of
  terminal calls, terminal output was ~11% of input, and one unsupported flag
  caused a 339-error retry loop
  ([Quesma](https://quesma.com/blog/does-rtk-make-ai-coding-cheaper/)). The
  gain is in large test and build logs, not in every command.
- **Agents almost never fetch more output.** A study of real sessions found no
  agent-initiated request for a second page ([arXiv 2608.26130](https://arxiv.org/abs/2608.26130)).
  Whatever the agent must know has to be in the first view.
- **Agents truncate on their own and miss failures.** Claude pipes through
  `tail`/`head` by habit and loses the crash line
  ([claude-code#39945](https://github.com/anthropics/claude-code/issues/39945)).
- **Peers spill to a file.** Claude Code keeps ~30k characters inline and saves
  the rest to a file with a preview; Goose keeps the last 50 lines and the
  file path; Codex keeps head and tail of 10 KB. Cursor drops output between
  20k and 40k characters with no file.
- **Masking old observations matches summarizing them** at about half the cost
  ([arXiv 2508.21433](https://arxiv.org/abs/2508.21433)). Rewriting old tool
  results breaks the prompt cache, so compress once, when the result is made.
- **A live Claude session keeps the system prompt it started with.** Resuming
  with a different appended prompt has no effect until compaction. New
  instructions must arrive in the conversation, as a user message or hook
  `additionalContext` ([Agent SDK](https://code.claude.com/docs/en/agent-sdk/modifying-system-prompts)).
  Codex appends a developer message saying the new instructions replace the
  old ones. `setPermissionMode()` does work on a live Claude session.

## Design

**`lazybox run -- COMMAND`** (`services/runner/code/cmd/lazybox/run.go`)
runs the command itself, so it knows the real exit status and keeps stdout and
stderr apart. It:

1. streams every byte to the session's console as it arrives;
2. saves the full output to a private file in the session's output directory
   (see Retention below);
3. gives the agent a compact view built by
   [gortk](https://github.com/codefly-dev/gortk): a first line with the
   command and exit status, the output with passing and progress lines
   dropped and secrets masked, what was dropped, and the full-output path
   when anything was dropped or the command failed;
4. exits with the command's own status;
5. reports to the Runner, when it closes the console capture, what the agent
   received: its exit status, the size of the agent's view against the full
   output, the gortk filter and the full-output path.

Commands get no stdin, so nothing waits for input. If the Runner is
unreachable, the command still runs and the agent still gets its view.
`--raw` gives the agent the full output for diffs and JSON.

The command runs under a process guard (`internal/processguard`, as managed
commands do): it and everything it starts are one process group, stopped as
one, and swept when the command exits. A server started with `&` inside a
`lazybox run` therefore ends with the run; start the server itself with
`lazybox run` instead (next section).

### Servers and watchers

A server, a watcher or a log follower never exits, so whatever waits on it
hangs: the agent's turn, the message the person queued behind it, a Run
button. `lazybox run` (`run_background.go`) gives every command a grace period
(20 seconds; `--wait=DURATION` sets it). A command still running after it
goes on **in the background** when it listens on a TCP port (its process
tree's listening sockets: `lsof`, or `/proc` on Linux,
`internal/listenports`) or looks like a server or watcher by its command line
(`npm run dev`, `pnpm start`, `make up`, `vite`, `rails s`, `uvicorn`,
`docker compose up` without `-d`, `tail -f`, `--watch`, …:
`internal/serverlike`, whose vectors the client's Run buttons share). A
command that is neither keeps being waited for (a long test run, say), and
moves as soon as it opens a port. `--background` moves any command once it
has run two seconds (so a command that fails at once still reports its
failure); `--wait` keeps it in the foreground until it exits. A command that
ends within the grace period is an ordinary run, whatever it looks like.

Moving keeps the **same process**: `lazybox run` hands the process guard and
the command's output pipes to a follower (this CLI again, in a session of its
own, so the agent's tool or the person's terminal ending does not end it) and
returns at once with exit 0 and a report, for example:

```
[lazybox] npm run dev → running in the background as command 4F2K… after 20.1s: it listens on port 5173
[lazybox] listening on http://localhost:5173
[lazybox] latest output:
  VITE v5.4.0  ready in 1203 ms
  ➜  Local:   http://localhost:5173/
[lazybox] output so far (2.1 KiB, still growing): <state>/command-output/<execution>/npm-….log
[lazybox] the output streams to the Lazybox console. Stop it: lazybox stop 4F2K… · your commands: lazybox ls
```

The latest lines have colour codes and secrets removed. The follower keeps
publishing to the same console run and appending to the same file, reports
the command's ports and process to the Runner (`FollowPipe`), and records its
exit (`FinishPipe`, with the signal when it was stopped). The console run
reads *Running in the background · port 5173*; Processes lists it (backend
*lazybox run*) with Stop. If the follower dies, the guard's helper stops the
command's group: nothing is orphaned. From a workspace terminal the report
goes to the terminal and the prompt comes back; without a Runner the command
still moves, and the report says how to stop it (`kill -TERM -PGID`).

**Stopping and moving from Lazybox.** A `lazybox run` follows its console run
(`FollowPipe`, a long poll), foreground or background. `CancelCommand` (Stop
in the console, the live bar, Processes; `lazybox stop ID`) reaches it there:
SIGTERM to the group, SIGKILL after 5 seconds. `BackgroundCommand` (the owner's
alone: **Move to background** in the live bar) moves a foreground run at once,
whatever it is. A followed run outlives a Runner restart: the next Runner
reopens it where its output stopped, the follower resumes publishing (keeping
up to 8 MiB it could not send meanwhile, and saying what it dropped beyond
that), and a run nothing hears from for 90 seconds is ended as *stopped
reporting*.

**Bounds.** One conversation, or one terminal, has at most 16 consoles open
at once (`lazybox run`, `lazybox log`, a terminal's run); a 17th is refused
until one finishes. An unfollowed console (`lazybox log`, a terminal's run)
that receives nothing is ended: after 15 minutes if it never received
anything, after 6 hours of silence otherwise. The producer's output still
reaches its terminal; only the console stops.

**Inside an agent**, `lazybox ls` lists the agent's own commands (running
first, a background server with its URL, then the last five that ended) and
`lazybox stop ID` stops one; the agent's credential lists and stops its own
commands only. The agent briefing says all this in two sentences.

`cmd | lazybox log` is unchanged for agents: a transparent tee for output from
a producer lazybox did not start. It cannot know the exit status, so `run` is
preferred.

**From a workspace terminal** (V1's `cargo test 2>&1 | lazybox log`), the
reader is a person who may hand the result to an agent. With no session, both
verbs publish a *workspace run*: the terminal shows every byte as usual, the
run appears in the console of every conversation in the workspace, and the CLI
also keeps the full-output file and builds the same compact view, sent with
the capture's completion (`CommandReport.agent_view`, at most 64 KiB; `piped`
for `log`, whose title picks the gortk filter). **Send to agent** in the
console, or `--to CONVERSATION` in the terminal, puts that view in the
conversation's message box; the person sends it. The agent receives the
compact view, never the full output. The terminal's credential, scope and
lifetime are in [terminals](TERMINALS.md#what-the-shell-may-do-with-the-runner).
Workspace runs are not counted in telemetry: they reach an agent only if a
person sends them.

**The console follows runs.** When a command starts in the session being
viewed, a collapsed "Command output" panel opens on it, without taking focus
from the composer. Commands that existed before the session was opened never
open it. **Putting it away is a choice that holds:** hidden (its title),
closed (×) or toggled from the keyboard (`] ] o`, "Show or hide command
output" in the command palette), it stays away through later runs, per
workspace and across reloads, until the person opens it again. It then shows
the newest run and follows runs as before. A closed panel comes back from
**Page actions → Show panel → Command output**, the same key, or **Open in
console** in the chat's live bar. The live bar's own output lines can be
hidden too (the × in their corner, **Show output** to bring them back); that
choice is shared by every conversation. Each run's header reads, for example, `exit 1 · agent saw
2.1 KiB of 1.4 MiB (go-test filter)`, with the full-output path to copy. The
Runner records the same numbers in telemetry (`commands` table, one row per
run) to measure savings (step 6).

**Reading the output as a log** (`log-lines.ts` reads it, `LogView.tsx`
shows it, in the docked console, the console window and the live bar's
tail). Output from several services (Codefly's `api/web  > …` and
`[codefly] …`, docker compose's `web-1  | …`, pnpm's `packages/web dev: …`,
turbo's `web:dev: …`) is laid out in columns: a dim time gutter for a leading
`21:28:14` (or an ISO or Go timestamp), the source in its own colour (one of
six theme tokens, `--log-source-1` … `-6`, the same for a name from run to
run), then the message, which wraps under itself; an indented line (a stack
frame) belongs to the source above it. A level the program wrote (`(WARN)`,
`[ERROR]`, `INFO`, `error:`, `npm ERR!`, `--- FAIL`, `level=warn`) is a small
badge; an error row is tinted. A milestone (`>> api: Running on :8080`,
"listening on", "ready in", Vite's `Local: http://…`) is a highlighted row.
A web address, a local port (`:57771`, `localhost:3000`, `port 3000`) opens
in the Web panel, ⌘/Ctrl-click in the browser; `file.go:42` opens in the
editor. A long bracketed list or run of addresses folds to `… 9 addresses`,
and the same line again (ignoring its time) folds to one row with ×N; both
open in place. With more than one source, a row of chips shows each with its
count: a click hides or shows its lines, the target (or ⌥-click) shows only
it; the choice is kept with the console's view. **Hide noise** shows each
kind of warning once (`(WARN) (Flow.checkLoopback) …`, numbers and lists
aside) and folds progress lines that differ only in numbers ("still
starting… 5s, 10s"); it is remembered per command line, so the next
`codefly run` starts quiet too. None of it is lossy: Copy and the full-output
file are the output as written. Lines are read once as they arrive; only rows
near the visible part are in the page, so the 512 KiB tail stays fast. The
text is JetBrains Mono (bundled), or the code font from Settings → Appearance.

**Running what the agent wrote.** A shell code block in an agent's message
(`bash`, `sh`, `shell`, `zsh`, or a `console` transcript, whose prompts and
output lines are dropped) has a **Run** split button beside Copy
(`code-runs.tsx`, rules in `code-run.ts`):

- **Run** (also Enter on the focused block) runs it in a shell of the
  Terminal panel; see "Run is the terminal" below. **Paste in terminal (don't
  run)**, in the ▾ menu, types it there without Return, to edit first.
- **Ask the agent to run it (no terminal)**, in the ▾ menu, sends the agent
  "Run this and tell me the result:" and the block, fenced, as its own message: the draft in the
  message box is untouched. The agent runs it with `lazybox run`, so it sees a
  failure and reacts. During a turn the message waits in the Runner's queue and
  the block says "Queued — runs after the current turn". The block then shows
  the agent's run from its matching tool call: running, ran ✓, failed ✗.
  A block that looks like a server or watcher (the same rules as
  `lazybox run`, `serverLike` in `code-run.ts`) asks instead "Start this in the
  background (it keeps running) and tell me when it is ready:"; when a
  workspace action runs the same command, "Start the workspace action
  `api/dev` (it runs this) and tell me when it is ready:", so the agent uses
  `start_action`. Its chip reads *Sent to Claude · starting…*, then *running
  in the background · port 5173* with **Open preview**, from the agent's tool
  call (a `lazybox run` report, or a `start`/`start_action` that returned).
- **Run as command output (no terminal)…** (the ▾ menu, the block's right-click menu, ⌥Enter) opens a
  confirm card under the block: the script, editable; the folder (the
  workspace root or one of its repositories, by default the conversation's);
  Run (Enter) and Cancel (Escape). A script with a placeholder (`<your-token>`,
  `…`, `YOUR_API_KEY`) cannot run until it is filled in. It runs as
  `/bin/sh -lc SCRIPT`, a command of the conversation (`StartCommand`, with
  its `cwd`, which the Runner requires to be inside the conversation's folder),
  and the console opens on it. The block shows Running, ✓ exit 0 or ✗ exit N;
  a failure offers **Ask the agent to fix**, which adds the script and the last
  60 lines of its output to the message box for the person to send. A server
  or watcher block is started the same way and never waited for: the card says
  it keeps running in the background, and the chip reads *Running ·
  starting…*, then *Running · port 3000* (the Runner reads a managed
  command's listening ports, `CommandStatus.ports`) with **Open preview** (the
  Web panel, or the browser), Show output and Stop. When a workspace action
  runs the same command (equal once `$LAZYBOX_*` arguments, a trailing `--`
  and a leading `cd <repository> &&` are set aside), Run myself starts the
  action instead (**Start action**: it gets the workspace's own port and shows
  in Run and Preview), and the chip follows the action.

`StartCommand` is the owner's alone, so a read-only or Plan conversation runs
a person's command too: the mode bounds the agent, and the person could type
the same command in a workspace terminal. The confirm card says so. An
agent's own start in such a conversation is still refused. Nothing runs
without a click or key, and Run as command output always confirms.

**Run is the terminal (2026-11-17).** A block's **Run** (its main button,
`Enter` on the focused block, `] ] r`, `/run`) used to ask the agent. An
interactive command then failed there: an agent's commands have no TTY, so a
login or browser sign-in cannot run through it, and an agent said as much when
a Run arrived. Run now runs it in a shell of the workspace's Terminal panel (a
real PTY) in the conversation's folder (`home-shell.ts`, rules in
`shell-input.ts`): a shell already open there and idle at its prompt
(`TerminalStatus.idle`), else a new one. A one-line command with nothing to
fill in runs (Return is sent); a script of several lines or one with a
placeholder is typed at the prompt, and the block says *Typed in the
terminal: review and press Return*. Nothing is written before the shell can
read it: zsh says so (OSC 133;B, `prompts_ready`); a shell without the mark
(bash, fish) gets it once its output settles; a timeout types it anyway. The
▾ adds **Paste in terminal (don't run)**, each repository's
[home checkout](WORKSPACES.md#home-checkouts), the home folder, the person's
own terminal app and Copy. Asking the agent and command output come last,
labelled *no terminal*, and are not offered for a command that needs a
terminal (`needsTerminal` in `home-run.ts`: a login, `auth`, a sign-in,
`--no-launch-browser`). The shell marks each command line's end (OSC
133;D;<status>, agentshell's zsh integration); the Runner counts the marks on
the terminal (`TerminalStatus.command_ends`, `last_command_status`), so the
block says *Finished · exit N* and offers **Tell Claude it's done** without
the window following the output itself.

Inline code that reads as a command (`make lint`, `bin/devbox check`, or a
bare subcommand such as `build` next to a program the message runs by path,
read as `bin/devbox build`) offers the same two in its right-click menu only,
so prose stays calm. **Run the last command block in the terminal** (`] ] r`)
and **Run the last command block as command output…** are in the command
palette.

The hand-over of a failed run carries the end of its output, not a gortk
view: only `lazybox run` builds one, and a person's run is not wrapped in it.

**Retention.** The Runner gives every execution
`LAZYBOX_V2_OUTPUT_DIR=<state>/command-output/<execution id>`. Deleting a
conversation deletes the directories of all its executions. Both output roots
together are bounded (1 GiB by default, `commandOutputBudget` in the Runner
config): after each run the oldest files are deleted first, but never a file
modified in the last hour, so a path the agent was just given stays readable.
When the variable is absent, or its directory is not writable, the CLI falls
back to `$TMPDIR/lazybox-output/<execution id>` (or `$TMPDIR/lazybox-output`
outside a session); the Runner deletes and bounds that directory too.

**Every coding agent is told about it.** The Runner appends a fixed briefing
(`internal/runner/command_output.go`) after the editable Settings → Agents
text, and puts the CLI first on the agent's PATH (next section). Being part of
the binary, it changes with releases without a settings migration. The same
briefing tells agents to publish plans and reports with
`lazybox artifact put` ([artifacts](ARTIFACTS.md)), and that `lazybox` is this
CLI (`lazybox --version` prints `lazybox <version> (Lazybox V2)`): if a
lazybox command answers `unknown command`, the agent's shell found another
lazybox, and it calls this one by its full path, which the briefing names
(also `$LAZYBOX_V2_CLI`). The launcher builds the CLI from the Runner's own
source; the Runner briefs agents only when the binary exists.

### The briefing, and what running conversations hear when it changes

The shared documents guidance asks agents to prefer Mermaid for workflows,
state transitions and service interactions, use tables and checklists where
they help, and record sources, assumptions and verification.

An agent's briefing is a list of sections, each with an ID and a version
(`runner/briefing.go`): the person's Settings → Agents text (`settings`) and
its provider part (`provider`), the Runner's own sections (`workspace`, `run`,
`documents`, `conversations`), and the conversation's browser (`browser`,
[BROWSER.md](BROWSER.md)). A Runner section's version is a number with a
history: each version has a one-line note of what it changed and a phrase it
brought into the text. The person's text and the browser are versioned by a
hash of their text.

A conversation records the version of each section it was told
(`sessionstore.Record.Briefing`), at its start and after each update; a
successor (a resume, a restart) inherits it. When one is behind, the next
message the person sends **that starts a turn** carries one compact note
(`op.Context`, out of band beside the person's text):

```text
[Lazybox] Since this conversation started, Lazybox added or changed the following; everything else you were told still holds.
- Start another conversation with any agent (Claude, Codex, …) when the person asks: `lazybox conversation new --provider P --title T --brief-artifact ID` (or `--brief "…"`), which the person approves in one click. Your own sub-agent tool only starts copies of yourself, and switching this conversation's subscription never changes its provider.
```

- **Only the delta.** A Runner section behind contributes the notes of the
  versions it missed (from the last that restates the section), never its
  whole text; the person's text and the browser, short and theirs, are given
  as they now read; text the person removed is said removed.
- **Once.** Delivery records the new versions durably and journals a
  `briefing_updated` event (the text, the message's operation ID, the
  sections and versions). The conversation shows that event as one quiet line
  before the message it went with, *Lazybox told Claude about new features*
  (or *… its updated instructions* when only the person's text changed), that
  opens to exactly what the agent received; the composer shows *Instructions
  updated* while one is due.
- **Never mid-turn.** While a turn runs or messages wait in the queue, a
  message goes without the update, which waits for the next one that starts a
  turn.
- **Conversations from before versions were recorded** are placed from what
  they were told (their start context and any whole update an older Runner
  sent): a section whose current text they hold is current; otherwise they are
  at the highest version whose phrase they hold.

Changing a Runner section's text is a version of its own:
`TestEachBuiltinSectionTextIsPinnedToItsVersion` pins each text, and fails
until the change has its version, phrase and note in `builtinBriefings`.

### Which lazybox agents run

Putting the CLI first on the PATH an agent starts with is not enough. Claude
Code and Codex run every command through the person's shell, and its startup
files rebuild `PATH`: zsh reads `~/.zshenv` for every shell, scripts included,
so `typeset -U path; path=(~/.local/bin ~/.cargo/bin ~/go/bin $path)` there puts
`~/.cargo/bin` first; a login shell (Codex runs `zsh -lc`) also runs macOS's
`path_helper` and `~/.zprofile` (mise, Homebrew's `shellenv`). V1 installs a
`lazybox` in `~/.cargo/bin` or Homebrew, so `lazybox artifact put` ran V1's and
failed with `unknown command artifact`. Every verb agents use was affected.

The Runner therefore gives every agent, every command it starts and every
workspace terminal a **shell integration** (`internal/agentshell`, files in
`<state>/shell-integration/`), three hooks that each run after the person's own
files and put the CLI back first:

| Shell | Hook | Covers |
|---|---|---|
| zsh | `ZDOTDIR` at generated `.zshenv`, `.zprofile`, `.zshrc`, `.zlogin`, `.zlogout`. Each runs the person's own file of that name (from their `ZDOTDIR`, or `HOME`, at top level so `typeset` keeps its meaning) with `ZDOTDIR` set as they had it, follows a `ZDOTDIR` their `.zshenv` sets, then points `ZDOTDIR` back and moves the CLI to the front. | `zsh -c`, `zsh -lc`, interactive shells (and before the first prompt, for prompt hooks), nested shells |
| bash | `BASH_ENV`, which bash reads after its profile files for every non-interactive shell. It runs the person's own `BASH_ENV` first. | `bash -c`, `bash -lc`, bash scripts |
| Claude Code | `CLAUDE_ENV_FILE`, a script Claude Code runs inside every Bash tool command after its shell snapshot. It runs the person's own `CLAUDE_ENV_FILE` first. | a snapshot that exports a PATH without the CLI (an `env.PATH` in the person's Claude settings), any shell |

Also set: `LAZYBOX_V2_CLI` (the CLI's full path) and `LAZYBOX_V2_CLI_DIR`.
The person's own `ZDOTDIR`, `BASH_ENV` and `CLAUDE_ENV_FILE` travel as
`LAZYBOX_V2_USER_*` for the integration to chain to. An integration inherited
from another Lazybox (a Runner started from a Lazybox terminal) is recognised
by its marker file and never chained to itself.

Why these and not the alternatives:

- **The provider's own command wrapper** (Claude Code's
  `CLAUDE_CODE_SHELL_PREFIX`) receives the whole command string, snapshot
  `source` included, as one argument and must run it in a shell of its own,
  which reads `~/.zshenv` again and then the snapshot: it cannot act after
  either without rewriting the command. Codex's `shell_environment_policy.set`
  sets variables in the environment the shell starts with, which its startup
  files then override, exactly like the PATH the Runner already gave.
- **Startup-file hooks** run after the person's files, are read by every
  shell (nested ones too), and change nothing when nothing reorders PATH. They
  are what terminals use for their shell integration (VS Code sets `ZDOTDIR`
  the same way).
- **The full path alone** (`$LAZYBOX_V2_CLI`) works in any shell but asks
  every agent to remember it on every call; it is the fallback, not the fix.

Verified with the real programs (November 2026, a `~/.zshenv` that puts
`~/.cargo/bin` first): Codex 0.169 (`codex app-server` started as the Runner
starts it, and `codex exec`, both against a stand-in model endpoint) runs
commands as `/bin/zsh -lc` and reached another lazybox (mise's shim, and
Homebrew's from a nested `bash -lc`) before the integration and this one
after; Claude Code 2.1.311 (through the Agent SDK, as the worker runs it) and
2.1.325 kept the CLI first through their shell snapshot (which exports the
PATH Claude Code started with) but not in a nested `zsh -lc`, reached the V1
lazybox when the Claude settings set `env.PATH` even with `ZDOTDIR` and
`BASH_ENV` in place, and this one with `CLAUDE_ENV_FILE`. The Go tests
(`internal/agentshell`, `internal/runner/agent_cli_test.go`) reproduce each
case with a hostile home: `zsh -c`, `zsh -lc`, `zsh -ic`, nested shells,
`bash -c`, `bash -lc` (macOS's bash 3.2 too) and Claude Code's Bash tool
command line.

**Checking it.** The Runner probes, through the person's shell (`$SHELL`, and
bash when that is neither zsh nor bash, as Claude Code then uses bash),
`command -v lazybox` with `-c` and `-l -c`, without the integration (what the
startup files do) and with it (what agents get), at most every 10 minutes, in
the background. When another lazybox comes first:

- if the integration fixes every shell, the first new conversation shows a
  Runner line saying so, once per shadowing lazybox;
- if it does not (a shell none of the hooks reach), every new conversation
  says so and gives the full path, which agents are briefed to use.

**Settings → About → Agents** shows the result (`Version` RPC, `agent_cli`), and
`lazybox doctor` checks it (*Agents' lazybox*, failing when a shell stays
unfixed).

**Sessions that outlive a Runner restart.** A session host keeps the
environment its agent started with. One started with the integration reads its
files again for every command, so a newer Runner's files apply to its next
command. One started before the integration existed keeps only the CLI first
on its initial PATH: its agent gets the briefing's full-path fallback with its
next message (the briefing update, step 3 of the roadmap), and a new
conversation gets the integration.

The UI reads the CLI the same however it was called: the live bar, tool lines
and Smart activity, Run buttons and the server rules (`serverlike`, mirrored by
`code-run.ts`, `lazybox-cli.ts`) take `/full/path/lazybox`, `"$LAZYBOX_V2_CLI"`
and `${LAZYBOX_V2_CLI}` as `lazybox`.

## Roadmap

| # | Step | Status |
|---|---|---|
| 1 | `lazybox run`: live console, full-output file, compact failure-first view, real exit status | Done |
| 2 | Briefing and PATH for every coding agent; launcher builds the CLI for Dev and staged releases | Done |
| 3 | **Deliver changed instructions to live sessions.** Each briefing section is versioned and each conversation records what it was told; one behind gets only what changed, as one compact note with its next message that starts a turn (Claude: a text block ahead of the user's; Codex: an input item, since this `turn/start` has no context field), once, never mid-turn, shown as one quiet event ([The briefing](#the-briefing-and-what-running-conversations-hear-when-it-changes)). Inherited by resumes. Text removed from Settings is said removed. | Done |
| 4 | **Live permission change** from the session menu (Ask / Read only / YOLO). Claude: `setPermissionMode()`, and while in YOLO the worker approves tool requests itself, so sessions started without the bypass flag still switch. Codex: `approvalPolicy`/`sandboxPolicy` on following turns. Saved to the session record, so resume and account rotation keep it. A request already waiting still needs one answer. | Done |
| 5 | **Console polish.** Open the console when a run starts in the viewed session; label each run with what the agent received (bytes in → out, filter, exit); open the full-output file from the console. | Done, except opening the file: the console copies its path, since a browser view cannot open local files and the desktop shell has no open-file action yet |
| 6 | **Measure before going further.** Record per-run bytes in/out and the gortk filter in telemetry. Judge by cost per completed task and failure rate, not bytes saved. | Recording done (telemetry `commands`); no report or analysis yet |
| 7 | **Opt-in automatic wrapping** of known noisy commands (test, build, lint, install) through a Claude `PreToolUse` rewrite and the Codex equivalent. Only with step 6 showing a benefit; fall back to the original command on any wrapper error to avoid retry loops. | Later |
| 8 | **Retention.** Delete full-output files with their session; bound total size. | Done |
| 9 | **Servers and watchers never block.** `lazybox run` moves a command that still runs after its grace period and listens on a port or looks like a server to the background (same process, a follower); Stop and Move to background from Lazybox; `lazybox ls`/`stop` for agents; Run buttons start servers in the background; queued messages can be sent now or steered. | Done |

## Known limits

- The CLI is built when the launcher starts. A Go change to the CLI needs a
  relaunch; the Runner itself hot-reloads.
- The shell integration covers zsh, bash and Claude Code's Bash tool. A
  shell that reads none of its hooks (`sh -l`, which reads `~/.profile` and
  `/etc/profile`; an interactive bash; fish, which neither Claude Code nor
  Codex runs commands in) can still find another lazybox first; the probe
  reports it and agents fall back to the full path. A Codex
  `shell_environment_policy` with `include_only` that drops `ZDOTDIR` and
  `BASH_ENV` (and the `LAZYBOX_V2_*` the CLI needs to reach the Runner) defeats
  it too.
- Codex's `workspace-write` sandbox may block the CLI's connection to the
  Runner. The command still runs and the agent still gets its view; the
  report says the console did not receive it.
- That sandbox lets a command write only in the workspace and the temp
  directories, so under Codex in Ask mode full-output files land in the temp
  fallback rather than the Runner's state. They are still deleted with the
  conversation and bounded, but the system may also clear the temp directory
  on its own. A Codex agent that changes `TMPDIR` writes where the Runner does
  not look.
- The console opens only for runs it sees start while the session is viewed:
  it polls every 1.5 s, so a run that starts while switching sessions is in
  the list but does not open the panel.
- The compact view is built from the first 32 MiB of each stream. The console
  and the file keep everything.
- Commands needing a terminal (TUIs, password prompts) are out of scope; the
  interactive shell is roadmap R3.
- A server whose listening socket belongs to another process tree (Docker's
  daemon for `docker compose up`, say) has no port in the report or the
  console; the command line still moves it to the background.
- The server heuristics are a guess: a command with a server's name that ends
  on its own within the grace period is an ordinary run, and one with an
  unusual name is moved only once it listens on a port.
- Claude's own Bash tool (not `lazybox run`) cannot be moved: the live bar
  offers Interrupt, with a hint to start servers with `lazybox run`.
- A command in the background is a running command of its conversation:
  continuing the conversation from a checkpoint waits until it is stopped, as
  for any running command, and ending or deleting the conversation stops it.
- Not verified: a real agent's turn returning through Claude's Bash or Codex's
  shell tool after a move (the tests run the CLI directly), Codex's
  `workspace-write` sandbox around the follower, and Linux's `/proc` port
  lookup outside its unit test.
