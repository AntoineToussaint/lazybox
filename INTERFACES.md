# Generic agent interfaces and RPC

Status: the local Runner contract is implemented in `services/runner/proto/api.proto` and `services/runner/code/internal/agent`. The shape below remains explanatory design material; [Running](RUNNING.md) and [runtime evidence](RUNTIME_EVIDENCE.md) describe the current implementation and limits. Distributed Hub/Runner behavior remains proposed. The selected service language is Go. Claude and Codex each implement a small provider adapter; session supervision and application coordination remain generic.

Local continuity additions: session database version 2 retains a stable Runner
identity and per-conversation title metadata. `ListHistory` accepts optional
workspace scope and literal title search before pagination; `RenameConversation`
updates presentation across attempts without changing provider state. The private
endpoint locator carries `runnerId` separately from the renewable bearer and is
replaced atomically. Native personal storage uses this identity; it is not a
remote authentication or enrollment credential. Older database binaries must not
open version 2 state.

`ListWorkspaceChanges` and `ReadWorkspaceDiff` resolve registered workspace
members through the collection owner. The injected `gitinspect.Inspector`
boundary performs bounded, read-only local inspection. Requests carry workspace,
member and relative changed-file identity, never a caller-selected absolute root.
Per-repository errors, binary previews and truncation remain explicit on the wire.
`SearchWorkspaceFiles` and `ResolveWorkspacePath` work on a workspace's files the
same way: a bounded, briefly cached index of what `git ls-files` shows in each
member (ignored files excluded) and existence checks through `os.Root`, so neither
returns a path outside the workspace. Resolution returns every match of an
ambiguous reference for the client to offer as a choice.
The PR loop's writes (`CommitWorkspaceChanges`, `PushWorkspaceBranch`) resolve
members the same way and go through `gitpublish`, the only package that writes a
member's history; pull requests go through `github.PullRequests`. See
[the PR loop](GITHUB.md#the-pr-loop). Inline review comments
(`ListReviewComments` and the writes beside it) take the same workspace and
member identity, are stored by `internal/runner/reviews` and anchored against
the working tree on every read; see [review comments](GITHUB.md#review-comments).

## Responsibility boundaries

| Generic Runner engine | Provider adapter |
| --- | --- |
| Execution identity, command ordering, queueing, and persistence | Provider startup/attach handshake and configuration mapping |
| Participant attribution and scoped authority | Native session, turn, request, and tool identifiers |
| Command receipts and bounded event journal | Provider messages and structured event parsing |
| Pending question and approval registry | Actual protocol replies to those pending requests |
| Output spooling, log publication, replay, and viewer subscriptions | Provider shell-tool integration and capability reporting |
| Timeouts, cancellation requests, and restart policy | Provider interruption, shutdown, and resume behavior |
| Conformance tests and observable failures | Provider-specific errors and retained raw evidence |

The Hub owns shared work and decisions. It issues operations to the generic Runner engine, which invokes the appropriate adapter. Agent-specific parsing and flags must not spread into Hub, Desktop, or command-console code.

## Provider use modes

Confirmed requirement on 2026-10-29: Claude Code and Codex can power ordinary
LLM-backed application features as well as coding-agent sessions. The provider
boundary belongs to the backend; Help, planning, and other client modules use
that shared boundary rather than spawning CLIs or resolving credentials.

| Use | Context and result | Execution boundary |
| --- | --- | --- |
| Help | Relevant product documentation, effective app configuration, question, and streamed answer. | A bounded assistant conversation; no Collection provisioning. |
| Planning | Objective, supplied repository/Collection facts, discussion, and proposed plan. | Planning output remains a proposal; applying it is explicit. |
| Summaries | Selected changes, messages, or evidence references and a generated explanation. | Evidence references retain their source/revision context. |
| Coding agent | A Collection, its worktrees, conversation, and explicitly available execution tools. | Uses the existing coding-session and command lifecycle. |

Share provider selection, native account resolution, streaming, cancellation,
follow-up support where available, usage observations, and failure reporting.
App-wide assistant requests need no repository selection; repository-aware
ones can reference a Collection. The Collection requirement applies to
repository-working agent sessions rather than every model interaction.

Keep tool exposure deliberate for each purpose. Answering a help question does
not implicitly enable shell execution, file editing, worktree creation, or
configuration changes. Preserve current provider permissions and the selected
subscription/account. Automatic account selection requires an explicit same-provider rotation pool;
there is no implicit API-key fallback. `OpenAssistant` now implements Help,
planning, and summary conversations without a repository or Collection. It
uses the same Engine, provider adapters, streaming journal, cancellation,
follow-ups, usage and account supervision. A Runner-owned neutral directory
provides the native process cwd. Product Help context comes from
`internal/runner/assistant/context.md`; caller-supplied context is bounded to
128 KiB. The **Ask Lazybox** panel (Help → Ask Lazybox…, ⌘K, `]] h`) uses this
RPC; it picks its profile itself from `GetProviderAvailability`.

`GetProviderAvailability` (owner only; on no agent, terminal, hook or device
allowlist) reports, for each provider in a fixed order, its program — found or
not, path, version from `--version`, source (`configured`, the Claude SDK's
`bundled` build, or the Runner's `path`) and, in plain words, why it cannot
run (a missing Claude worker or Node, a program that does not start, a Claude
Code installed after the Runner started) — plus an install command when it is
missing, and every saved subscription with its state: `available`,
`unverified` (no limit observed and a credential that cannot be checked, such
as a keyring), `limited` with its reset, `sign_in` or `billing`, best first.
The sign-in check (`accounts.CheckSignIn`) resolves a profile as an execution
would and reads only file presence and account metadata, never a token.
Programs are probed once and again on `refresh`; subscription states are read
on every call. The provider summary is `ready`, `limited`, `sign_in`,
`billing`, `no_subscription`, `not_installed` or `unusable`. Ask Lazybox uses
the person's last-chosen provider's best usable subscription, else the
other provider's, and lists what each needs when none is usable; the welcome
shows the same report. When the SDK's bundled Claude Code is missing, the
Runner runs one found on its PATH at startup.

Assistant execution is narrower than a coding session: managed commands and
generic file references are disabled. Native images remain supported. Claude
receives no built-in tools and strict MCP configuration. Codex uses read-only
permissions with shell, external MCP/plugins, apps, sub-agent, goal and image
file tools disabled; its remaining native tool catalog is checked by the
installed app-server/local gateway fixture. Coding startup still inherits
native permissions unless explicitly overridden. No native settings file is
rewritten.

The Help assistant (and only it) can change Lazybox's own agent settings. It
gets two Runner tools — Claude sees them as the in-process `lazybox_settings`
MCP server (`get_settings`, `update_settings`), Codex as the dynamic tools
`lazybox_get_settings` and `lazybox_update_settings`. Reading returns the
Settings → Agents & workspaces fields and their revision, never credentials,
accounts or prices. An update names that revision, a reason and only the
changed fields (`naming`, `permission`, `context`, `claudeContext`,
`codexContext`); the Runner validates it like the Settings page and refuses a
stale revision, so it applies at most once. Every update is put to the user as
an approval card showing the reason and current → proposed values, whatever
the Runner's default permission: Claude runs the help assistant in `default`
mode with an `ask` rule, and its worker runs an update only for an input the
user allowed; Codex dynamic-tool calls always surface as Runner approvals.
Snippets are not changeable: the Runner's snippet catalog is a read-only
adapter. Coding agents, planning and summary assistants never get these tools.

It also reads the user documentation (docs, the pages Help → Documentation
shows) through two read-only Runner tools that never ask: Claude's
`search_docs` and `read_doc` on the same `lazybox_settings` server, Codex's
`lazybox_search_docs` and `lazybox_read_doc` (`internal/runner/docstool`).
They serve Markdown pages inside the configured docs folder only (never
`docs/internal`, nothing a link leads out to), and the help briefing lists
every page by path and title. The folder is `docsDirectory`, else
`LAZYBOX_V2_DOCS_DIR`: an installed release ships `runtime/docs`; Dev and
Working pass their source's `docs/`. Without one, the briefing says so.

`GetRunnerSettings`, `SaveRunnerSettings`, and `SetModelPrice` expose mutable
Runner preferences under its resolved state root (`runner-settings.json`). Atomic
private-file replacement and revision checks prevent a stale window from silently
overwriting newer preferences. Invalid settings fail before publication. This store
is independent of provider auth and native CLI settings.

`ListThemes` and `CreateTheme` serve the user's theme files from the Runner's
themes folder (`<state>/themes`, or `themeDirectory`): each `.toml`/`.json`
file parsed to JSON, or its parse error with the line, plus a revision that
changes with any edit. The client validates them against its token catalogue
and polls while a user theme is in use, so edits reload live. `CreateTheme`
never replaces a file. Both are owner-only. See [Appearance](APPEARANCE.md).

Missions live under `<Runner state>/work`: one append-only, fsynced JSONL event
log per mission (`missions/<id>.jsonl`; a torn last line is dropped on open, a
damaged log is listed with its error and never written) and the prompt log
(`prompts.jsonl`, bounded and compacted to its newest half). Boards are derived
from the logs on every read; nothing else is stored. See
[orchestration](ORCHESTRATION.md#slice-1-as-built).

The Agents & workspaces tab configures workspace display-name suggestions,
default permissions, common Lazybox context, additional Claude/Codex context,
and whether new Claude sessions load the person's claude.ai connectors
(`RunnerSettings.claude_ai_connectors`, off by default; see
[Artifacts](ARTIFACTS.md#lazybox-artifacts-by-default)).
`RunnerOptions.instructions` adds context for one new conversation (32 KiB limit).
The effective context is common + provider + session instructions alongside
workspace facts; Claude appends it to its native system preset, Codex uses native
developer instructions. The resolved context is retained with the session and
reused on resume, without accumulating copies or adopting later settings edits.

Explicit `yolo` uses Claude `bypassPermissions`/`allowDangerouslySkipPermissions`
and Codex `never` approval policy with `danger-full-access`. It is available per
coding session or as an explicitly saved Runner default. The shipped default
remains unchanged native CLI behavior; `inherit` explicitly skips a Runner
permission override. Plan sessions cannot request YOLO; help/planning/summary
application assistants remain read-only regardless of the coding default, and
the Help assistant's settings updates always ask, even under a YOLO default.
Permission flags affect the launched process, never persisted CLI configuration.

Approvals answer with the provider's own scopes. A request's data carries an
`always` list (`id`, `scope`, `label`, `detail`) and `Approval.always` sends
one id back: Claude's are its `canUseTool` suggestions returned as
`updatedPermissions` at destination `session` ("this conversation") or
`localSettings` (the worktree's `.claude/settings.local.json`, "this
workspace"); Codex's are `acceptForSession`, the exec-policy amendment it
proposed (`acceptWithExecpolicyAmendment`, which Codex keeps for later
conversations) and proposed network hosts. Lazybox's `ask` permission adds a
tool-wide `ask` rule for Bash and the managed command tool, which outranks
allow rules and suppresses Claude's suggestions; for those two tools the
worker offers "this exact command" and "commands starting with …" for the
conversation and enforces them itself, conservatively (a command that chains,
pipes or redirects asks again). An id the request did not offer is refused
and the request stays open. `Approval.reason` goes with a decline: Claude gets
it as the deny message; Codex's decline has no text, so the reason follows as
a steer of the same turn. The `operation_accepted` event of an answer or
approval carries the response in its data (`response`: kind, decision,
always, reason, answers), which the conversation shows as one line. Settings
updates and YOLO are unchanged: the first always asks, the second never does.


`Session.status` and `Session.agents` expose provider-derived activity.
`done` means a completed turn; `closed` means the conversation was ended.
Root completion does not finish active children or clear their questions.
Lost streams yield `unknown`. Activity is bounded to 256 nodes with explicit
truncation. Child text stays out of the main conversation and child native IDs
cannot replace the root resume reference. Claude task/bookend/membership
signals and Codex collaboration/thread signals are normalized in their adapters.
Not every native child relationship is exposed by every provider version;
unknown parents remain attached to the root rather than guessed from text.

V1 is a reference: `HelpAsk` routes questions through
`Model::handle_help_question` / `start_help_run_command` into a structured
`StartAgentRun` and reuses the run for follow-ups. Relevant files are
`crates/tui/src/realm/components/help_ask.rs`,
`crates/tui/src/realm/model/inputs.rs`, and `crates/server/src/agent_runs.rs`.
This records the existing pattern, with no V1 implementation admitted or copied;
any reuse still requires its scoped audit.

## Minimal Go interface shape

This is a design sketch, not committed production API. Define the request/event types and their state transitions with the first implementation.

```go
type AgentAdapter interface {
    Open(context.Context, OpenSession) (AgentSession, error)
}

type AgentSession interface {
    Capabilities() SessionCapabilities
    Apply(context.Context, AgentOperation) (ProviderReceipt, error)
    Receive(context.Context) (AgentEvent, error)
    Close(context.Context) error
}
```

`OpenSession` distinguishes fresh creation from explicit resume using a stored provider reference. It carries the execution identity, working-directory reference, tool configuration, and scoped credentials. Its context controls establishment; the generic engine supplies a separate execution lifetime so canceling a caller's RPC does not accidentally kill the agent.

Include the selected account profile reference and its verified isolation mode in the execution context. Profile switching is a generic continuation operation over native resume or checkpoint handoff, not a provider adapter silently changing a global login. [Subscriptions](SUBSCRIPTIONS.md) specifies the identity and recovery requirements.

`AgentOperation` is a closed, typed union of message/start-turn, steer-active-turn, interrupt-turn, stop-one-sub-agent, answer-question, and decide-approval. Stopping a sub-agent names the Lazybox agent id; the engine resolves its native id from agent state and refuses one that has finished, and it is offered only where `Capabilities.stop_agent` says the provider can (Claude through the SDK's `stopTask`; not Codex). Switching the model (`ApplyRequest.model`) names one model token (an alias or an id) for the following turns and is offered where `Capabilities.live_model` says so (Claude through the SDK's `setModel` on a worker that reports the `model` feature; Codex as the `model` override of the next `turn/start`); otherwise `RestartWithModel` ends the agent between turns and resumes the same provider conversation with that model, as `RestartWithPermission` does for a permission mode. Both, like a permission change, are the owner's: a device's or an agent's credential cannot call them. The Runner records `model_changed` and `permission_changed` (text: the new value; status `live` or `restart`) in the conversation's journal, and the Claude worker records `command_catalog` (the agent's own slash commands with descriptions and argument hints, and its models) and `compacted` (trigger, tokens before and after). It is not an arbitrary method name plus an unvalidated JSON blob. Queueing follow-ups is an engine operation: the engine dispatches them at the permitted boundary rather than assuming each provider queues identically.

While a permissions restart is pending, the client retains its confirmation as
**Restarting…**, disables repeated confirmation and the old composer, and returns
keyboard focus only after adopting the replacement session. A rejected request
restores the existing composer and shows its error.

`ProviderReceipt` distinguishes actual provider acknowledgement from a successful local pipe write. Where a provider has no explicit acknowledgement, expose that weaker status. The generic engine durably records acceptance and result under its own operation ID.

`AgentEvent` is a typed union of session/turn lifecycle, message deltas, tool activity, questions, approvals, request resolution, usage where available, and failures. Provider identifiers remain attached. A sub-agent's own messages and tool activity carry its agent id (Claude: `claude-tool:<spawning tool_use id>`, Codex: `codex:<thread id>`, both prefixed `attempt-N:` by the rotation layer); its `agent_status` events carry, in their data, the spawning call's `toolUseId`, its brief (`prompt`), and where the provider reports them progress `usage` (tokens, tool uses, duration), a one-line `summary`, and the final outcome. `Receive` has one owner in the engine; several viewers subscribe to the resulting journal rather than racing to consume the provider stream.

`Close` closes the adapter connection and owned resources according to a documented provider lifecycle. Interrupting a turn, terminating an owned process, and releasing an attachment are distinct operations with conformance coverage. The final API must make those effects explicit.

## Adapter implementation choices

**Codex:** evaluate app-server's structured request/event protocol from a Go adapter. Preserve native thread, turn, and pending-request IDs. Pin and verify the actual version and capabilities.

**Claude:** evaluate the Agent SDK's streaming-input interface. If the supported SDK requires TypeScript or Python, use a small Runner-supervised worker speaking a typed local protocol to the Go adapter. Keep orchestration and journals in Go. A new unsupported reimplementation of an SDK's private wire protocol is not an assumed shortcut.

Both adapters run the same conformance cases. Capability reporting permits differences such as mid-turn steering or pending-request restoration without silently simulating them. Adapter code should be small because shared mechanics have one owner; no percentage-of-generic-code target overrides correctness.

Command capture is a separate generic service. Each provider proves how its tools publish output or invoke managed commands without bypassing permissions. Seeing a provider's tool summary does not establish access to the full live shell output.

## Proposed RPC stack

Use Protobuf service definitions and generated Go and TypeScript types/clients. The local Runner uses Codefly's `go-grpc` service: one native gRPC implementation, with its generated Connect listener dispatching through the same server and interceptors. The Hub and distributed control stream remain subsequent work. Maintain one schema with separate services for project work, agent operations, Runner control, and command output. Schema generation and compatibility checks belong in the build gate.

| Connection | Proposed mechanism | Reason |
| --- | --- | --- |
| Desktop or web to Hub | Connect unary requests plus server-streaming subscriptions | Typed commands and live events work within browser request/response capabilities. |
| iPhone to Hub | Same service schema; validate Connect transport with Expo networking | Share client semantics; verify streaming and app suspension on device. |
| Runner to Hub | Runner-initiated gRPC bidirectional control stream | The Runner can connect outward while receiving assignments and returning acknowledgements and observations. |
| Runner agent adapter to harness | Provider-supported local protocol or supervised SDK worker | A remote RPC framework does not replace a provider's own protocol. |
| Artifact and preview traffic | Authorized HTTP transfers and preview routing; WebSockets for application services that require them | Large blobs and application protocols have separate flow control from control messages. |

A browser client sends a message or approval using a unary RPC while it receives events over a separate server stream. The product interaction is bidirectional without requiring a browser bidirectional-streaming RPC. Connect's web client currently supports unary and server-streaming calls, not portable client/bidirectional streaming.

The Runner control stream carries bounded control traffic. Bulk logs and artifact transfers must not delay approvals, heartbeats, or cancellation; use separate requests/streams with independent queue budgets. gRPC flow control alone does not implement fairness or durable delivery.

The Runner initiates its control connection to support common remote deployment topologies. Enrollment, TLS, identity, reconnect, assignment reconciliation, and proxy timeouts remain explicit implementation work.

## Application guarantees above RPC

RPC request IDs and an open stream are insufficient for durable work. Define operation IDs, idempotent retries, actor attribution, expected revisions, execution/assignment identity, event cursors, replay gaps, and explicit outcomes in the application contract.

Recover streams from the committed journal or an authoritative snapshot. A transport acknowledgement is distinct from provider delivery and task completion. Canceling a subscription closes that subscription; canceling execution requires the corresponding authorized operation.

For the first experiment, a local test client can call the engine directly while the adapter contract is being proven. A generated unary-plus-server-streaming test path must then prove browser consumption. The complete Hub/Runner enrollment and distributed recovery model remains later milestone work.

## References

- [Connect introduction](https://connectrpc.com/docs/introduction/) describes Go HTTP integration and support for Connect, gRPC, and gRPC-Web.
- [Connect web client](https://connectrpc.com/docs/web/getting-started/) documents browser streaming limits.
- [Connect Go streaming](https://connectrpc.com/docs/go/streaming/) describes streaming behavior and infrastructure requirements.
- [Connect protocol](https://connectrpc.com/docs/protocol/) specifies protocol framing and HTTP requirements.

## Telemetry and metering

Implemented in `internal/runner/telemetry`, with separate `Store`, `Pricer` and
`ResponseParser` contracts. The adapter wrapper lives inside account rotation,
so every native process has an immutable account/attempt scope. Generic execution
and native permission settings remain unchanged. `GetTelemetry` selects exactly
one workspace or execution and returns aggregate groups, context snapshots and
the latest 100 request observations. Aggregates include the full retained ledger.

The built-in `metered` route binds an OS-assigned loopback port, assigns revocable
unguessable routes per native process, and streams bytes without rewriting prompts.
It forwards the selected CLI's authentication headers; none are stored in telemetry.
As in V1's `crates/server/src/proxy`, Codex requests with `chatgpt-account-id` go to
the ChatGPT backend; API-key requests go to the Responses API. Claude uses Messages.
The proxy rejects redirects and unsupported endpoints rather than forwarding
credentials to arbitrary destinations. Configured cloud-provider environments are
rejected instead of silently changing their destination. `meterUpstreams` can
configure explicit upstreams, but this is not a generic browser proxy or WebSocket
proxy. Metered Codex uses its existing HTTP Responses gateway adapter.

`<Runner state>/telemetry.db` is a private SQLite WAL ledger. Each HTTP request is
recorded before forwarding, then finalized before terminal stream bytes reach the
client. A restart marks unfinished rows interrupted. SSE framing handles arbitrary
chunks and LF/CRLF/CR. Claude cumulative stream counts merge within a response;
Responses totals require a terminal event. A bounded 2 MiB observation buffer
never blocks forwarding of larger responses; such usage stays partial/unpriced.
The meter stores counts/identities, never prompt bodies, headers or tool contents.

Metered sessions do not add SDK usage to gateway usage. Direct Claude observations
use stable message IDs; direct Codex lifetime counters are differenced atomically
against durable baselines. An imported resumed thread without a baseline establishes
one instead of charging its old history. Completed response IDs are deduplicated
across retries and SDK/gateway observations within a conversation/account. Distinct
provider responses from billed retries remain distinct. Unpriced or incomplete
requests remain visible, never a confident zero. Gateway traffic without a child
ID is attributed to the workspace/session/account with child attribution unknown.

Pricing is API-equivalent **token cost**, not a subscription invoice or quota.
The initial exact-model standard rate card is versioned `2026-11-02`, sourced from
[Anthropic pricing](https://platform.claude.com/docs/en/about-claude/pricing),
[GPT-5.4](https://developers.openai.com/api/docs/models/gpt-5.4) and
[GPT-5.3-Codex](https://developers.openai.com/api/docs/models/gpt-5.3-codex).
Unknown models, service tiers, incomplete usage, additional detected tool fees
and unsupported long-context pricing remain unpriced. Direct Codex usage is priced when the cumulative counter change matches the
latest request and its model is known. Unattributed or aggregated changes
retain their token counts. Codex context shows reported token types where
they add up to the context total. Claude's SDK reports semantic categories
such as system instructions and tools. Published long-context rates apply to
GPT‑6.2 Sol, GPT‑6.2 Astra, and GPT‑6 Sol. Newer cache-write protocols and
cloud/region discounts require provider parser/rate additions; they are not
assumed equivalent to the supported standard rates.

`meterPrices` overrides exact `provider/model` keys with a `version` and integer
nanodollars per token (`input`, `output`, `cacheRead`, `cacheWrite5m`,
`cacheWrite1h`, optional `maxInput`). A USD-per-million-token price is multiplied
by 1000 for these units. Prices are stamped into records at observation time;
changing a card does not rewrite already-priced history. **Settings → Pricing**
lists the rate card as one searchable, sortable table per provider and edits
exact model overrides in USD per million tokens (three decimal places) in a
dialog: **Add model price**, or a row's edit button. Removing an override asks
first.
The Runner assigns each edit a version; removing an override restores the configured
base/default rate. Captures use the latest rate, while reports can estimate retained
observations previously missing a rate, without rewriting those observations.
Incomplete usage, unsupported tiers and detected non-token fees remain unpriced.
A priced record keeps its cost by part (`costs`: input, output, cache read,
five-minute and one-hour cache writes). A report's group sums them when every
priced record says (one priced before parts were stored gets them from the
current rate only when that is the rate it was priced at, to the nanodollar),
and says of its unpriced records why (`unpricedReasons`, each once) and how many
tokens they leave out (`unpricedTokens`).
Fable 5.1 (`claude-fable-5-1`) was added from the Anthropic price table on
2026-11-03: $10 input, $50 output, $0.25 cache read, $12.50 five-minute cache
write and $20 one-hour cache write per million tokens. `defaultGateways` may explicitly set
`claude` or `codex` to `metered`; existing defaults remain unchanged.

Context is per agent/thread, never summed as one workspace context window.
Codex uses its structured token-usage notifications. Claude also queries the
pinned SDK's `getContextUsage({detail:'summary'})` after turns: category estimates,
capacity and compaction reserve are retained without memory paths or tool schemas.
Rows retain provider classification; deferred tools are outside the used window.
Unsupported CLIs retain available totals, with missing breakdowns marked unknown.
Snapshots are timestamped observations; they do not claim to update while no
provider event has occurred. Full per-category token-count API queries are not
silently invoked. The surfaces are consumers; closing them does not stop metering.
