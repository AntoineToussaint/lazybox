# Runner resource ownership

This is P03's first increment: supervised process groups and bounded admission.
It is not complete crash recovery or a containment boundary for hostile code.

## Process lifetime

Claude/Codex JSONL transports and Runner-managed commands use the shared
`internal/processguard` package. Each start re-executes the same Runner binary
as a small helper, with private lifetime and result pipes. The helper leads its
own process group; the target and ordinary descendants inherit that group.

The Runner keeps the lifetime pipe open. Cancellation or Runner death, including
SIGKILL, closes it. The helper terminates its own group. Normal target exit also
terminates remaining background descendants and records the target's exit
status. The helper pins the group identity while signalling it; cancellation
does not read a historical PID and signal whatever now occupies that number.
No global tmux server, process-name scan or global container cleanup is used.
(The optional tmux hosting backend runs a private server per Runner state
directory, on its own socket, and only for session hosts; see
[hosting backends](SESSION_HOSTS.md#hosting-backends-session-hosts-default-or-tmux).)

Waiting reaps the helper and consumes its outcome. Concurrent Stop/Wait calls
are idempotent. Missing cleanup evidence is an error, not a successful stop.
An output-drain failure also remains uncertain, including an escaped child
holding a descriptor open after the group is gone. The guard requires a plain
unstarted command; unsupported attributes, descriptors and cancellation hooks
are rejected rather than silently dropping their semantics. Cancellation must
go through the lifetime capability, not kill only the helper.
Provider closure retains the engine's existing `native_exit_unconfirmed` state.
An uncertain managed command keeps its admission slot and workspace ownership;
workspace/conversation deletion refuses to remove resources under that state.
Failed target startup stays distinct from an executed command's nonzero exit.

This prevents ordinary children from surviving their Runner and retaining a
gateway connection to a dead Runner generation. Agents and the managed commands
they start are the deliberate exception. Each runs under a guard inside a
[session host](SESSION_HOSTS.md), whose lifetime is the session's rather than
the Runner's; the next Runner re-attaches to it, and stops any host it cannot
claim. A host with no Runner for a day stops its program. A guard that is cancelled while its program is still
starting kills the program too, never leaving it outside the group. It does not
make a lost process reconnectable. Historical conversation recovery remains conservative and does
not automatically replay messages, approvals or provider effects.

## Unconfirmed ownership

A workspace records the sessions (conversations' attempts) that work in it. A
Runner that starts reads those records, re-attaches the agents it can
([session hosts](SESSION_HOSTS.md)), and is left with the ones it did not:
their **ownership is unconfirmed** ("Owner uncertain" on the workspace's row).
Nothing this Runner holds says their agent and commands stopped, and one that
still ran would be writing in the workspace with no Runner watching. So until
it is settled the workspace takes no new conversation, is not archived or
deleted, and the conversation is not deleted or resumed. A conversation can be
unconfirmed on its own too ("needs recovery"): a previous Runner left it open,
or one of its commands' stop was never confirmed.

`GetOwnership` (owner only) says what is unconfirmed, for a workspace or a
conversation: each recorded agent and command, its PID and command line where
recorded, and whether a process with that identity still runs, checked by the
Runner (`internal/runner/ownership.go`):

- A session host that answers with its secret token is the recorded one:
  **running**. One whose record is gone or that refuses connections is gone,
  and its program with it (processguard): **gone**.
- A host that does not answer is checked by its PID **and start time**
  (`internal/procid`, recorded in the host's record): the same start is the
  same process (**running**); a free PID or one with another start time (the
  system gave the number to another program) is **gone**. A record without a
  start time and a live PID is **unknown**: a PID alone is never an identity.
- A command that started before the computer last started is **gone**.
- No host left for an agent means its agent is gone: every agent runs in one.

What it proves gone it settles there and then, and the Runner does the same
for every unconfirmed session when it starts (`SettleProvenOwnership`, after
re-attaching): after the computer restarts nobody is asked anything.
Otherwise the person chooses (`ResolveOwnership`, owner only):

- **Stop them and continue** stops exactly the recorded processes that run:
  a host that answers through its own control connection, one this Runner
  holds as its owner would, and one that does not answer by `SIGKILL` only
  while its PID still has the recorded start time. Never a recycled PID, never
  a process-name match.
- **They're already stopped, continue** records the person's word, as
  `processes_stopped` does on resume. It is refused for a process the Runner
  can see running.

Settling records the resolution on the workspace (`lastOwnershipResolution`)
and on each unconfirmed command (`<id>.settled` beside its journal), and ends
a conversation the previous Runner left open as interrupted, with a notice and
its queued messages kept: it continues by sending a message, as after a reboot.
The older `ReconcileCollection` (one named owner at a time, with a reason)
remains for scripts.

## Agent credentials

The owner's `endpoint.json` holds the bearer that can do anything: approve,
answer, send, delete, change settings. No agent or managed command is given it.
Each execution instead gets `<state>/agent-endpoints/<execution id>.json`
(mode 0600: the Runner URL and a credential bound to that execution), named by
`LAZYBOX_V2_ENDPOINT`. Any `LAZYBOX_V2_*` the Runner itself inherited is
dropped from agent and command environments, so an owner endpoint exported in
the Runner's shell does not leak through either. So is the Runner's Codefly
service wiring (`CODEFLY*`: its Codefly credentials, its endpoints and its
container recovery marker), as from workspace terminals: an agent's own
`codefly` would otherwise refuse that marker as another process's, and the
agent would hold the Runner's Codefly credentials. The same holds for the account probes the Runner runs with the person's provider CLIs. The Runner cannot tell a `CODEFLY_*` the person exported (such as `CODEFLY_HOME`) from Codefly's own, so agents lose those too, as terminals do: set them in shell startup files, or per command.

The transport (`internal/transport/auth.go`, `agentRPCs`) accepts that
credential for a bounded set of RPCs, including the ones `lazybox run` and `lazybox log`
publish a console with (`RegisterPipe` for its own execution, and `Publish`,
`FinishPipe` and `FollowPipe` for pipes that execution owns); the ones
`lazybox ls` and `lazybox stop` use on the execution's own commands
(`ListCommands` for its own execution, without the workspace's runs, and
`CancelCommand` for commands it owns); the ones `lazybox artifact`
uses: reading any workspace's artifacts (`ListArtifacts`, `GetArtifact`),
writing only in its own execution's workspace (`PutArtifact`,
`RetireArtifact`); and the ones `lazybox workspace` asks for a repository with
(`ListWorkspaceRepositories` for its own workspace, `RequestRepository` for
its own execution, `WaitRepositoryRequest` for its own requests — asking,
never adding; see [workspaces](WORKSPACES.md#when-an-agent-needs-another-repository)).
The agent may also rename its own workspace display name with `RenameCollection`, pin a spec to its own conversation with `BindDelivery` (`lazybox delivery bind`), read and publish its own conversation's delivery outcome (`GetDeliveryOutcome`, `PublishDeliveryOutcome`, never by Mission task), and record its own review's verdicts with `RecordReviewCriterion` (refused unless it is that review's reviewer).
Every RPC outside the allowlist, including any added later, answers `PermissionDenied`; a test
walks the generated service description to keep that exhaustive. Comparisons are constant-time, and the credential never
appears in an error, journal or response.

The credential is an HMAC-SHA256 of the execution id under
`<state>/agent-credential.key`, created once and never replaced. It needs no
storage, so an agent that outlives its Runner keeps working: the next Runner
rewrites the file with its own URL when it re-attaches the execution. Deleting
the conversation revokes it (the deletion record is checked on every call) and
removes the file. A closed execution's credential can still finish a pipe it
already opened, but cannot open a new one.

A workspace terminal gets a credential of its own, built the same way under a
different label and bound to the terminal: it may only open runs in its
terminal's workspace and publish to and finish the runs it opened
(`terminalRPCs`; see [terminals](TERMINALS.md#what-the-shell-may-do-with-the-runner)).
Closing the terminal, its shell exiting, or deleting the workspace revokes it.

The agent browser ([BROWSER.md](BROWSER.md)) needs no credential: an agent
drives it through its own execution's tool pipe, and the Runner derives the
conversation from the execution, never from the call. Every browser RPC
(watching, Take over, input, answering its requests, its settings) is the
owner's. Its service is an ordinary Runner-owned child under the process
guard, with Chromium its descendant; it stops with the Runner, and the
profiles it keeps per workspace are deleted by Clear browser data.

A paired phone gets neither: its credential is issued by the device gateway
against a signature by the phone's own key, is accepted only on that gateway,
and only for the RPCs its grants allow ([remote control](REMOTE.md)).

A user hook ([power](POWER.md#hooks)) holds no credential at all unless the
actions file grants it one (`grants: [read, respond, start]`). Then the
Runner writes `<state>/hook-endpoints/<run>.json` for that run only: an HMAC
of the run id and its grants, accepted while the run is live and for exactly
the RPCs a paired phone with the same grants may call, less `Info`
(`internal/transport/hook_auth.go`; walked exhaustively by
`TestHookCredentialReachesOnlyItsGrantsWhileItRuns`). The run ending revokes
it and removes the file. Every power RPC (the actions file, hook runs, the
CLI's window requests) is the owner's alone.

This separates authority at the RPC boundary only, and **prevents accidents,
not a hostile agent**. An agent runs as the same OS user and can read the
Runner state directly: the owner's `endpoint.json`, `agent-credential.key`
(from which any conversation's credential can be computed), the saved API key,
other executions' endpoint files and the session hosts' control sockets. An
agent that misbehaves on purpose can therefore act as the person. What that
leaves, what the harnesses could deny and why they do not, and the design
that closes it (credentials in the Keychain, owner identified by a Unix
socket's peer rather than a bearer) are in
[SECURITY.md](SECURITY.md#what-an-agent-can-reach). An agent started before this change keeps the
owner path in its own environment until it ends: re-attaching cannot change a
running process's environment, though commands the Runner starts for it get
the scoped file. Continue or restart such a conversation to drop it.

## Admission configuration

The private Runner JSON configuration accepts:

```json
{
  "maxLiveAgents": 4,
  "maxLiveCommands": 16
}
```

Omitted or zero values select the existing bounded defaults: 32 agent executions
and 128 commands/capture streams. Negative values fail configuration validation.
There is no unlimited sentinel. Agent admission reserves capacity before provider
startup; failed admission releases its reservation. Confirmed closure frees a
slot. Command admission is atomic across concurrent requests. Limits apply to one
Runner; they are not a distributed Hub budget or a limit on provider-internal
subagents. Restart is required to adopt changed Runner configuration.

## Remaining P03 work

- Durable attempt/resource inventory, connection generations and fencing.
- Container and PTY ownership, escaped/new-session children, reboot and helper
  failure reconciliation. A child that deliberately changes process groups is
  outside this guard's guarantee. Killing the helper alone also destroys its
  cleanup capability; this is reported as uncertain, not silently repaired.
- Other subprocess paths (Git operations, login helpers and snippet tools) and
  provider behavior that creates detached groups need individual qualification.
- Evidence for processes outside a session host: a `lazybox run` pipe's PID
  has no recorded start time, so a live PID there stays "unknown" (see
  [Unconfirmed ownership](#unconfirmed-ownership)).
- Per-attempt quotas, resource accounting and Linux runtime qualification.

Codefly remains the native service supervisor. Hub agent/assignment state remains
owned by the external Hub. This helper implements a child-lifetime capability, not a second
daemon, reconnect service or orchestration state machine.

## Verification

Tests cover SIGKILL of a fixture Runner with a child and grandchild, an unrelated
process remaining alive, normal exit with background descendants, cancellation,
exact argv/environment/cwd/stdout/stderr, missing targets, concurrent Stop/Wait,
unowned helper rejection, uncertain-cleanup propagation and admission races.
Disabling lifetime EOF cleanup reproduces the stranded-process failure.

The process guard, commands, JSONL and Runner suites run under Go's race detector.
These tests exercise real local subprocesses with fixture programs. They do not
qualify live Claude/Codex processes, detached descendants, containers or reboot.
