# Agents outlive the Runner

**Requirement.** Restarting the Runner (promoting a release, a Dev hot reload,
Restart Runner in the app, a crash, `launchctl kickstart`) is a no-op for running agents and the commands
they started. The next Runner re-attaches to the same native processes, with
their turn, pending approvals, queued messages and command output intact.
"History plus resume" is the fallback, not the plan.

## Shape

```
Runner              restarts and upgrades often; a client of its session hosts
  │  authenticated loopback control connection, one per agent or command
Session host        one per native agent or managed command, its own session
  │                 (setsid); owns the program's pipes and a sequenced output
  │                 buffer; for agents also a session-stable gateway proxy
Program             Claude SDK worker, codex app-server, or a managed command,
                    in a processguard group
```

A session host (`internal/sessionhost`) is the Runner binary re-executed in a
private helper mode, exactly as `processguard` does. It is not a child of the
Runner's process group or session, so a Runner exit, crash, group kill, launchd
job stop or terminal hangup does not reach it. The program runs under
`processguard` inside the host, so its descendants still die with the host.

A host runs its program in one of three modes:

- **Lines** (agents): JSONL on stdin/stdout. Each line is one sequenced entry;
  requests and responses are tracked so an unanswered request is redelivered.
- **Stream** (managed commands): no stdin; every read from stdout or stderr is
  one sequenced chunk. Cancelling a command kills its group at once: there is no
  graceful stdin close to wait for.
