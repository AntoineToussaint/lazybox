# ACP agents

Lazybox runs any agent that speaks the
[Agent Client Protocol](https://agentclientprotocol.com) as a first-class
conversation. This is how V2 covers "any CLI agent" without becoming a terminal
emulator (herdr re-audit, brief R3): a terminal pane can show an agent, but
only a protocol lets Lazybox put its permission requests on approval cards, its
tool calls in the chat's lines, its plan in the checklist, its edits in
Changes, its commands in the console and its cost in telemetry.

The person-facing guide is
[docs/guides/other-agents.md](docs/guides/other-agents.md).

## What is spoken

- **ACP protocol version 1** (the stable schema, `schema/v1` in
  [agentclientprotocol/agent-client-protocol](https://github.com/agentclientprotocol/agent-client-protocol),
  schema 1.32 and SDK 2.7 when this was written; v2 is an RFD in alpha and not
  spoken). Version negotiation is strict: an agent that answers `initialize`
  with another version is refused with a sentence saying so.
- **JSON-RPC 2.0 over the agent's stdio**, one message per line, on the same
  `jsonl.Process` the Claude worker and the Codex app-server use, in a dialect
  (`jsonl.Options`): every message carries `"jsonrpc":"2.0"`, and a reply to no
  call of the Process is delivered to the reader instead of dropped.

**Status: Preview.** The adapter is exercised against Lazybox's fake agents
(`scripts/fixtures/acp-fake.mjs`) and one real agent on request
(`TestRealACPAgent`), not against each built-in; every surface that offers an
ACP agent (New conversation, the agent picker, Settings → Agents, the guide)
labels it Preview. The gaps behind the label are below: launch commands come
from the registry, not the installed version (checked, below, not enforced);
protocol v1 only; `mcpServers: []`; questions are not mapped.

Built-in agents (`adapters/acp/catalog.go`), found on the Runner's PATH:
Gemini CLI (`gemini --acp`), Google Antigravity (`agy acp`), Hermes Agent
(`hermes acp`), Goose (`goose acp`), OpenCode (`opencode acp`),
Qwen Code (`qwen --acp`), GitHub Copilot (`copilot --acp`), Cursor Agent
(`cursor-agent acp`), Kiro CLI (`kiro-cli acp`), Cline (`cline --acp`) and
Auggie (`auggie --acp`), each with its install command. Launch commands follow
the [ACP registry](https://github.com/agentclientprotocol/registry). Claude
Code and Codex are deliberately not built in: their native adapters do more
(sub-agents, steering, subscriptions and rotation, the metered gateway); their
ACP bridges (`claude-agent-acp`, `codex-acp`) can be added as custom agents.

## Add an agent

For an ACP v1 agent, install its executable on the Runner, then use
**Settings → Agents → Add an agent** to register its name and launch command.
The Runner creates the profile and the conversation picker discovers it.
The adapter negotiates optional capabilities during initialization and maps
them to the same conversation operations and events the built-in agents use.

To try the path without an account, register the checked-in
`scripts/fixtures/acp-fake.mjs` as a custom agent with the command
`node /absolute/path/to/lazybox/scripts/fixtures/acp-fake.mjs`. Start a
conversation with that agent and send a message. The fixture is also the
reference for the required handshake, response, turn completion, and the
optional capability and update shapes.

For a native integration, implement `agent.Adapter.Open` and register it in
the Runner's adapter map. `agent.Session` is the lifecycle contract: return a
stable native reference, declare capabilities, accept operations through
`Apply`, publish normalized events through `Receive`, and release resources
through `Close`. Return `agent.ErrUnsupported` for an unavailable operation.
Add tests for startup, a complete turn, cancellation and cleanup, and each
declared capability. The ACP tests below exercise the same contract through
an external process.

## Where it lives

| Part | File |
| --- | --- |
| The adapter (`agent.Adapter`) | `services/runner/code/internal/runner/adapters/acp/` — `acp.go` (open, attach, apply), `read.go` (the read loop, turn ends), `update.go` (session/update → events, tool-call shapes), `permission.go`, `client.go` (fs and terminals), `modes.go` (modes, models), `catalog.go` (agents, the registry) |
| Providers, profiles, adapter lookup | `internal/runner/acp_agents.go`; `accounts.ACPProvider`, `accounts.Resolve` |
| Availability | `internal/runner/availability` (`Probe.Agents`, `Probe.AgentProgram`, `Provider.ACP`) |
| Wire | `SaveAcpAgent`, `RemoveAcpAgent`, `ProviderStatus.acp` (`AcpAgentInfo`) in `proto/api.proto` |
| Cost and context | `internal/runner/telemetry/acp.go` |
| Client | `providers.ts` (names, icons, choices), `AgentPicker.tsx`, `AcpAgentsSettings.tsx`, icons in `Icons.tsx` |
| Fake agent | `scripts/fixtures/acp-fake.mjs` (Go tests and the UI suite), `acp-ui-fake.mjs`, `acp-journal.json` |

## The launch check

The built-in launch commands follow the ACP registry, not the version the
person installed, and an agent that renamed or dropped its ACP flag would
fail only when a conversation starts. When the Runner looks for agents
(`availability.Probe`, at start and on **Check again**), it runs each
installed built-in agent's `--help` (bounded, no input, `NO_COLOR`) and checks
that every argument Lazybox starts it with (`--acp`, `acp`) is named there as
a whole word (`acp.Agent.LaunchProblem`). A missing one is reported in
`AcpAgentInfo.launch_problem` and shown on the agent's row in Settings →
Agents with the command that updates it. It is a hint, never a refusal: help
text is free-form. A custom command is not checked (it may be a launcher such
as `npx`). Checked by hand on 2026-11-17: Gemini CLI 0.68.0 names `--acp`.

## Providers and profiles

An ACP agent is the provider `acp:<id>`. Each built-in agent installed on the
Runner's PATH, and each agent the person added, has exactly one profile,
`acp-<id>`, labelled with the agent's name, with `isolation: inherited`: the
agent runs on its own sign-in, with the Runner's environment plus the agent's
configured environment. `accounts.Resolve` refuses an ACP profile carrying
credentials of Lazybox's (a home, a token file, env references): Lazybox does
not hold ACP agents' accounts, so named subscriptions, rotation at a limit and
the metered gateway are Claude's and Codex's.

The person's agents are in `<state>/acp-agents.json` (0600). A custom agent with
a built-in id replaces that built-in's command; removing it restores the
default. Environment values never leave the Runner: the wire carries names
only, and an empty value on save keeps the stored one.

A built-in agent can share another's credentials (`Agent.SharesCredentialsWith`,
`SharedEnv`). Google Antigravity shares Gemini CLI's `GEMINI_API_KEY`. When
Antigravity's own environment lacks that name, the registry copies the value
from Gemini CLI's environment (`Agent.Inherited`, as private as `Env`). At
launch the shared values go first and the agent's own go after, so its own
value wins, and both come after the Runner's environment. The registry file
never records shared values. The wire carries only `shares_credentials_with`
and `inherited_env_names`, with `sign_in_command` and `credentials_note` for
Settings → Subscriptions.

Detection runs through `Config.ACPLookPath` (the service sets
`exec.LookPath`); left nil, only the person's agents are offered, so a test
machine's installed agents never change a test's providers.

## The mapping

| ACP | Lazybox |
| --- | --- |
| `session/new` (`cwd`, no MCP servers) | the conversation; `sessionId` is its reference |
| `systemPrompt` (when the agent advertises it) | the conversation's context; otherwise the context is the first message's leading block |
| `session/prompt` | a turn: `turn_started` before it is written, `turn_ended` when its reply arrives |
| prompt content | the person's text, then each attachment in order: an `image` block (only when `promptCapabilities.image`), a text file or pasted text as an embedded `resource` (when `embeddedContext`, else a fenced `text` block), anything else as a `resource_link` |
| stop reasons | `end_turn` → completed; `cancelled` → interrupted; `max_tokens`, `max_turn_requests` (budget), `refusal` → a failure, then the next message continues |
| a JSON-RPC error on the prompt | a turn failure; `-32000` is `auth_required` with an account observation |
| `agent_message_chunk` | `message_delta` (text; a resource link as a Markdown link) |
| `agent_thought_chunk` | `activity: thinking` — the text is never journaled |
| `tool_call`, `tool_call_update` | `tool_activity` with a normalized `tool_call`, merged per call |
| `plan` | the `update_plan` call of the turn (the checklist) |
| `available_commands_update` | `command_catalog` commands (the `/` menu) |
| config option of category `model`, or pre-standard `models` | `command_catalog` models; `LiveModel` via `session/set_config_option` or `session/set_model` |
| session modes, or config option of category `mode` | Lazybox's permission modes (below) |
| `usage_update` | context used and size, and changes in the agent’s reported USD cost recorded at the time observed (`telemetry/acp.go`) |
| `session/request_permission` | an approval card; `allow_always` options are its "Always allow" choices |
| `session/cancel` | Interrupt; waiting permission requests are answered `cancelled` first |
| `session/resume`, else `session/load` | resume; the history `session/load` replays is not journaled again: its reply is read in order with the replayed updates, so the replay ends exactly there |
| `session_info_update` title | the conversation's generated title (`agent.Titler`), when the agent names its session |

Tool calls take the vocabulary the client already reads (`tool-lines.ts`), by
ACP kind: `read` → `Read`, `edit` → `Write`/`Edit` with the changed region of
its diff (or `File changes` for several files), `search` → `Grep`, `execute` →
`Bash` with its command, `fetch` → `WebFetch`/`WebSearch`, `think` →
`Thinking`. A call that embeds a Lazybox terminal carries the command's id,
and once it ends its output tail and exit code.

## The client half, served by Lazybox

- **Files.** `fs/read_text_file` and `fs/write_text_file` are served from the
  conversation's workspace only: absolute paths, every symlink of the existing
  part resolved, nothing outside the workspace root and nothing written inside
  `.git`. Files the person attached may be read where the Runner keeps them.
  Writes are atomic and keep the file's mode. A read-only or plan conversation
  writes nothing.
- **Terminals.** `terminal/create` starts a managed command of the
  conversation (the `lazybox_command` tool, extended with a title, a folder
  inside the workspace and added environment): it is in the console, outlives
  the Runner and the person can stop it. A command line with shell syntax and
  no arguments runs under `/bin/sh -c`. `terminal/output` is the command's tail
  within the agent's byte limit on a UTF-8 boundary; `wait_for_exit`, `kill`
  and `release` (which stops a running command; the console keeps it) map to
  wait and cancel. A read-only conversation starts nothing (the Runner refuses).
- Nothing else is offered: no `elicitation`, no `session.notices`, no
  compaction updates, no `authenticate` (below); other client methods get
  `-32601`.

## Permissions

Lazybox's *Ask*, *Read only* and *YOLO* map onto the agent's own modes by name
(`yolo`/`bypass`/`full access`…, `read only`/`plan`…, `default`/`ask`…), set at
start and live. Whether or not a mode matched, Lazybox enforces the choice on
what the agent asks of it: YOLO answers permission requests with the agent's
allow option; Read only (and plan) allows `read`, `search`, `think` and `fetch`
requests and rejects the rest, writes no file and starts no command. Tools an
agent runs inside itself without asking are bound only by its own mode — the
one gap Lazybox cannot close from outside the agent.

A decline's reason has no field in ACP: it reaches the agent as a leading
block of the next message.

## Restarts

The agent runs in a session host like Claude and Codex
([SESSION_HOSTS.md](SESSION_HOSTS.md)). The durable state records the
in-flight `session/prompt` id and turn, the permission mode, modes, models,
the readable attachments and pending decline notes. The prompt is written with
an id recorded *before* it is written: the durable state always holds the id
the next `session/prompt` will use (`next`, from `jsonl.Process.NewID`), and
the engine saves it before each delivery. Its reply reaches the reader as an
ordinary line with a transport cursor (`Request`), so a turn that ends while
no Runner is attached ends correctly in the next one, and is never ended
twice. A Runner that died after writing the prompt but before recording it
running leaves `next` unrecorded: whoever attaches next may find that prompt
running, so it moves `next` to `orphans` (the last eight) and reserves a new
id. A new prompt never reuses an orphaned id; a reply to one ends its turn
when no prompt of this Runner runs, and is otherwise only forgotten (the
running turn and its requests are this Runner's). A permission
request the host redelivers is answered with the person's recorded decision
when they had answered (and recorded resolved, so its card clears), else
asked again. When the prompt's reply arrives, a permission request still
open is over: the adapter tells the host to forget it (never redelivered to
a later Runner) and records `request_resolved`. An answer whose write fails
leaves the request open, to be answered again. A message delivered while the
adapter still waits on a prompt's reply is not sent (`agent.ErrTurnRunning`):
the engine keeps it first in line and sends it when that turn ends.

Not covered: an orphaned prompt may still be running when the next message
goes. The agent may then refuse the new prompt (a turn failure the person
sees, their message kept in the conversation) or run both; Lazybox cannot tell
whether the orphaned prompt was ever written, so it neither waits on it nor
holds messages for it, which could leave the conversation waiting for a reply
that never comes.
A redelivered `terminal/create` is refused with an explicit "unknown whether it
started" — never run twice; terminal reads and waits are served again (the
commands outlived the Runner too).

## Gaps, by ACP's design or not yet built

- **Sign-in.** An agent that needs signing in fails to start with a sentence
  naming its auth methods; the person signs in in their terminal. ACP's
  `authenticate` (agent-driven sign-in) is not offered yet: it would open a
  browser on the Runner's computer from a click in Settings.
- **Steering and questions.** ACP has no mid-turn input and v1 has no
  structured question; messages queue for the next turn, questions arrive as
  prose. `elicitation` (forms) is a follow-up.
- **Sub-agents.** ACP has no sub-agent stream; an agent's own delegation shows
  as its tool lines.
- **Cost.** Only the agent's own cumulative USD figure is recorded; tokens are
  not reported by ACP, so the rate card cannot price them. A resumed session
  whose agent reports its lifetime cost may count earlier spending again.
- **MCP servers and Lazybox tools.** No MCP servers are passed; the agent uses
  the `lazybox` CLI through its shell (the briefing) rather than Lazybox tools.
- **Agents outside the PATH.** Detection is the Runner's PATH; an agent in a
  folder the service's PATH does not include is added by absolute path.
- **ACP v2** (`state_update`, multiple sessions per connection) is not spoken.

## Tests

- `go test ./internal/runner/adapters/acp` — the protocol against the fake
  agent: the tour (updates, approval, files, terminal, plan, usage, context),
  every decision, modes answering without asking, workspace scoping and
  symlinks, shell command lines, cancel and every failure, startup failures,
  models, resume without replay, a turn and a pending permission across a
  Runner restart under a real session host.
- `go test ./internal/runner -run ACP` — through the whole Runner: profile,
  host, journal, approval, files, the console, telemetry; read-only refuses
  commands; built-in detection and availability; and
  `TestACPJournalFixtureIsCurrent`, which keeps `acp-journal.json` equal to
  what the adapter journals (`LAZYBOX_UPDATE_ACP_JOURNAL=1` regenerates it).
- `npm run test:acp` — the UI on that journal: tool lines, plan, command,
  reply, the approval card with the agent's Always choice, Settings → Agents
  (with a failed launch check), the Preview labels, and the keyboard agent
  picker.
- `go test ./internal/runner/availability -run Launch` — the launch check
  against fake `--help` output: a missing flag, a subcommand, a bracketed
  flag, an unreadable help, a custom command. `scripts/acp-agents.test.ts` (in
  `test:client`) covers names, choices and command-line parsing.
- `LAZYBOX_V2_REAL=1 LAZYBOX_V2_REAL_ACP=gemini go test ./tests/integration -run TestRealACPAgent`
  — one real turn with a real, signed-in agent. Live and billable.
