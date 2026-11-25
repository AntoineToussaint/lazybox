# `lazybox` — the V2 CLI

Build with `make build-go` from the repository root (`bin/lazybox`); the Dev and Working
launchers build it into the environment and put it first on every agent's
`PATH`. It never resolves V1 daemon state. See `RUNNING.md`.

Everything a person does in the app is scriptable from here: list and follow
conversations, send, approve, stop, wait, open things in the window, take a
screenshot, run your own actions, inspect hooks. Every power verb takes
`--json` and exits with a documented code, so scripts and agents can rely on it.

## Which Runner it talks to

1. `--endpoint FILE` — an endpoint file, as written (`<state>/endpoint.json`
   is the owner's).
2. `LAZYBOX_V2_ENDPOINT` — set inside Lazybox: an agent's, a workspace
   terminal's or a hook's own **scoped** endpoint.
3. `--runner NAME` or `LAZYBOX_RUNNER=NAME` — a registered Runner.
4. The registered Runners: the Dev and Working launchers register theirs in
   `$XDG_CONFIG_HOME/lazybox/runners/<name>.json` (else
   `~/.config/lazybox/runners/`), and an installed release's service its own
   as `lazybox`. With one, it is used; with several, `lazybox` (the installed
   app) and then `working` are; otherwise the CLI asks you to choose.
   `lazybox runners` lists them and which is live.

`lazybox run` and `lazybox log` resolve the Runner the same way from your own
terminal, and the workspace from `--collection` (an id or a workspace's
name) or, without it, the workspace whose folder or worktree you are in.

Inside an agent, a workspace terminal or a hook (any of
`LAZYBOX_V2_SESSION_ID`, `LAZYBOX_V2_COLLECTION_ID`, `LAZYBOX_V2_ENDPOINT`
set), the CLI **never** falls back to a registered owner endpoint on its own:
it uses the scoped credential it was given, and owner verbs fail with exit 4
and a sentence saying what that credential may do. An explicit `--runner` or
`--endpoint` is the person's choice and is honoured. (Same-user processes can
read the owner's file anyway; this is a guard against accidents, not a
boundary — see `RESOURCE_OWNERSHIP.md`.)

The Runner refuses any HTTP `Host` other than the one it serves on
(localhost), answering 404 — a DNS-rebinding guard. The CLI uses the endpoint
URL exactly as written; if you see "the Runner refused the request's Host",
you are reaching it under another name or address.

## Power verbs

