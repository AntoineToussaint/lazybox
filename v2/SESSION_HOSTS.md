# Agents outlive the Runner

**Requirement.** Restarting the Runner (promoting a release, a Dev hot reload,
a crash, `launchctl kickstart`) is a no-op for running agents and the commands
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

A host runs its program in one of two modes:

- **Lines** (agents): JSONL on stdin/stdout. Each line is one sequenced entry;
  requests and responses are tracked so an unanswered request is redelivered.
- **Stream** (managed commands): no stdin; every read from stdout or stderr is
  one sequenced chunk. Cancelling a command kills its group at once: there is no
  graceful stdin close to wait for.

Agent hosts live under `<state>/hosts/<id>/`, command hosts under
`<state>/command-hosts/<id>/` (both 0700). The private `host.json` (0600) carries
the loopback port and a random token; there are no fixed ports or paths. Working
and Dev have separate state, so separate hosts.

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
  A request the Runner answered is never replayed.
- Any replayed line a previous Runner already received is marked
  **redelivered**. Adapters never act twice on a redelivered request whose first
  handling may have had effects:
  - reading or waiting for a managed command has no effect (and the command
    outlived the Runner), so a redelivered `read` or `wait` simply runs again;
  - a redelivered `start` or `cancel` is answered with an explicit "outcome
    unknown" error, recorded as `outcome_unknown_after_restart`, and shown in
    the conversation;
  - an unanswered approval simply stays pending (recorded once, not twice).
- While no Runner is attached the host keeps reading until a 64 MiB buffer is
  full, then stops: the program blocks on its own output instead of losing it.
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
  and the Codefly service. Ephemeral test Runners keep the old contract: their
  agents and commands stop with them, and at startup they stop any agent host
  a crashed predecessor left behind.

## Upgrades

A host speaks the wire protocol of the binary that started it, for its whole
life. A Runner reads the host's version from `host.json` and says hello in that
version if it is inside its window (`supported` in `wire.go`). A wire change
bumps `Protocol` and keeps the window's lower bound at the oldest version a
supported release could have launched a host with. Terminate and release are
version-stable, so a host outside the window can still be stopped; its
conversation then falls back to native resume.

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

## Testing

| Command (from the repository root) | What it runs | Accounts |
| --- | --- | --- |
| `make -C v2 test-outlive` | Every deterministic test below, with the race detector | none |
| `make -C v2 test` | The whole Go suite, which includes them | none |
| `make -C v2 test-real-outlive` | The live restart check with real agents | `claude` and `codex` logins; billable |

The deterministic tests start real processes: a session host, a fake agent or
a shell command, and a Runner that is itself a re-executed test binary, so it
can be SIGKILLed together with its process group. No provider is called. To
run one test while iterating, from `v2/services/runner/code`:

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

1. `make -C v2 dev`, then start a conversation.
2. Ask for something long that uses a command, for example: *Run
   `sleep 90 && echo finished` with lazybox_command, wait for it, then tell me
   what it printed.* Approve the command.
3. While it waits, restart the Runner in any of these ways: save a Go file under
   `v2/services/runner/code` (hot reload); Ctrl-C the launcher and run
   `make -C v2 dev` again; or SIGKILL the Runner and run `make -C v2 dev`
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
  `TestExitedAgentHostKeepsOutputUntilRecorded`,
  `TestReattachedRunnerUsesFreshRequestIDs`,
  `TestOrphanedHostWithAFullBufferExits`,
  `TestOutputBeforeTheSpawnReplyIsKept`,
  `TestFailedOpenLeavesAdoptedCommandsRunning`,
  `TestCloseDuringAHostedStartStopsTheCommand`,
  `TestReplayDeduplicationEndsWithTheReplayedLine`.
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

**Known gaps** (found in review, not fixed yet):
- A user's *decline* is not journaled with its approval, so a declined
  `lazybox_command` call redelivered after a restart is handled as answered:
  `read`/`wait` run, and `start`/`cancel` are reported as outcome unknown.
- A Runner that stops consuming output for 30 seconds is disconnected by its
  hosts as if it had died; the agent keeps running unattached.
- A superseded connection's operations (an acknowledgement, a proxy upstream)
  are still applied until it closes, which can take seconds.
- Writing to an agent's stdin runs on the control connection's reader, so an
  agent that stops reading stdin delays that connection's acknowledgements.
- The unauthenticated hello has no size limit before the token check, and a
  failing `Accept` ends the host.
- A message the agent received is placed in the rebuilt conversation where it
  was accepted, not where it was delivered, and text replayed through a
  rotation can repeat in the excerpt it hands over.
