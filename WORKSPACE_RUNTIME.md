# Workspace runtime: setup, actions, ports and preview

A repository can commit `.lazybox/workspace.yaml` to say what every new
workspace of it needs: files to copy from your checkout (`.env`), a setup
script, named **actions** (a dev server, a watcher, the tests), and an archive
script for when the workspace is deleted. Every workspace gets its own block of
ten ports, so two workspaces of the same app run side by side. The bar is
Conductor's setup/run/archive scripts and `CONDUCTOR_PORT`.

## Use it

- **Run** in the workspace header starts the default action. Its menu (▾)
  lists every action with its state and port; a running one shows a live dot,
  its port and how long it has run, with **Stop** beside it. While setup runs
  the control reads *Setting up api · 12s*; click it for the log.
- **Actions** (from Run's menu, or **Panels** in the workspace header) lists each
  repository's setup and actions: state, port, URL, the last output line,
  **Start / Restart / Stop**, **Log** (the command's console, inline) and
  **Preview**. Setup can be run again from here.
- **Preview** shows what an action serves (its `url`) in the page, in the
  same viewer as the [Web panel](WORKSPACES.md#the-web-panel): address bar,
  back and forward, reload, zoom and **Open in browser**.
- **Review scripts** appears when a repository's scripts are new or changed.
  The dialog shows the file, marks what changed since you last approved it,
  and offers **Approve and run setup**, **Approve** or **Not now**.
- **Settings → Workspaces** sets the port range and, per repository, your own
  additions (same YAML), the default action and *skip setup*.
- Action, setup and archive runs also appear in each conversation's **Command
  output** console, labelled *action*, *setup* or *archive*. They never open it
  by themselves.

## The file

YAML, like Lazybox's other configuration (`~/.lazybox/config.yaml`,
`workspace.codefly.yaml`). Setup and archive are multi-line shell, and YAML's
block scalars keep them readable and reviewable in a diff; TOML would need
quoted multi-line strings and JSON (Conductor's choice) escapes every line. A
JSON Schema for editors is at
[`workspace.schema.json`](services/runner/code/internal/runner/workspaceruntime/workspace.schema.json);
`TestSchemaMatchesTheParser` keeps it and the parser in step. Add
`# yaml-language-server: $schema=<that file>` for completion.

```yaml
version: 1                      # optional
setup: |                        # once per new worktree, after files are copied
  npm ci
archive:                        # before the workspace is deleted
  run: docker compose down -v
  timeout: 60                   # seconds; 120 by default
files:                          # copied from your checkout, never overwriting
  - .env
  - config/*.local.json
env:                            # for every script and action of this repository
  API_URL: http://localhost:$LAZYBOX_PORT_1
actions:
  web:
    run: npm run dev -- --port $LAZYBOX_PORT
    url: http://localhost:$LAZYBOX_PORT
    default: true               # what Run starts
    concurrent: false           # one workspace at a time (see below)
  api:
    run: go run ./cmd/api
    port: 1                     # this action's $LAZYBOX_PORT is the block's port 1
    cwd: services/api
    env: { LOG_LEVEL: debug }
  test: npm test                # a string is just `run`
```

Decoding is strict: an unknown key, a misspelt one or a wrong type is refused
with its line (`line 7: unknown key "comand"`), and nothing from that file
runs. Limits: 64 KiB, 32 actions, 64 files and variables; names are lowercase
`[a-z0-9._-]`; `cwd` and files stay inside the repository and never touch
`.git`; `url` must point at this machine (`localhost`, `*.localhost`,
loopback IPs); `LAZYBOX_*` variables cannot be overridden.

In a multi-repository workspace each member reads its own file. Actions are
keyed `<repository>/<name>` (`api/web`); the default is your Settings choice,
then the first `default: true`, then the first action.

## Approval

A committed script runs code on your machine as you, so nothing from a
repository's file runs until you approve it: not setup, not an action, not its
`env` (a variable like `NODE_OPTIONS` runs code too), not its file copies.

- An approval covers one exact version: the SHA-256 of the parsed
  configuration (comments and formatting do not count; any change to a script,
  file, variable or action does). It is kept per repository, on this Runner
  (`<state>/workspace-runtime/approvals.json`, mode 0600), with up to 32
  versions so a workspace on an older branch is not asked again.
- The approve request names the hash you were shown; if the file changed while
  the dialog was open it is refused and the dialog shows the new version.
- A changed file reads *changed* and lists the entries that differ
  (`setup`, `archive`, `files`, `env`, `action:<name>`).
- Your own entries from Settings need no approval: you wrote them. They run
  even while the repository's wait, with only your own variables.
- Approving is the owner's alone; an agent cannot approve (see below).

## Lifecycle

1. **Creation** (`PrepareWorkspace`, and the lower-level `CreateCollection` /
   `AddCollectionRepositories`): once the worktrees exist, each new member with
   approved (or your own) scripts gets its files copied and its setup started.
   The New workspace dialog shows each step (*Starting setup for api*, or
   *api has workspace scripts to review before they run*); setup itself runs on
   as a managed command, shown in Run and Actions. A retried request never runs
   setup twice. Adding a repository to a workspace does the same for it.
2. **Approval later**: *Approve and run setup* copies the files and starts the
   setup if it has not succeeded yet.
3. **Actions** start, restart and stop from Run, Actions, or an agent.
4. **Deletion**: after the confirmation, the workspace's actions and any setup
   are stopped, then each member's archive script runs in its worktree (bounded
   by its timeout) before the worktrees are removed; the port block is freed. A
   failing archive does not keep the workspace; its output stays in the command
   journal. A retried deletion does not run archive twice.

Scripts run in your login shell (`$SHELL -l -c`, as a terminal would start),
in the member's worktree or its `cwd`, with your environment minus the
Runner's own (`LAZYBOX_V2_*`, `CODEFLY*`, as [terminals](TERMINALS.md) drop),
`TERM=dumb`, and:

| Variable | Value |
|---|---|
| `LAZYBOX_PORT` | The action's port: `LAZYBOX_PORT_<port>` (0 by default) |
| `LAZYBOX_PORT_0` … `_9` | The workspace's block |
| `LAZYBOX_WORKSPACE_ID`, `_NAME`, `_ROOT` | The workspace |
| `LAZYBOX_REPOSITORY`, `LAZYBOX_WORKTREE` | The member's name and folder |
| `LAZYBOX_ROOT_PATH` | The source checkout it was made from |

`$LAZYBOX_*` in `env` values and `url` is expanded by Lazybox; anything else is
left to the shell. Scripts get no Runner credential.

## Ports

Each workspace gets a block of 10 consecutive ports from the range in Settings
(default 20000–29999: above the ports dev servers pick by default, below
Linux's and macOS's ephemeral ranges). The block is allocated when first
needed and kept until the workspace is deleted, across Runner restarts and
range changes (`<state>/workspace-runtime/ports.json`). Blocks never overlap;
a new block skips any block where a port already answers or cannot be bound.
Before an action with a `url` starts, its port is checked again: if another
program holds it, the start is refused with the port and `lsof -i :<port>`.

## Actions: start, stop, concurrency

- An action is a managed command in its own [session host](SESSION_HOSTS.md),
  so it survives a Runner restart, reload or upgrade like any command, and its
  output is in the console (`kind: action` in the command record).
- **Start** on a running action does nothing; **Restart** stops it first.
- **Stop** sends SIGTERM to the action's whole process group, then SIGKILL to
  the group after 5 seconds. The group's helper (`processguard`) sends the
  signal itself, on a request over its lifetime pipe, so no PID is ever
  signalled from outside; the session host applies the 5 seconds, so a stop in
  progress completes even if the Runner goes away. An action reads *stopping*
  until its group is gone, then *stopped*; one that exits by itself reads
  *succeeded* (exit 0) or *failed*.
- **`concurrent: false`** runs the action in one workspace at a time: starting
  it first stops the same repository's action of the same name in every other
  workspace (and waits until it is gone). The UI says what it stopped, and an
  action shows *also running in Beta release* before you start it.
  `concurrent` defaults to true, since every workspace has its own ports.
- An action cannot start while its repository's setup is running.

## Preview

The Preview panel is the Web panel's viewer on the action's URL
([WORKSPACES.md](WORKSPACES.md#the-web-panel)): in the desktop app a native
web view over the panel, in a browser an iframe. A restart or another action
starts a new history at its address. In the desktop app links followed inside
the page are history too; in a browser a page on another origin hides its own
navigation, so back and forward step through the addresses opened in the
address bar. **Open in browser** opens the current address in your browser
(the desktop host's `open_link`, http/https/mailto only; `window.open` in a
browser).

Limits:

- `localhost` is the machine running the window. With a Runner on another
  machine the preview does not reach the action; open it there, or forward the
  port.
- In a browser, a page that forbids framing (`X-Frame-Options`,
  `frame-ancestors`) shows blank, with a line saying so and Open in browser.

## Agents

An agent's `lazybox_command` tool (Claude's `mcp__lazybox__command`, Codex's
dynamic tool) has two more actions: `actions` lists the workspace's actions
with their state, URL, port and command id (readable with `read`), and
`start_action` starts one by name (the default when no name is given;
`restart: true` restarts it). Starting follows the session's permission like
any command start: refused for read-only, planning and assistant sessions; in
Ask mode the person approves the tool call; it never bypasses script approval.
Agents cannot approve scripts, stop actions or change runtime settings: those
RPCs are not on the agent or terminal allowlists
(`TestWorkspaceRuntimeRPCsAreTheOwnersAlone`), and the agent briefing tells
agents to use `start_action` for a workspace's dev server (and `lazybox run`,
which moves servers to the background, for any other;
[command output](COMMAND_OUTPUT.md#servers-and-watchers)).

A shell block in an agent's message that runs an action's command (equal
once `$LAZYBOX_*` arguments, a trailing `--` and a leading `cd <repository>
&&` are set aside) is the action: **Run as command output** starts the action
(`StartWorkspaceAction`) rather than a plain command, and **Run** asks the
agent to start the action by name.

## RPCs

`GetWorkspaceRuntime`, `ApproveWorkspaceScripts`, `StartWorkspaceAction`,
`StopWorkspaceAction`, `RunWorkspaceSetup`, `GetWorkspaceRuntimeSettings`,
`SaveWorkspaceRuntimeSettings` (`services/runner/proto/api.proto`). Owner
only. Code: `internal/runner/workspaceruntime` (format, approvals, ports, file
copy), `internal/runner/workspace_runtime.go` (lifecycle and actions),
`internal/transport/workspace_runtime.go`.

## Known limits

- Setup is not queued behind approval in the creation dialog: the dialog
  closes on *Ready* and setup continues in Run and Actions.
- Database or other external-resource isolation is not provided: use `env`
  with `$LAZYBOX_PORT_n` or the workspace id, and setup/archive hooks.
- An action that leaves its process group (`setsid`, `nohup … &`) is not
  stopped by Stop, as with any managed command.
- In a browser, the Preview's back/forward cannot follow navigation inside a
  cross-origin page (see Preview).
- Not verified: the desktop (Tauri) app build and its native Preview view; a live
  Runner restart in Dev with an action running (covered by
  `TestActionRunsOnTheWorkspacePortsOutlivesTheRunnerAndStopsGracefully`,
  which detaches and reopens the Runner in one process).

## Tests

| Command (from the repository root) | What it covers |
| --- | --- |
| `cd services/runner/code && go test ./internal/runner/workspaceruntime` | Parsing, line-numbered errors, schema ↔ parser keys, approval hash (formatting ignored, every runnable change counted), changed entries, approvals across restarts, port blocks (stable, disjoint, busy ports skipped, released, exhausted), settings revisions, file copy confinement |
| `cd services/runner/code && go test ./internal/runner -run 'TestSetupWaits\|TestActionRuns\|TestNonConcurrent\|TestArchiveRuns\|TestAgentStarts'` | Lifecycle with real git worktrees and shells: approval gates setup and files; setup sees its ports and copied `.env`; actions on their ports, surviving a Runner restart, stopping gracefully on SIGTERM; non-concurrent stop elsewhere; archive before deletion and the block freed; agent permission |
| `cd services/runner/code && go test ./internal/processguard -run Interrupt` and `./internal/sessionhost -run Grace` | SIGTERM to the group first, SIGKILL after the grace period, descendants included |
| `cd services/runner/code && go test ./internal/transport -run WorkspaceRuntime` | Owner-only RPCs (agent and terminal credentials of the same workspace denied), settings validation and listing |
| `npm run test:workspace-runtime` | Run, its menu, non-concurrent start, Stop, Actions and logs, Preview, the approval prompt, Settings, phone width (Chromium; `TEST_WEBKIT=1` for WebKit), against the fake Runner |
