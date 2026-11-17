# Workspace terminals

A terminal is your login shell, running on the Runner, in a workspace: at its
root or in one of its repositories. It is for you, not for agents: agents run
managed commands (`lazybox_command`), and no agent tool can open, read or type
into a terminal.

## Use it

- **Terminal** in the workspace header shows the page's **Terminal** panel and
  puts the keyboard in it. With no panel, it docks one on the right of the work
  (or where you last docked a terminal). It starts a shell in the workspace
  root only when none is running; otherwise it focuses the running one. It
  never adds a second panel.
- The panel header moves it like any other panel (see
  [Arrange panels](WORKSPACES.md#arrange-panels)). Its **×** closes the
  *panel*: shells keep running and re-attach when the panel comes back.
- In the panel, pick a directory (**Workspace root** or a repository) and press
  **New terminal** for another shell. The list also has each repository's
  [home checkout](WORKSPACES.md#home-checkouts) and **Home folder (~)**: a
  shell there starts outside the workspace (`OpenTerminal` with
  `repository_home` or `user_home`, resolved by the Runner from its own
  records) but is still this workspace's terminal, with its credential. Tabs switch between them; a tab's **×**
  closes that shell (it hangs up the shell, as closing a terminal window does).
- **Panels → Terminal** in the workspace header also adds it. It docks like
  any other panel (see [Arrange panels](WORKSPACES.md#arrange-panels)). Opening or restoring the panel never starts a shell; it
  shows the workspace's running terminals.
- A reload, another window, or a Runner restart re-attaches to the same shell
  and shows its recent output.

### Send a command's output to the console, and to an agent

```sh
go test ./... 2>&1 | lazybox log --title "go test ./..."
lazybox run -- cargo test              # knows the exit status too
make lint 2>&1 | lazybox log --to "fix the parser"
```

No flag is needed. You see every line in the terminal as usual; the run also
appears in the **Command output** console of every conversation in this
workspace, labelled *From your terminal* (the console opens on it in the
conversation you are viewing). It ends with one line saying where the run went
and what an agent would get, for example `"go test ./..." is in the workspace
console (an agent gets 2.1 KiB of 1.4 MiB, go-test filter)`.

- **Send to agent**, in the console, adds the run's compact view to the message
  you are writing in the conversation on the page: the outcome first, failures
  kept, passing and progress lines dropped, and the path of the full-output
  file. You decide when to send. The agent never receives the full output.
- **`--to CONVERSATION`** (an id, a title, or a unique part of a title) does
  the same from the terminal: when the run ends, its compact view is added to
  that conversation's message box, once, the next time it is open. A name that
  matches nothing or several conversations is refused with the list to choose
  from; `lazybox log` still shows the output and publishes the run unshared,
  `lazybox run` runs nothing.
- For `lazybox log`, the title picks the compact view's filter: title it with
  the command (`--title "go test ./..."`) to get the go-test view. A pipe
  cannot see the producer's exit status; `lazybox run` can.
- If the Runner is unreachable, the output still reaches your terminal and the
  command's exit status is kept.

### What the shell may do with the Runner

Each terminal gets its own credential, never the owner's: anything started in
the shell, such as an agent CLI run by hand, runs with what the shell holds.
The shell's `LAZYBOX_V2_ENDPOINT` names `<state>/terminal-endpoints/<terminal
id>.json` (mode 0600: the Runner URL and a token bound to that terminal). The
token is accepted for five RPCs only (`internal/transport/auth.go`,
`terminalRPCs`):

- `RegisterPipe` for a run in **this terminal's workspace** (optionally shared
  with one of its conversations), never in a conversation's own console or in
  another workspace;
- `Publish`, `FinishPipe` and `FollowPipe` for runs **this terminal** opened,
  and `CancelCommand` to stop one (a server `lazybox run` moved to the
  background, see [command output](COMMAND_OUTPUT.md#servers-and-watchers)).

It cannot send, approve, answer, read conversations, open terminals or change
settings; every other RPC, including any added later, answers
`PermissionDenied` (a test walks the generated service description). Sharing
with `--to` only adds text to a message box: a person still sends it.

The token is an HMAC of the terminal id under the Runner's credential key, with
a different label from agent credentials. It needs no storage, so it survives a
Runner restart: the next Runner rewrites the file with its own URL when it
re-attaches the shell. It stops working when the terminal is closed (the file
is removed, and runs it left open are finished), when its shell exits, and
when the workspace is deleted.

## How it works

```
Panel (xterm.js)  ──WatchTerminal (stream, by byte offset)──┐
                  ──WriteTerminal / ResizeTerminal (unary)──┤
Runner            terminals.Manager: bounded scrollback per terminal, a record per
                  │ running terminal in <state>/terminals/
Session host      one per terminal, in <state>/terminal-hosts/
                  │ owns the pseudo-terminal and its own bounded scrollback
Login shell       session leader, the terminal as its controlling terminal
```

- RPCs: `OpenTerminal`, `ListTerminals`, `WatchTerminal`, `WriteTerminal`,
  `ResizeTerminal`, `CloseTerminal` (`services/runner/proto/api.proto`). They are
  only reachable through the Runner's authenticated API, like every other RPC.
- The directory is resolved on the Runner and must stay inside the workspace
  after symbolic links are resolved; `..`, absolute paths and links out of the
  workspace are refused.
- The shell is `$SHELL` if it names an executable, else `/bin/zsh`, else
  `/bin/sh`, started as a login shell (`argv[0]` is `-zsh`).
- The environment is the Runner's (that is, yours) minus Runner configuration and
  service wiring: every inherited `LAZYBOX_V2_*`, every `CODEFLY*` (including
  Codefly's scoped auth secret), the launching terminal's identity
  (`TERM_PROGRAM`, …), `PWD`, `OLDPWD`, `SHLVL` and `CLAUDECODE`. It adds
  `TERM=xterm-256color`, `COLORTERM=truecolor`, `LAZYBOX_V2_COLLECTION_ID`, the
  terminal's own `LAZYBOX_V2_ENDPOINT` (above, never the owner's),
  `LAZYBOX_V2_OUTPUT_DIR` (`<state>/command-output/workspace-<id>`, deleted with
  the workspace and bounded with all command output) and the Runner's CLI first
  on `PATH`. Credentials a managed account profile injects for agents are never
  added.
- A login shell rebuilds `PATH` from your profile (macOS `path_helper`,
  Homebrew, mise, cargo), which can put another `lazybox`, such as V1's, first.
  The terminal gets the same shell integration as agents
  ([command output](COMMAND_OUTPUT.md#which-lazybox-agents-run)): for zsh,
  `ZDOTDIR` points at generated startup files
  (`<state>/shell-integration/zsh/`) that each run your own, with your
  `ZDOTDIR`, then put the Runner's CLI first on `PATH`, once more before the
  first prompt for prompt hooks such as mise's. `ZDOTDIR` keeps pointing there
  so a nested `zsh` does the same; your history file stays where it was. Bash
  scripts and `bash -c` get `BASH_ENV`. `command -v lazybox` shows which one
  runs.
- `OpenTerminal` takes a request key: a retried request returns the shell it
  already started instead of starting a second one.
- Deleting a workspace hangs up its terminals (revoking their credentials) and
  deletes their full-output files before its files are removed.

### Attach from another terminal (tmux)

With **Settings → Runner → Process hosting → tmux**, a new terminal's host also
runs in a tmux window: **Copy attach** in the Processes panel gives
`tmux -S <socket> attach -t <host>`. That window is the same shell: what you
type in either place reaches it, and both show its output. The shell keeps the
size the panel sets. Closing the tmux window closes the terminal. See
[hosting backends](SESSION_HOSTS.md#hosting-backends-session-hosts-default-or-tmux).

### Surviving a Runner restart

A terminal runs in its own [session host](SESSION_HOSTS.md), exactly like agents
and managed commands, so a Runner restart, crash or upgrade does not reach the
shell. `Bench.Detach` leaves terminals running; the next Runner re-attaches them
when it opens (records in `<state>/terminals/`). `Bench.Close` and ephemeral test
Runners hang them up. A host with no Runner attached for 24 hours stops its shell
(`DefaultOrphanAfter`).

Terminal output is a view, not evidence: it is not journalled, and nothing is
recorded "exactly once". When a Runner attaches, the host acknowledges everything
it has read and sends its scrollback in the welcome; live output continues from
that offset, so each byte is shown once. Offsets are the host's byte count, so a
client can resume its stream across a Runner restart.

### Lifetime of the shell's processes

The shell is not under `processguard`: an interactive shell puts each job in its
own process group, which a group kill cannot reach. Its lifetime is bound the way
a terminal window binds it:

- **Close** sends SIGHUP to the shell's process group (the shell passes it on to
  its jobs), then SIGKILL after 2 seconds if the shell is still there. The shell
  is signalled only while it has not been reaped, so a reused PID is never hit.
- **Host exit** (killed, orphaned, or its directory deleted) closes the terminal,
  which hangs up the shell.
- After the shell exits, output still in the terminal is delivered; a background
  job that keeps the terminal open is not waited for longer than a second.

A process that ignores SIGHUP and left the shell's session (`nohup`, `setsid`,
`disown`) keeps running, as it would after closing any terminal.

The session host ignores SIGHUP, SIGINT and SIGPIPE, and an ignored signal stays
ignored across `exec`. A terminal host therefore handles those signals itself
before starting the shell, so the shell and its programs start with the default
dispositions and Ctrl-C works. `TestTerminalHostRunsAnInteractiveProgram` fails
without this.

## Limits

- At most 16 running terminals per Runner (`maxLiveTerminals` in the Runner
  config). Exited terminals stay listed until closed.
- Scrollback: the last 1 MiB of output per terminal, on the Runner and in the
  host. The panel keeps 5,000 lines.
- Input is at most 64 KiB per request; the panel splits larger pastes.
- Workspaces only: there is no terminal for an empty-workspace session or an
  arbitrary directory.
- A machine reboot ends every terminal.
- After a rollback to a release without terminals, running shells are not
  shown; their hosts stop them after the 24-hour orphan limit.
- Not verified yet: a live Runner restart in Dev with a terminal open (the
  restart path is covered by `TestTerminalOutlivesTheRunner`, which detaches and
  re-opens the manager in one process); the Tauri desktop window (the panel is
  tested in WebKit and Chromium through Playwright).

## Tests

| Command (from the repository root) | What it covers |
| --- | --- |
| `cd services/runner/code && go test ./internal/sessionhost -run Terminal` | PTY mode: controlling terminal, size, default signals, input, resize, exit code, hang-up, bounded scrollback handed over once |
| `cd services/runner/code && go test ./internal/runner/terminals` | A real `/bin/sh`: input, cwd, environment, resize (`stty size`), exit, re-attachment after detach, close, directory confinement |
| `cd services/runner/code && go test ./internal/transport -run Terminal` | RPCs against a real workspace: refused directories, request-key retry, environment, close; the terminal credential's scope (own workspace only, never another's, never Apply/approve/history), exhaustive allowlist, revocation on close, shell exit and workspace deletion, survival across a Runner restart, `--to` resolution and claim-once |
| `cd services/runner/code && go test ./internal/transport -run TestAPipeInAWorkspaceTerminal` | End to end: a real zsh login shell whose `.zshrc` puts another `lazybox` first; `printf … \| lazybox log` runs the Runner's CLI and publishes over Connect with the terminal credential |
| `cd services/runner/code && go test ./cmd/lazybox` | `lazybox log` and `lazybox run` without a session: everything echoed, the compact view kept, `--to`, output preserved on publication failure |
| `npm run test:terminal-share` | A terminal run in the console; **Send to agent** and `--to` fill the message box with the compact view, never the full output, and nothing is sent until the person sends (Chromium; `TEST_WEBKIT=1` for WebKit) |
| `npm run test:terminal` | The panel against the fake Runner in WebKit (`TEST_CHROMIUM=1` for Chromium) |