A verb's flags may come before or after its other arguments (`send review
--steer text` as well as `send review text --steer`). A `--` ends the flags:
every word after it is an argument, even one that looks like a flag, so
`send review -- try -runner=x` sends that text and never changes the Runner.
Put `--` before text or an ID that may begin with `-`.

| Verb | What it does |
|---|---|
| `ls [--all]` | Workspaces, their conversations and agents with live state (needs input, failed, working, review, idle, ended), sub-agents still running. `--all` lists every registered Runner, grouped by Runner. Inside an agent: its own commands instead, running first (a server in the background with its URL), then the last five that ended. |
| `inbox [--all]` | What needs you, in order: needs input › failed › working › ready for review, with the `approve`/`deny` line for each pending approval. |
| `runners` | Registered Runners, live or not, and which one the CLI uses. |
| `send <conversation> [text\|-]` | Send a message. Text from the arguments, or stdin with `-` or when piped. An ended conversation is resumed first. `--steer` steers the running turn. |
| `open <workspace\|conversation\|file[:line]> [--panel KIND] [--workspace W]` | Show it in the Lazybox window in front (desktop preferred over a browser tab). A file opens in its workspace: `--workspace`, the terminal's, or the only one. |
| `workspace export-definition [WORKSPACE]` | Print a reusable collection.v1 definition. |
| `workspace realize FILE --key REQUEST [--name NAME]` | Create worktrees from a definition. Repeat the request key to recover the same workspace. |
| `workspace rename "DISPLAY NAME"` | Rename the current workspace in the sidebar. An agent can rename only its own workspace. |
| `workspace repos [WORKSPACE]` | A workspace's repositories (name, path, branch) and the requests agents made that still wait. Inside an agent or a workspace terminal, its own workspace. |
| `repos` | Every repository and the person's home checkout of it, with its branch and changed files ([WORKSPACES.md](../../../../../WORKSPACES.md#home-checkouts)). An agent sees its own workspace's repositories. |
| `repo home <name\|owner/repo> [--create [--path P] \| --adopt PATH \| --forget \| --update]` | Print a repository's home checkout (`cd "$(lazybox repo home api)"`; exit 3 when it has none). The person can create one (a clone at the suggested place or `--path`), adopt a checkout they have, forget it (no file changes), or `--update` it: fetch, then fast-forward the default branch only when it is checked out and clean, else say why. |
| `workspace request-repo <name\|owner/repo\|path> --reason WHY [--no-wait] [--timeout 90s]` | For agents: ask the person to add a repository to this workspace instead of reaching outside it ([WORKSPACES.md](../../../../../WORKSPACES.md#when-an-agent-needs-another-repository)). Matched against the catalog; adds nothing itself. Waits for the answer: the new worktree's path (exit 0), declined with the reason (exit 4), no answer yet (exit 5, the answer comes later as a message in the conversation). A timeout ends only the CLI wait. Check current status with `lazybox workspace repos` before retrying. |
| `review add --file <repo>/<path> --line N[-M] --severity blocker\|major\|minor\|nit --title T [--body B\|-] [--key K]` | For a reviewer agent Lazybox started ([GITHUB.md](../../../../../GITHUB.md#agent-review)): file one finding, anchored to the lines as the file is now. It lands in the person's Review tray, from that agent, for them to accept or dismiss. The same file, line and title (or `--key`) again updates it. Refused from a conversation that reviews nothing. |
| `review list` | The review this conversation takes part in and its findings, with ids, severity, where they point now and what became of them. |
| `review resolve <finding-id> --note WHY` · `review reopen <finding-id> --note WHY` | For the reviewer, in a later round of the [review loop](../../../../../GITHUB.md#the-review-loop): its finding is fixed, or still open (back to pending, to reach the author again). Only its own review's findings. |
| `review cancel [<review-id>] [--workspace W]` | Yours (the person's credential; an agent is refused): [cancel a review](../../../../../GITHUB.md#cancelling-a-review) under way, as its card's **Cancel review** does. No further round starts, the reviewer's turn is interrupted and its session ended; its conversation and findings are kept. Without an id, the one review under way in the workspace (`--workspace`, or the one the shell works in). |
| `ask author\|reviewer "question" [--timeout 90s] [--no-wait]` | Between the author and the reviewer of one review: the question goes to the other conversation as a labelled message; its reply prints here (exit 0), or comes later as a message in this conversation (exit 5 when the wait ends first). Refused for anyone else, while the other side waits on your question, or over the limits. |
| `link <pull request or issue URL> [--role delivers\|mentions] [--workspace W]` | Track a pull request this workspace delivers ([GITHUB.md](../../../../../GITHUB.md#tracked-pull-requests)): what an agent runs when it opened one in a way Lazybox could not see. A pull request URL is delivered (tracked, its transitions reach the Inbox and hooks); `--role mentions` only lists it. An agent's credential links to its own workspace and conversation only; it never unlinks, reacts or merges. (`link` without a URL is the installation verb below.) |
| `items [--workspace W \| --all]` | The workspace's tracked pull requests: state, checks with the failing ones' links, review decision, conflicts, how each was found, and what the workspace only mentions. Inside an agent, its own workspace; `--all` (the person's credential) every workspace's, deleted ones' history included. |
| `new <name> --repo R [--repo R…] [--branch B]` | Create a workspace from catalog repositories (`owner/name`, catalog name, identity or path). Progress on stderr, the new id on stdout. |
| `approve\|deny <request\|conversation>` | Answer a pending approval, by request id (or its suffix after `:`), or by conversation when it has exactly one. Questions are answered in the app. |
| `stop <conversation\|command> [--agent ID] [--end]` | Interrupt the running turn; `--agent` stops one sub-agent; `--end` ends the conversation (resumable). A command id cancels that command (SIGTERM to its group, SIGKILL after 5s), a `lazybox run` in the background included. Inside an agent: only its own commands. |
| `conversation new --title T (--brief B\|-\|--brief-artifact ID) [--workspace same\|new] [--provider P] [--profile ID] [--model M] [--reason WHY] [--timeout 90s] [--no-wait]` | For agents: ask the person to start a conversation, when they ask you to ([guide](../../../../../docs/guides/orchestrator.md#ask-an-agent-to-start-another-conversation)). The request waits in this conversation, the Inbox and on a paired phone: **Start**, **Edit…** or **Deny**. The new conversation is a peer, in this workspace or (`--workspace new`) one with new branches from it; its first message is the brief (an artifact's, with `--brief-artifact`) and who asked for it. Waits for the answer: started, with its conversation id (exit 0), declined with the reason (exit 4), no answer yet (exit 5, the answer comes later as a message). At most 3 wait at once per conversation. With an Orchestrator grant in force it starts at once, as an agent of the mission. |
| `agents spawn --title T [--brief B\|-] [--workspace same\|new] [--permission P] [--profile ID] [--role R]` · `agents ls` · `agents send <agent\|parent> "…"` · `agents wait [agent…] --until settled\|idle\|needs-input\|reported\|closed [--any] [--timeout 10m]` · `agents watch` · `agents stop <agent> [--end]` · `agents report "…"` | For an agent the person made an Orchestrator ([guide](../../../../../docs/guides/orchestrator.md)): start agents for its mission within the grant's ceiling, and drive only those; for an agent it started: talk to its parent and report its task. Without a grant, `spawn` asks the person to start it, as `conversation new` does; past the caller's lineage, refused (exit 4); above the ceiling or over the limits (exit 1, saying why: 4 KB, six messages a minute to one agent). `wait` exits 5 on timeout; the agents keep running. Never approves, answers or accepts. |
| `mission plan <file\|->` · `mission approve <mission> [--plan ID] [--cap $] [--wip N] [--note …]` · `mission reject <mission> --note …` · `mission budget <mission> --cap $` | An orchestrator proposes its plan (YAML or JSON: summary; tasks with key, title, brief, role, permission, workspace, paths, estimate_usd, after); the person approves it with a cap and a fan-out, rejects it, or raises the tree's budget. An agent's `approve`/`reject`/`budget` is refused (exit 4). |
| `lease claim <path> [--exclusive] [--ttl 2h] [--task ID] [--reason …]` · `lease release <id\|path>` · `lease ls` | An agent's advisory lease on the paths its task changes (an orchestrator's, also for its agents' tasks). A claim meeting another task's exclusive lease is refused, naming it; an edit inside one anyway is recorded as an overlap after the turn. Nothing is locked. |
| `wait <conversation> --until idle\|needs-input [--timeout 30m]` | Block until reached (exit 0); exit 5 on timeout; exit 1 if the turn failed, or ended without asking when waiting for input. |
| `watch <conversation\|workspace> [--since CURSOR\|DURATION] [--tail N]` | Follow events live as a timeline: time, agent or sub-agent, event, short text; streamed message deltas are folded into one line. Shows the last `--tail` (20) events first; `--since 0`/`--since 10m` replays from a cursor or a time. A workspace follows every live conversation and picks up new ones. `--json` prints NDJSON, one record per journal record (deltas included, `replay: true` for history). Colour only on a terminal, never with `NO_COLOR`. |
| `debug <conversation> [--events N]` | Diagnosis from the journal and the Runner's state: busy/queued/fault/status, pending requests, sub-agents, the last N lifecycle events (turns, accepted operations, receipts, failures, re-attachments, agent status) and findings in plain words — "the last turn ended by an interrupt you sent at 16:59", "fault X blocks messages", "a message has been queued behind the running turn for 5m", "no event for 12m while a turn runs". |
| `missions [ls\|show ID\|new TITLE --brief B\|accept ID TASK\|reject ID TASK --note N]` | Missions and their tasks; accept or reject work at a task's gate. |
| `shot [window\|PANEL] [--out FILE]` | PNG of the window or one panel, taken by the desktop app (a browser tab cannot). Default `./lazybox-shot-<time>.png`; `--out -` writes to stdout. |
| `actions [ls\|validate [FILE]\|schema\|path\|init\|run ID [--conversation C]]` | Your actions and hooks file (`POWER.md`): list what loaded and its problems by line, check a file locally (exit 2 on errors), print the JSON Schema, create the file, or have the window run an action. |
| `hooks [ls\|runs\|test ID\|log RUN]` | Your hooks, their recent runs, a test run with its output, a run's output. |
| `do <action> [argument] [--workspace W] [--conversation C]` | Run any action of the app's registry in the open window, as its key or the palette would (`do layout.maximize`, `do settings.open keyboard`); the id of one of your own actions runs it. Exit 3 for an unknown id (with the closest one), 6 with no window, 1 when the window cannot run it now. A hook with the `respond` grant may run only the actions marked safe. |
| `do --list [WORDS…]` | The registry by group: id, title, default keys, argument, and `safe` for the actions a hook may run. It is `internal/power/actions.manifest.json`, generated from `actions.ts` (`npm run generate:manifest`). |
| `config [path [FILE]\|edit FILE\|validate [FILE…]\|get FILE [KEY]\|set FILE KEY VALUE [--remove]\|export [--out F] [--actions]\|import F\|- [--dry-run]]` | Your config files (`POWER.md`, "Config files"): `keys`, `settings`, `actions`, `layout NAME` (or a path). Works on the files with the Runner's own parser, so it needs no Runner (one that answers names the folder). `set` keeps comments and writes nothing when the change would add a problem; `validate` prints `file:line` and exits 2 on an error; `edit` opens `$VISUAL`/`$EDITOR`; `export`/`import` move every file as one bundle, checked first, a replaced file kept as `.bak`. |
| `extension new <id> [--name N] [--example agent-board\|cost-dashboard]\|validate [<id>\|PATH…]\|list\|path` | Sandboxed panel extensions (`EXTENSIONS.md`) in the config folder's `extensions/`. `new` writes a starter with `lazybox.d.ts` (or copies a bundled example) and never replaces one; `validate` reads each folder with the Runner's parser and prints `file:line`, exit 2 when one does not load; `list` asks the Runner what each may do and whether it is on. Approving happens in the app. |
| `completion zsh\|bash\|fish` | Shell completion, generated from the same table as the verbs, with conversation, workspace, action and hook names asked of the Runner. |

The flag forms the test client always had keep working: `send --session S
--text T`, `open --cwd D`, `approve --request R --decision accept`, `wait --id
COMMAND`, and `events`, `exec`, `read`, `cancel`, `output`, `close`,
`continue`, `answer`, `interrupt`, `info`, … (`lazybox` with no arguments
lists them).

### Exit codes

| Code | Meaning |
|---|---|
| 0 | done |
| 1 | failed (the Runner refused, the turn failed, the hook failed) |
| 2 | usage or validation error (bad flags, an ambiguous name, an invalid actions file) |
| 3 | not found (conversation, workspace, request, action, hook) |
| 4 | not allowed: a scoped credential (agent, terminal, hook) or a refused token |
| 5 | timed out (`wait`, a window that did not answer) |
| 6 | unavailable: no Runner answers, none is registered, or no Lazybox window is open for `open`/`shot`/`actions run`/`do` |

### An installed release

These act on the release this `lazybox` belongs to (`INSTALL.md`); a Dev or
Working build says so and exits 6.

| Verb | What |
| --- | --- |
| `--version`, `version` | The release; app, CLI and Runner carry the same one |
| `service [status]` | Installed? loaded? running (pid)? answering, with which version? Exit 6 when it is not answering |
| `service install` | Write the launchd job (`~/Library/LaunchAgents/ai.lazybox.runner.plist`) and start it; on a loaded job, replace it: the old Runner detaches its agents, the new one re-attaches them. Also refreshes the PATH agents get, from your login shell |
| `service start`, `stop`, `restart` | Load or kickstart; unload (until the next login); a fresh Runner. Agents keep running throughout |
| `service logs [-f] [--lines N]` | `~/Library/Logs/Lazybox/runner.log` |
| `service plan` | The plist `install` would write, written nowhere |
| `service uninstall` | Unload and remove the job and the CLI locator; data stays |
| `doctor` | Release, service, endpoint (and its mode), versions, Node, Claude worker, Claude Code, Codex, git, gh, GitHub connection, disk, `lazybox` on PATH; each with its fix. Exit 1 when a check fails |
| `link [--dir DIR] [--shell S]` | Link this CLI into `/usr/local/bin` (when writable without sudo) or `~/.local/bin`, with completions; never replaces another program without `--force`. Says when another `lazybox` comes first on the login shell's PATH (an older Lazybox from Homebrew, another install, a version manager's shim, another program), with the fix; removes a `lazybox-v2` an earlier build linked |
| `link --remove-older [--yes] [--dry-run]` | Remove an older Lazybox installed by Homebrew that comes first on PATH: `<prefix>/bin/brew uninstall lazybox`, after asking (or `--yes`); refuses anything else |
| `uninstall [--dry-run] [--remove-data [--yes]]` | Remove the service, links and completions it created; with `--remove-data`, also state, workspaces, logs and the app's window state, after typing `delete` |

### Runners on other computers

The owner link over SSH (`REMOTE_RUNNERS.md`, P0;
`internal/link`). On this computer:

| Verb | What |
| --- | --- |
| `remote add <ssh-host> [--name N] [--bundle FILE [--sha256 HEX]] [--service auto\|none] [--progress json]` | Probe the host over your own `ssh` and `~/.ssh/config`, upload and install the Runner when it has none (the host checks the SHA-256 before unpacking), start it, add this computer's key to its owners and pin its Runner key; then open one link to check it. `--progress json` prints one line per step (the app's *Add a computer*) |
| `remote ls`, `remote rm <name> [--keep-on-host]`, `remote hosts` | The computers added, with the link's state and round trip; forget one (and revoke this computer's key there); the `Host` entries of `~/.ssh/config` |
| `link connect <name>` | The link agent: keeps a link to that Runner (redialling after a drop, a sleep or a network change) and serves it on a loopback origin with a per-start token, in `~/.config/lazybox/remotes/<name>/endpoint.json`, which its locator names: `lazybox --runner <name> …` then works as for a local Runner. `SIGUSR1` makes it recheck the link at once |

On the host, run by `ssh` as your own login:

| Verb | What |
| --- | --- |
| `link serve --stdio` | The link endpoint on stdin and stdout: Noise IK with this Runner's key, an enrolled owner key and its identity signature, then the Runner's RPCs proxied to the loopback Runner with the bearer read here. Never a listener; refused from inside an agent, a workspace terminal or a hook |
| `link owners [ls\|add\|revoke KEY-ID]` | The client keys with owner access (`<state>/owners.json`); a revoke ends that key's links within a second |
| `link up [--service auto\|none]` | Make sure the Runner runs: the registered one, else the release's launchd service, else started detached for this login |

`lazybox schema [actions|process|workspace|keys|settings|layout|theme] [--write DIR]` prints (or
writes, for an editor) the JSON Schemas of the files a person writes. They
ship with the CLI and are identified by `urn:lazybox:schema:…` ids, never a
web address; a process file names `process.schema.json` beside it.

`LAZYBOX_SERVICE_LABEL` selects another launchd label, with state, logs and a
locator of its own: the release smoke test installs a test service that way.

### Completions

```sh
lazybox completion zsh  > "${fpath[1]}/_lazybox"            # or: source <(lazybox completion zsh)
lazybox completion bash > ~/.local/share/bash-completion/completions/lazybox
lazybox completion fish > ~/.config/fish/completions/lazybox.fish
```

## Debugging an agent headlessly

An agent (or you, from any terminal) can drive and observe Lazybox without the app:

```sh
lazybox ls --json | jq '.workspaces[].conversations[] | {title, state}'
lazybox send "Alpha/fix the tests" "Run the suite again"
lazybox watch "fix the tests" --json | jq -c 'select(.type=="turn_ended")'
lazybox wait "fix the tests" --until idle --timeout 20m && lazybox debug "fix the tests"
```

## Security

Owner verbs need the owner's endpoint (`<state>/endpoint.json`, 0600). An
agent's credential reaches only its own console (`run`, `log`), its own
commands (`ls`, `stop`) and its own workspace's artifacts (`artifact`); a
workspace terminal's only its own runs;
a hook's only what its file grants (`read`, `respond`, `start` — what a paired
phone with those grants may do, less `Info`). None of them gains a verb from
this CLI: the Runner enforces the allowlists (`internal/transport/auth.go`,
`hook_auth.go`), and every new RPC is owner-only.

## Other verbs

In a Lazybox workspace terminal no flag is needed: `cmd 2>&1 | lazybox log
[--title CMD] [--to CONVERSATION]` and `lazybox run [--to CONVERSATION] --
cmd` publish to the workspace's console with the terminal's own credential.
See `TERMINALS.md` and `COMMAND_OUTPUT.md`.

`lazybox run` moves a server or watcher to the background once it has run
20 seconds and listens on a TCP port or looks like one (`npm run dev`,
`docker compose up`, `--watch`): it returns at once with the command's id,
URL, latest output and output file, and the command keeps running and
streaming to the console. `--wait` keeps it in the foreground until it exits
(`--wait=2m` waits that long first); `--background` moves it as soon as it has
started. `lazybox stop ID` stops it (`COMMAND_OUTPUT.md`, "Servers and
watchers").

`lazybox artifact put|get|list` publishes and reads workspace artifacts
(`ARTIFACTS.md`).

`lazybox run --check CRITERION -- cmd` records the run as the check of a
delivery criterion (exit 0 passes, any other exit fails), and `--needs
CRITERION` as a prerequisite (a non-zero exit blocks it). Both wait for the
result (no `--background`) and name the files they ran on, uncommitted
changes included; `lazybox ls` repeats what each recorded. A run is checked
against its Mission task's approved contract, or against the spec.v2 version
`lazybox delivery bind <spec-id>@<version>` pinned to the conversation;
without one it is refused before it runs (`docs/guides/missions.md`,
"Checking delivery criteria"). `lazybox delivery status [--json]` shows the
verdict, what blocks it and each criterion's evidence (`--mission ID --task
ID` from your own terminal); `lazybox delivery publish [--id ID]` keeps it as
a versioned report artifact; `lazybox delivery review <criterion>
passed|failed` records a contract-bound reviewer's verdict.

`lazybox process validate|show|explain EVENT|events|schema` checks the
orchestration process file (`.lazybox/process.yaml`, see
`ORCHESTRATION.md` §4) from files alone; with `--workspace` it asks the
Runner. It is implemented in `internal/process`.

`lazybox review verify --name NAME --member REPOSITORY --expected TEXT [--regression TEXT] -- COMMAND [ARGS...]` registers a frozen acceptance check in the caller’s active Behavior verification review. It returns the base and candidate managed-command IDs as JSON. `lazybox review list --json` includes recorded checks and current results.

`lazybox review test --member REPOSITORY [--name NAME] [--expected TEXT] -- COMMAND [ARGS...]` is how any reviewer runs tests, builds and UI gates: its own shell is read-only, so a command that writes a cache, a build or a temporary file fails there. The Runner runs the command in a disposable, writable checkout of the review's captured candidate (the same isolation as `review verify`, candidate only; loopback network for the servers a gate starts), never in the author's worktree, and records the result in the review. Two run at once per review; the same name in a round returns the run already made. It prints the check with its candidate command ID as JSON; `lazybox wait --id ID` gives its exit status and `lazybox read --id ID` its output (or the command tool's `wait` and `read` actions).