- **Terminal** (a person's shell): a pseudo-terminal; raw input and
  resizes from the Runner, every read one sequenced chunk, plus a bounded
  scrollback a re-attaching Runner gets in its welcome instead of a replay. The
  shell is not under `processguard`; a hang-up ends it. See
  [TERMINALS.md](TERMINALS.md).

Agent hosts live under `<state>/hosts/<id>/`, command hosts under
`<state>/command-hosts/<id>/`, terminal hosts under
`<state>/terminal-hosts/<id>/` (all 0700). The private `host.json` (0600) carries
the loopback port and a random token; there are no fixed ports or paths. Working
and Dev have separate state, so separate hosts. A host reads at most 64 KiB from
a connection before it has authenticated, and refuses a client that does not
speak the version in its record without displacing the attached Runner.

## Delivery contract

- Every output line or chunk gets a transport sequence number. The Runner
  acknowledges it only after what it derived is durable: for agents, the
  events in the execution journal (each records its `transportCursor`); for
  commands, the chunk in the command's output journal (with its `seq`). A
  re-attaching Runner resumes from exactly what it holds.
- One agent line can yield several events. All but the last are marked
  `transportPartial`, and the line is acknowledged only with its last event. A
  Runner that dies in between gets the whole line again; the events it had
  already recorded are recognised by content and recorded once.
- Unacknowledged output is replayed on re-attachment. Agent requests that were
  acknowledged but never answered are replayed too: the agent is still waiting.
  A request the Runner answered is never replayed, and neither is one the agent
  stopped waiting on without a reply: when the Codex app-server resolves a
  request itself (`serverRequest/resolved`) or ends the turn it belonged to,
  the adapter sends the host a `resolve` frame and the host forgets it, as it
  would an answer. Otherwise every re-attach showed a phantom "waiting for
  approval", and enough of them (64) faulted adoption. `resolve` is additive: a
  host older than it ignores the frame (and keeps redelivering, as before), so
  the wire version did not change.
- Any replayed line a previous Runner already received is marked
  **redelivered**. Adapters never act twice on a redelivered request whose first
  handling may have had effects:
  - reading or waiting for a managed command has no effect (and the command
    outlived the Runner), so a redelivered `read` or `wait` simply runs again;
  - a redelivered `start` or `cancel` is answered with an explicit "outcome
    unknown" error, recorded as `outcome_unknown_after_restart`, and shown in
    the conversation;
  - a declined command is declined again: nothing ran;
  - a native approval or question the user answered (its reply never reached
    the agent, or the host would not replay it) is answered again with the
    user's response, which the journal records with its decision and answers;
  - an unanswered approval simply stays pending (recorded once, not twice).
- While no Runner is attached the host keeps reading until a 64 MiB buffer is
  full, then stops: the program blocks on its own output instead of losing it.
  A Runner that is slow to read gets the same backpressure; only its process
  dying, or a newer Runner attaching, ends its connection.
- Only the attached Runner's operations count. A connection a newer Runner
  superseded is closed at its next operation, which is not applied.
- The Runner's input to an agent is queued and written by its own goroutine, so
  an agent that stops reading stdin never holds up acknowledgements.
- After the program exits, the Runner releases the host only once it has
  recorded the last output it received. A Runner that dies in between leaves
  that output and the exit for the next one.

## What a re-attaching Runner restores

| Restored exactly | How |
| --- | --- |
| Live native process, its conversation and in-flight turn | session host |
| Pending approvals and questions | snapshot + host redelivery, deduplicated |
| Queued, undelivered messages (including image bytes) | `<id>.attach.json` |
| Operation idempotency and receipts | journal scan |
| Codex turn ID, model, provider settings and overrides | adapter state in `<id>.attach.json` |
| Subscription-rotation attempt and current profile | rotation state |
| The conversation excerpt a later rotation hands over | rebuilt from the journal |
| Metering attempt identity | telemetry state; a fresh gateway route is registered |
| Provider traffic through the metered gateway | the host's proxy is re-pointed at the new Runner's gateway |
| Workspace ownership | `collections.Manager.Adopted` clears the uncertainty for adopted owners |
| Running managed commands, their output and exit status | command hosts, re-attached when the command registry opens |

A message the old Runner accepted but cannot prove it delivered gets an explicit
`unknown_after_runner_restart` receipt. It is never silently re-sent. The app
shows it, `runner_reattached` and `outcome_unknown_after_restart` as Runner
notices in the conversation.

## Lifecycle

- `Bench.Detach` (used on shutdown when `LAZYBOX_V2_KEEP_AGENTS=1`) records each
  execution's snapshot and attachment, then leaves its host running; running
  managed commands stay too. `Bench.Close` and ending a session still stop them.
- A Runner shutting down closes every journal under its readers, but a live
  conversation goes on in the next Runner. So a `WatchEvents` stream it cuts
  ends `Unavailable`, and the app reconnects from its cursor; only a
  conversation that is over (closed, or read-only history) ends its stream
  cleanly, which tells a reader to stop following. A `ReadSessionTail` page a
  restart interrupts is read again from the next Runner: journals keep their
  cursors across restarts, and their turn index is rebuilt as they open.
- Cancelling a hosted process's context stops it, exactly as for a local pipe.
  Only an explicit `Detach`, or the Runner dying outright, leaves it running.
- Opening the command registry re-attaches every unfinished command with a host.
  A command whose host is gone is reported as `command outcome unknown` (its
  host's exit killed its group); one whose host cannot be adopted is stopped.
  A Runner that fails to start detaches from what it re-attached rather than
  stopping it.
- `Bench.Adopt` runs once at startup. It re-attaches every unclosed execution
  with an attachment record. Then it stops any agent or command host nobody
  attached: a program this Runner cannot supervise must not keep writing in a
  workspace. The record of a host it cannot reach is removed only if the host
  is gone; a slow or interrupted probe is reported and the host left alone.
  "Attached" means a client in this Runner's process holds the host
  (`sessionhost.Held`), never the host's own report: a host counts a crashed
  or detached predecessor as attached until it has read that connection's
  close, which can take longer than a Runner restart.
  An execution it could not re-attach for another reason than its host being
  gone (an attachment it cannot read, a profile no longer configured) is
  stopped by that sweep and then ended like a lost agent (below): interrupted,
  resumable, with an `agent_lost` notice saying why. It is never left open.
- A fault that says only that the Runner lost its stream from the agent
  (`agent.TransportFault`: "provider stream ended", "event capture failed") is
  settled by re-attaching: `engine.Adopt` clears it, and so does any event the
  agent sends after it. The agent is evidently alive; what it reports says how
  it is. An agent that really exited reports its exit again from its host and
  fails again; one whose host is gone cannot be re-attached at all. Provider
  faults (limits, sign-in, billing, a turn's error) are never cleared this way.
- Resume on a live conversation with such a fault re-attaches it in place, as
  a restart would (`Bench.reconnect`: Detach, then adopt): no second agent is
  started. If the host is gone, or cannot be adopted (it is stopped), the
  conversation ends as interrupted and resumes natively.
- The Runner's own writes (the journal, the saved snapshot, metering, account
  state) can fail while the agent is fine: a full disk. That never ends the
  supervision of a live agent. The failure is said once in the conversation
  (`storage_error`) and in the log, and retried: a journal write with the same
  event, while nothing more is read from the agent or acknowledged to its host
  (the host keeps the output; the agent blocks once that buffer is full); a
  snapshot with the next event, or by a retry. A message sent meanwhile is
  refused, saying the disk is full. A journal that takes nothing more stops
  the Runner reading the agent, never the agent: Resume or the next Runner
  re-attaches it.
- Closing an agent whose host connection was lost asks the host to stop it;
  a host that is gone took its agent's group with it, which confirms the exit.
  A conversation whose agent's exit stays unconfirmed resumes once the person
  confirms the agent is stopped (`processesStopped`).
- A Runner that loses its stream from an agent (the host connection, its own
  record keeping) faults the conversation as a lost stream
  (`agent.TransportFault`): re-attaching settles it, and Resume re-attaches in
  place. An agent whose process exited is not that: its exit was observed (the
  host's exit report, `agent.ErrProcessExited`), the conversation's fault is
  `provider_process_exit` (`agent.ProcessExitFault`), which neither
  re-adoption nor a later event clears and no message continues, and Resume
  ends the session and continues the provider's conversation in a new process
  (`TestAnAgentThatExitedIsResumedInANewProcess`,
  `TestAnObservedExitIsNotSettledByReadoption`).
- A newer client attaching to a host supersedes the old one and tells it so; the
  superseded Runner stands down and never stops a program it no longer owns.
- A host whose program exited while no Runner was attached keeps the remaining
  output and the exit status until a Runner collects them, or until the orphan
  limit below.
- A host left with no Runner attached for 24 hours (`DefaultOrphanAfter`) stops
  its program. A restart takes seconds; a day means its environment is no
  longer run, and the program would only spend quota or hold ports for no one.
  The conversation still resumes natively when its Runner returns.
- A host whose directory was deleted (its state is gone) stops its program and
  exits. A host removes its directory when it exits.
- `scripts/runner-command.mjs` sets `LAZYBOX_V2_KEEP_AGENTS=1` for Dev, Working
  and the Codefly service; an installed release's `lazybox-runner serve` keeps
  agents by default, and its launchd job is the supervisor that offers Restart
  Runner (`--supervised`, [INSTALL.md](INSTALL.md)). Updating the app replaces
  the job (`lazybox service install`: bootout, then bootstrap of the new
  release's Runner); `TestReleaseServiceUpdateKeepsWhatRuns` checks a
  terminal's shell survives that against the built artifacts. Ephemeral test Runners keep the old contract: their
  agents and commands stop with them, and at startup they stop any agent host
  a crashed predecessor left behind.
- `RestartRunner` (owner only, `internal/restart`) records a request for the
  launcher and then takes the same SIGTERM shutdown, so it detaches exactly as
  above. It is offered only when a launcher declared it will start the Runner
  again (`LAZYBOX_V2_RESTART_REQUEST`) and agents are kept; see
  [ENVIRONMENTS.md](ENVIRONMENTS.md#restart-the-runner-from-the-app).

## After a reboot

A reboot is the one restart session hosts do not survive: it ends the Runner,
every agent, command and terminal host, and the programs in them, at once.
launchd starts the Runner again at login, and `Bench.Adopt` finds each
conversation's host record left on disk with nothing listening on it.

- **The agent.** When adopting an execution fails because its host is gone
  (`sessionhost.Gone`) and no host started for it still answers, the Runner
  knows nothing of the old agent runs. It ends the conversation as
  *interrupted*, **not** "recovery required": the person is never asked to
  confirm that processes they cannot see are stopped. The journal gets one
  `agent_lost` event whose text says what happened, what the interruption cost
  (a turn in progress, a request that was waiting, queued messages) and what to
  do; the app shows it as a notice line. Sending a message resumes the agent
  through the provider's own session (`Bench.Resume` with the saved reference),
  as a continuation of the same conversation.
- **Queued messages.** The messages waiting behind the interrupted turn move
  from the attachment record to `<id>.carried.json` (private: it can hold image
  bytes). The conversation keeps listing them as queued, and resuming delivers
  them to the resumed agent first, in order, before anything sent next.
  Deleting the conversation deletes them.
- **Everything else is on disk and simply reloads**: the transcript (the
  journal), turn checkpoints (`code-checkpoints`), missions (the work log),
  and command history with its output. A command that was running ends with
  "command outcome unknown"; a terminal host that is gone is forgotten.

Each step is repeatable: a Runner that dies part way through leaves either
the attachment record (and the next Runner finishes the job, writing the
notice once) or an ended conversation it skips.

**The reboot gate** (`make test-reboot`, the `reboot` entry of
[`scripts/release-gates.json`](scripts/release-gates.json), also part of
`make test`) proves this without credentials or network. A fixture Runner (the
test binary re-executed, with a fake Claude worker) leaves a conversation as a
working day would: a finished turn with its checkpoint, a second turn waiting
for an approval, a message queued behind it, a finished command and a running
one, and a mission task the conversation works on. The test then SIGKILLs what
a reboot kills (the Runner's process group, every host and every program's
group), waits until none is left, starts a new Runner on the same state and
checks: Adopt reports no failure; the conversation is whole (both questions,
the answer, the queued message), interrupted, resumable and not "recovery
required", with one notice; the queued message is still listed; the
checkpoint, the mission and both commands with their output are there, the
running one marked "outcome unknown"; resuming opens the provider session it
had (`resume=fixture-ref`) as the same conversation, and delivers the queued
message to it. A second test checks the notice is written once across further
restarts.

It does not reboot a Mac: launchd starting the Runner at login, a session host
whose port another process took meanwhile, and the real providers' resume are
not exercised here (`make test-real-outlive` covers real resume after a
Runner restart, with logins). Terminals are not part of the fixture.

## Upgrades

A host speaks the wire protocol of the binary that started it, for its whole
life. A Runner reads the host's version from `host.json` and says hello in that
version if it is inside its window (`supported` in `wire.go`). A wire change
bumps `Protocol` and keeps the window's lower bound at the oldest version a
supported release could have launched a host with. Terminate and release are
version-stable, so a host outside the window can still be stopped; its
conversation then falls back to native resume.

The version guards a host against frames it cannot understand, so a change only
existing hosts could receive bumps it. Terminal mode did not: its spec field and
`input`/`resize` frames go only to terminal hosts, which only a binary that knows
terminals launches, and agent and command frames are unchanged. Bumping would
have cost a rollback its live agents: an older Runner cannot adopt a host of a
newer version.

## Why there is no local daemon

A per-user launchd daemon was considered as the owner of hosts. Every job it
would have had is already covered without a second long-lived process to
install and upgrade:

- **Surviving the Runner's service manager.** Hosts leave the Runner's session
  and process group; a launchd job boot-out was verified not to reach them.
- **Limits across Runner generations.** Adopted executions and commands count
  against the new Runner's limits like any other.
- **Garbage collection.** Runner startup stops unclaimed hosts; a host with no
  Runner for a day stops itself.

A daemon becomes worth its cost if hosts must be reachable without any Runner
(for example a client attaching directly), which nothing needs today.

## See every hosted process: the Processes panel

**Panels → Processes** (workspace header) (or **Settings → Runner → Show
processes**) lists every process the Runner hosts, whether this Runner holds it
or not:

- **Agents** (named by their conversation, with provider and workspace),
  **Commands** (with the conversation that started them) and **Terminals**.
- **Not supervised by this Runner**: hosts no Runner holds, for example after a
  Runner crashed before it could hand them over. Each stops by itself after a
  day without a Runner (`DefaultOrphanAfter`).
- For each: state (running, starting, exited, not answering), the program's
  PID (the host's on hover), uptime, CPU and memory summed over the host and
  everything under it (one `ps` sample per refresh, every 3 seconds), and the
  backend that started it.
- **Open** goes to the agent's conversation, the command's console or the
  shell's terminal tab, switching workspace if needed. **Copy attach** (tmux
  only) copies `tmux -S <socket> attach -t <host>`. **Stop…** asks first and says
  what happens; it stops a process the way its owner would: the agent's session
  is closed (the conversation stays and can continue), the command is cancelled,
  the terminal is closed; an orphan is stopped directly. **Clean up orphans…**
  runs the same safe sweep as Runner startup (`stopOrphanHosts`): it stops only
  hosts this Runner does not hold, leaves a host it cannot probe alone, and
  forgets records of hosts that are gone.

`ListProcesses`, `StopProcess` and `CleanUpOrphanProcesses` are the owner's
alone: neither an agent's nor a terminal's credential may call them
(`TestProcessRPCsAreTheOwnersAlone`, and the exhaustive allowlist tests in
`internal/transport`). A host being launched counts as held from before it
exists until its client holds it, so a sweep never takes a process on its way up
for an orphan (`TestAHostBeingLaunchedIsHeld`).

## Hosting backends: session hosts (default) or tmux

How a host is *started* is pluggable (`sessionhost.Backend`): Launch, attach,
stop, probe, list, forget and held. Two backends exist:

- **Session hosts** (`sessionhost`, the default): the host is a detached process
  in its own session, as described above.
- **tmux** (`tmux`, optional): the same host runs in a window of a private tmux
  server, so you can watch any agent, command or shell from any terminal.

Only launching differs. The host in a tmux window is the same session host: it
owns the program's pipes or pseudo-terminal, sequences and buffers its output,
answers on the same authenticated wire and keeps the gateway proxy. So **every
guarantee in the delivery contract above holds in tmux mode unchanged**:
acknowledged cursors, replay and redelivery, backpressure, exactly-once
journaling, terminal scrollback handed to a re-attaching Runner. The Runner never
reads anything back from tmux; the window is a view. Records carry the backend,
and attach, stop, probe and the sweep work on any host whichever backend started
it, so switching backends never strands a running process.

### Choose it

**Settings → Runner → Process hosting**, or in the private Runner configuration:

```json
{ "processes": { "backend": "tmux", "tmux": "/opt/homebrew/bin/tmux", "tmuxConfig": "" } }
```

- `backend`: `sessionhost` (default) or `tmux`. A choice in Settings wins over
  the configuration; choosing the configured one there follows the
  configuration again.
- `tmux`: the executable; `tmux` on the Runner's PATH when empty. tmux 3.0 or
  newer.
- `tmuxConfig`: a file the private server loads; none by default, so a personal
  `tmux.conf` cannot change how hosts behave. The options hosts depend on
  (`exit-unattached off`, `destroy-unattached off`, `remain-on-exit off`) are set
  after it either way.

A change applies to the next process started, without a restart; running
processes keep the backend they started with. If tmux is chosen but cannot
start a host (not installed, too old, the server refuses), the process starts in
a session host anyway and the Processes panel and Settings say why: an optional
backend never stops an agent from starting.

### What you get

- One tmux server per Runner state directory, on its own socket
  (`tmux -L lazybox-<hash of the state directory>`), so Dev and Working never
  share one and your own tmux is untouched. One session per host, named after
  the host; the window is named after the conversation, command or terminal.
- `tmux -S <socket> ls` lists every hosted process; **Copy attach** gives the
  command for one. Detach with your prefix, then `d`.
- An **agent's** window shows each JSONL line to (`>`) and from (`<`) the agent,
  cut to 400 bytes, with Runner attach and detach events. A **command's** window
  shows its output, stderr in red. A **terminal's** window *is* the shell: its
  output, and keys you type there go to the shell. The window takes the size the
  app's panel sets.

### What tmux does worse

- **One server hosts them all.** `tmux kill-server` on that socket, or the
  server crashing, hangs up every host: each stops its program (the next Runner
  still collects the output and exits). Session hosts have no shared process.
- **Closing a window stops its process** (kill-window, kill-session), as
  closing a terminal window would. Detaching does not.
- **The window is a lossy view.** Writes to it are queued and dropped (with a
  marker) when tmux falls behind, so a stopped or slow tmux never holds the
  program up. Agent lines are truncated. The app, the console and the journals
  are complete; the window may not be.
- **Window scrollback is tmux's** `history-limit` (2,000 lines with no
  `tmuxConfig`), separate from the app's 1 MiB terminal scrollback and the
  journals.
- **Terminal size belongs to the app.** A tmux client of another size sees the
  panel's size (padded or cut); resizing tmux does not resize the shell.
- The tmux server's environment is the Runner's without `LAZYBOX_V2_*`,
  `TMUX` and `TMUX_PANE`; a window you open yourself in that server (prefix `c`)
  gets it. Hosted programs get exactly the environment they would in a session
  host.
- tmux leaves its socket file after the server exits (the last host ended); the
  next host reuses it.

### Not verified

- Linux (macOS with tmux 3.8 only).
- `kill-server` and a crashing tmux server are not tested separately: they take
  the same hang-up path as `kill-window`, which `TestClosingATmuxWindowStopsItsProgram`
  covers.
- A Runner restart in Dev with tmux selected, live (the restart path is covered
  by `TestBackendChosenInSettingsHostsTheNextProcess`, which detaches and
  re-opens the Runner with one host of each backend).

## Testing

| Command (from the repository root) | What it runs | Accounts |
| --- | --- | --- |
| `make test-outlive` | Every deterministic test below, with the race detector | none |
| `make test` | The whole Go suite, which includes them | none |
| `make test-reboot` | The reboot gate: everything a reboot kills is killed, and what must come back does ([After a reboot](#after-a-reboot)) | none |
| `make test-real-outlive` | The live restart check with real agents | `claude` and `codex` logins; billable |
| `cd services/runner/code && go test ./internal/sessionhost -run 'Tmux\|Launched\|LocalBackend'` | The backends: tmux keeps the delivery contract (replay, redelivery), shows the traffic in its window, takes a terminal's keys and follows its size; closing a window stops its program; a missing tmux fails clearly. Skipped without tmux | none |
| `cd services/runner/code && go test ./internal/runner -run 'Processes\|Tmux\|ProcessBackend\|BackendChosen'` | The inventory names every agent, command, terminal and orphan; the sweep stops only the orphan; Stop goes through each owner; tmux chosen but missing still starts and says why; a Settings choice applies to the next process and both backends are re-attached | none |
| `cd services/runner/code && go test ./internal/transport -run TestProcessRPCs` | Process RPCs refuse agent and terminal credentials | none |
| `npm run test:processes` (from the repository root) | The Processes panel and Settings → Runner → Process hosting against the fake Runner (Chromium; `TEST_WEBKIT=1` for WebKit) | none |

The deterministic tests start real processes: a session host, a fake agent or
a shell command, and a Runner that is itself a re-executed test binary, so it
can be SIGKILLed together with its process group. No provider is called. To
run one test while iterating, from `services/runner/code`:

```bash
go test -race -count=1 ./internal/runner/commands -run TestHostedCommandOutlivesARunnerKilledWithItsGroup -v
```

`test-outlive` also collects data races found inside the session hosts, which
are re-executed test binaries: their reports would otherwise go to a `host.log`
deleted on exit. It fails if any process reported one.

A test that fails can leave a host running. List them with
`pgrep -fl lazybox-session-host-v1`. Each host stops itself once its
directory, under the test's temporary directory, is gone.

### By hand, in Dev

1. `make dev`, then start a conversation.
2. Ask for something long that uses a command, for example: *Run
   `sleep 90 && echo finished` with lazybox_command, wait for it, then tell me
   what it printed.* Approve the command.
3. While it waits, restart the Runner in any of these ways: save a Go file under
   `services/runner/code` (hot reload); Ctrl-C the launcher and run
   `make dev` again; or SIGKILL the Runner and run `make dev`
   again.
4. Expect the following:
   - `pgrep -fl lazybox-session-host-v1` shows the same agent host and command
     host before and after the restart.
   - The conversation shows a **Runner** notice.
   - The turn finishes and the agent reports `finished`, with nothing resent.
   - The command's output appears once.
5. End the session. Its hosts disappear.

Host directories and their `host.log` are under the environment's state
directory, in `hosts/` and `command-hosts/`.

### Evidence

- `TestRunnerKilledMidTurnIsANoOpForTheAgent`: a Runner process blocked on an
  approval is SIGKILLed together with its process group. A new Runner adopts
  the execution, the user approves, and the *same worker PID* finishes the turn.
  The journal shows one approval, one `runner_reattached` and no process exit.
- `TestGracefulDetachHandsTheLiveAgentToTheNextRunner` and
  `TestGracefulDetachHandsRunningCommandsToTheNextRunner`: the same through
  `Detach`, for an agent and for a managed command.
- `TestHostedCommandOutlivesARunnerKilledWithItsGroup`: a command keeps running
  through a Runner SIGKILL and its output is recorded exactly once. Removing
  `setsid` makes it fail.
- `TestLineRecordedInPartIsCompletedOnceAfterAdoption`: a line recorded in part
  is completed once. Disabling the content match makes it fail.
- `TestRotationAfterReattachHandsOverTheRecordedConversation`: a rotation after
  a restart still hands over the conversation. Dropping the rebuilt excerpt
  makes it fail.
- Review fixes, each failing without its fix:
  `TestOrphanSweepLeavesAHostItCouldNotProbe`,
  `TestRunnerThatDoesNotAdoptStopsAPredecessorsAgents`,
  `TestSweepStopsAHostThatStillShowsAPredecessorAttached`,
  `TestExitedAgentHostKeepsOutputUntilRecorded`,
  `TestReattachedRunnerUsesFreshRequestIDs`,
  `TestOrphanedHostWithAFullBufferExits`,
  `TestOutputBeforeTheSpawnReplyIsKept`,
  `TestFailedOpenLeavesAdoptedCommandsRunning`,
  `TestCloseDuringAHostedStartStopsTheCommand`,
  `TestReplayDeduplicationEndsWithTheReplayedLine`,
  `TestUnreadStdinDoesNotBlockAcknowledgements`,
  `TestOversizedHelloIsRefusedAtOnce`,
  `TestIncompatibleHelloDoesNotSupersede`,
  `TestCommandAdoptedElsewhereEndsHereWithoutStopping`,
  `TestRedeliveredDeclinedCallIsDeclinedAgain`,
  `TestRedeliveredAnsweredApprovalIsAnsweredAgain`,
  `TestAdoptionPassesTheRecordedDecision`,
  `TestAdoptedConversationPlacesMessagesWhereTheyWereDelivered`,
  `TestReplayedReplyTextIsHandedOverOnce`,
  `TestResolvedRequestIsNotReplayedAfterReattach` (Codex), and in `internal/redact`
  `TestSecretCutByTheWindowDoesNotLeak`.
- Redelivery, backpressure, proxy re-pointing, token authentication, the
  version window, stream mode, orphan limit, terminate-kills-descendants, and
  exit while detached: `internal/sessionhost`.
- Adapter rules: `TestRedeliveredApprovedCommandIsNeverRunTwice`,
  `TestRedeliveredApprovedWaitRunsAgain` (Codex),
  `TestRedeliveredToolCallIsRejectedNotRerun`, `TestRedeliveredWaitRunsAgain`
  (Claude). Making every tool call unsafe to replay fails the wait tests.

Live:

- `make test-real-outlive` with Claude Code 2.1.322 through the metered gateway:
  the Runner detached while a reply was streaming; a new Runner adopted it, and
  the same agent process finished the reply and remembered the earlier turn.
- A user launchd job that launched a session host was booted out with default
  keys (no `AbandonProcessGroup`): the host and its agent survived and still
  answered a probe.

**Not verified yet:**
- Codex live: the account was at its usage limit on 2026-11-10. Run
  `LAZYBOX_V2_REAL_PROVIDER=codex make test-real-outlive` once it resets.
- Codefly's own `service stop` path (only a plain launchd job was tested).
- A Dev hot reload, live.
- A machine reboot still ends every agent and command. Native resume remains
  the fallback there.
- The call in `work.go` that sweeps a crashed predecessor's agent hosts when
  `LAZYBOX_V2_KEEP_AGENTS` is not set: the sweep itself is tested, the Codefly
  service wiring is not.

**Fixed without a regression test**, because no test could fail on the old
behaviour quickly or reliably:
- a slow Runner is no longer disconnected (the old limit was 30 seconds);
- a superseded connection's operations are no longer applied (the old window
  was the moment before its connection closed);
- the host retries a failed `Accept` instead of exiting (needs descriptor
  exhaustion);
- a command is refused before its host is launched when the registry is full,
  and `Cancel` no longer holds the command's lock across a host write.
