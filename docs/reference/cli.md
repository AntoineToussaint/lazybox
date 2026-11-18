# The `lazybox` CLI

Everything you do in the window is scriptable from a terminal: list and follow
conversations, send, approve, stop, wait, open things in the window, take a
screenshot, run your actions. Every command below takes `--json` and exits
with a [documented code](#exit-codes).

The installed app puts it on your `PATH` when you ask (first run, Settings →
About, or `lazybox link`); from a checkout, `make build-go` builds it into
`bin/lazybox`. Agents in Lazybox already have it on their `PATH`. Run `lazybox` with no arguments for the
list. The complete, contributor-level reference is
[`cmd/lazybox/README.md`](../../services/runner/code/cmd/lazybox/README.md).

## Which Runner it talks to

You rarely need to say. In order, it uses:

1. `--endpoint FILE` — an endpoint file (`<state>/endpoint.json`);
2. `LAZYBOX_V2_ENDPOINT` — set for you inside Lazybox (an agent, a workspace
   terminal, a hook);
3. `--runner NAME` or `LAZYBOX_RUNNER=NAME` — a registered Runner;
4. the Runners registered in `~/.config/lazybox/runners/` — the installed
   app's (`lazybox`) and the Dev and Working launchers': the only one, or
   `lazybox`, then `working`, when several run.

`lazybox runners` shows them and which one is live. `--endpoint` and `--runner`
go **before** the command: `lazybox --runner dev ls`.

## Runners on your other computers

Over your own `ssh` and `~/.ssh/config` ([Runners on your other
computers](../guides/remote-computers.md)):

| Command | What it does |
| --- | --- |
| `lazybox remote add <host> [--name N] [--bundle FILE]` | Checks the computer, installs and starts its Runner if needed (the computer verifies the SHA-256 first), adds this computer's key to its owners and pins its Runner's key. `--progress json` prints one line per step. |
| `lazybox remote ls` · `remote rm <name>` · `remote hosts` | The computers added and how each link is; forget one (and revoke this computer's key there; `--keep-on-host` does not); the hosts of `~/.ssh/config`. |
| `lazybox link connect <name>` | Keeps the link to that computer's Runner open and serves it on this computer, so `lazybox --runner <name> …` works. The app runs one per computer by itself. |
| `lazybox link owners [ls\|revoke KEY]` | **On the other computer**: the computers with owner access to its Runner; a revoke ends their links within a second. |
| `lazybox link up` | **On the other computer**: start its Runner if it is not running. |

## Follow and answer

| Command | What it does |
| --- | --- |
| `lazybox ls [--all]` | Workspaces, their conversations and agents, with live state (needs input, failed, working, review, idle, ended). `--all` covers every registered Runner. |
| `lazybox inbox [--all]` | What needs you, in order — needs input › failed › working › ready for review — with the `approve` / `deny` line for each pending approval. |
| `lazybox send <conversation> [text\|-]` | Send a message (text from arguments, or stdin with `-` or a pipe). An ended conversation is resumed first. `--steer` steers the running turn. |
| `lazybox approve <request\|conversation>` / `deny …` | Answer a pending approval, by request id, or by conversation when it has exactly one. Questions are answered in the app. |
| `lazybox stop <conversation\|command> [--agent ID] [--end]` | Interrupt the running turn; `--agent` stops one sub-agent; `--end` ends the conversation (resumable). A command id cancels that command. |
| `lazybox wait <conversation> --until idle\|needs-input [--timeout 30m]` | Block until reached (exit 0); exit 5 on timeout; exit 1 if the turn failed. |
| `lazybox watch <conversation\|workspace> [--since 10m] [--tail N]` | Follow events live as a timeline. `--json` prints one JSON record per line. A workspace follows every live conversation. |
| `lazybox debug <conversation> [--events N]` | A diagnosis in plain words: busy, queued, faults, pending requests, sub-agents, the last lifecycle events, and findings such as "the last turn ended by an interrupt you sent". |

A conversation is named by its id, its title (case does not matter), the start
of its title when only one matches, or `Workspace/title` to look in one
workspace. An ambiguous name lists the candidates and exits 2.

## Work with the window

| Command | What it does |
| --- | --- |
| `lazybox open <workspace\|conversation\|file[:line]> [--panel KIND]` | Show it in the Lazybox window in front. A file opens in its workspace (`--workspace W` when there are several). |
| `lazybox shot [window\|PANEL] [--out FILE]` | A PNG of the window or one panel, taken by the desktop app itself (no screen-recording permission). A browser tab cannot. |
| `lazybox new <name> --repo R [--repo R…] [--branch B] [--start-from B]` | Create a workspace from repositories Lazybox knows (`owner/name`, a name or a path). Each repository's branch starts from the branch last chosen for it, else origin's default; `--start-from B` starts every one from origin's `B` (fetched first) and is remembered for next time ([Start from another branch](../getting-started.md#start-from-another-branch)). Also with `--from`. |
| `lazybox new --from <url\|owner/name#N> [MESSAGE]` | [Start from an issue](../guides/start-from-an-issue.md): the workspace in the issue's repository on a branch named after it (`812-fix-refund-rounding`), and the agent with your message and the issue quoted after it. The agent starts in Ask; `--permission ask\|read-only\|yolo` chooses. `--name`, `--branch`, `--repo` (instead of the issue's repository) and `--profile` override; `--no-comments` leaves the comments out. An issue that already has a workspace says which and exits 1; `--another` starts one more. Prints the workspace's id (`--json`: with its branch and conversation). |
| `lazybox do <action> [argument] [--workspace W] [--conversation C]` | Run any action of the app in the window, as its key or ⌘K would: `lazybox do layout.maximize`, `lazybox do settings.open keyboard`. Your own actions' ids work too. |
| `lazybox do --list [WORDS…]` | Every action of the app by group, with its default keys, its argument, and **safe** when a hook may run it. |

Panels for `--panel` and `shot`: `chat`, `console`, `terminal`, `assistant`,
`changes`, `usage`, `sessions`, `missions`, `pulls`, `processes`, `actions`, `preview`,
`files`, `editor`, `artifacts`, `settings`, `inbox`.

## Missions, artifacts, actions, hooks

| Command | What it does |
| --- | --- |
| `lazybox missions [ls\|show ID\|new TITLE --brief B\|accept ID TASK\|reject ID TASK --note N]` | [Missions](../guides/missions.md) and their tasks; accept or send back work. |
| `lazybox artifact put FILE\|- --title T --kind plan` · `get ID [--version N]` · `list` | Publish and read a workspace's [artifacts](../guides/artifacts.md). Used by agents, from inside their workspace. |
| `lazybox actions [ls\|validate [FILE]\|schema\|path\|init\|run ID]` | Your [actions file](configuration.md#actionsyaml-your-actions-and-hooks): list what loaded and its problems, check a file, print the schema, create it, or have the window run an action. |
| `lazybox hooks [ls\|runs\|test ID\|log RUN]` | Your hooks, their recent runs, a test run, a run's output. |
| `lazybox config path [FILE]` | The folder of your [config files](configuration.md#your-config-files-keys-settings-layouts), or one file's path. FILE is `keys`, `settings`, `actions` or `layout NAME`. |
| `lazybox config edit FILE` | Open it in `$VISUAL`/`$EDITOR` (created from a commented starter), then check it. |
| `lazybox config validate [FILE…]` | Check every config file (or these, by name or path): problems as `file:line`, exit 2 on an error. |
| `lazybox config get FILE [KEY]` | The file, or one value: `config get keys palette.open` prints its keys and where they come from (the file and line, or the preset). |
| `lazybox config set FILE KEY VALUE` · `--remove` | Change one value, keeping comments: `config set keys palette.open Mod+y`, `config set settings notifications.review true`. A change with a problem (a key already taken) is refused and nothing is written. |
| `lazybox config export [--out F] [--actions]` · `import F\|- [--dry-run]` | Every config file in one YAML bundle (actions.yaml only with `--actions`: its webhook headers may be secrets), and back: each file is checked first, a replaced one is kept as `.bak`. |
| `lazybox extension new <id> [--example E]` · `validate [<id>\|PATH…]` · `list` · `path` | Your [extensions](extensions.md#lazybox-extension): a starter (or an example) in the extensions folder, each checked by `file:line`, what each may do and whether it is on. Approving is in the app. |
| `lazybox process validate\|show\|explain EVENT\|events\|schema` | Check [`.lazybox/process.yaml`](configuration.md#lazyboxprocessyaml-how-missions-run) from files, no Runner needed. |
| `lazybox link URL [--role delivers\|mentions] [--workspace W]` | Track a [pull request](../guides/review-agent-work.md#pull-requests-your-agents-open) this workspace delivers (the default for a pull request URL), or list an item it only mentions. An agent links to its own workspace; you name one with `--workspace`. |
| `lazybox items [--workspace W \| --all]` | The workspace's tracked pull requests: state, checks (failing ones with their links), review, and what it mentions. `--all` is every workspace's (your credential). |

## Command output

| Command | What it does |
| --- | --- |
| `cmd 2>&1 \| lazybox log [--title CMD] [--to CONVERSATION]` | Publish piped output to the workspace's Command output console. `--to` also hands its compact view to a conversation's message box. |
| `lazybox run [--to CONVERSATION] -- cmd args…` | Run a command: you see every byte, the console keeps it with its exit status, an agent gets the compact view. |

Both need no flag inside a Lazybox workspace terminal or agent. From your own
terminal they find the Runner as above, and the workspace you are in (your
current directory inside its folder or one of its worktrees), or the one
`--collection` names by id or by name — see
[Run your app](../guides/run-your-app.md#send-your-terminals-output-to-lazybox).

## The installation

For the installed app ([Install](../install.md)):

| Command | What it does |
| --- | --- |
| `lazybox --version` | The release. The app, the CLI and the Runner always share it. |
| `lazybox doctor` | Checks everything the app needs and prints the fix beside each problem. |
| `lazybox service [status\|install\|start\|stop\|restart\|logs [-f]\|plan\|uninstall]` | The Runner's launchd service. `install` also repairs it and refreshes the PATH agents get; a restart or update keeps agents running. |
| `lazybox link [--dir DIR]` | Puts this `lazybox` on your PATH with completions, no password, and says when another `lazybox` comes first on your PATH, with the fix. (`lazybox link URL` tracks a pull request instead, above.) |
| `lazybox link --remove-older [--yes] [--dry-run]` | Removes an older Lazybox installed by Homebrew that comes first on your PATH: shows `brew uninstall lazybox`, runs it once you confirm, and says which `lazybox` your terminal runs afterwards. Anything else (a version manager's shim, another program) gets its fix, never a removal. See [Another `lazybox` comes first](../install.md#another-lazybox-comes-first). |
| `lazybox uninstall [--dry-run] [--remove-data]` | Removes the service, link and completions; `--remove-data` also your conversations and workspaces, after asking. |
| `lazybox schema [actions\|process\|workspace\|keys\|settings\|layout\|theme] [--write DIR]` | The JSON Schemas of the files you write, for your editor; they ship with the CLI, nothing is fetched. |

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Done |
| 1 | Failed: the Runner refused, the turn failed, the hook failed |
| 2 | Usage or validation error: bad flags, an ambiguous name, an invalid actions file |
| 3 | Not found: conversation, workspace, request, action, hook |
| 4 | Not allowed: a scoped credential (agent, terminal, hook) or a refused token |
| 5 | Timed out: `wait`, or a window that did not answer |
| 6 | Unavailable: no Runner answers, none is registered, or no Lazybox window is open for `open` / `shot` / `actions run` / `do` |

## Shell completion

Completes commands, flags, and the names of your conversations, workspaces,
actions and hooks:

```sh
lazybox completion zsh  > "${fpath[1]}/_lazybox"      # or: source <(lazybox completion zsh)
lazybox completion bash > ~/.local/share/bash-completion/completions/lazybox
lazybox completion fish > ~/.config/fish/completions/lazybox.fish
```

## What agents and hooks may do

Inside an agent, a workspace terminal or a hook, the CLI uses that place's own
**scoped** credential, never yours: an agent reaches its own console and its
workspace's artifacts; a terminal only its own runs; a hook only what its
`grants` allow. Your verbs fail there with exit 4 and a sentence saying what
the credential may do.
