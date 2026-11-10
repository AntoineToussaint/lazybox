# First deliverable agent and console test bench

The first runnable deliverable proves the most important execution assumptions: real back-and-forth communication with Claude and Codex through a generic Go engine, plus command output available to the agent and live human viewers. It corresponds to V2 008 in the [roadmap](ROADMAP.md).

Implementation is runnable as the Codefly Runner service. See [launch and configuration](RUNNING.md), [source boundaries](STRUCTURE.md), and [observed acceptance evidence](RUNTIME_EVIDENCE.md). The implementation gate is not claimed closed while required live-provider/profile checks remain unverified.

## Deliverable contents

| Part | Minimum delivered behavior |
| --- | --- |
| Generic Go session engine | Own operation ordering, queueing, session identity, receipts, bounded event capture, and independent subscribers. |
| Claude adapter | Use a verified structured integration, including a small SDK worker if required. |
| Codex adapter | Use a verified structured integration with actual provider request and turn identifiers. |
| Generic command capture | Register one command stream, preserve output access for the agent, and support human subscriptions with replay offsets. |
| Managed command execution | Start, wait/read, observe actual failure, and cancel a background command. |
| Account context and continuation | Pin sessions to profiles; test exhaustion classification, selection, native resume limits, and checkpoint fallback without changing sibling credentials. |
| Test client | Send messages, inspect events, answer pending requests, interrupt, and inspect command output without the full desktop application. |
| Browser console viewer | A small React surface consuming the generated service client; two independent viewers can follow the same output. |
| Evidence and launch instructions | Reproducible setup, pinned versions, automated conformance results, and recorded real-agent outcomes and limits. |

This is an execution experiment. Production Hub membership, multi-host scheduling, full work graph, native desktop docking, remote previews, and iPhone application remain subsequent deliverables. The experiment uses isolated test state and explicit local execution authority; it cannot be advertised as the shared production Hub.

## Demonstration

1. Select Claude or Codex and start a session through the same Go interface.
2. Send a message, receive structured progress, and send a follow-up in the same conversation.
3. Send an image and establish from the response that the provider received it.
4. Trigger a real supported question and approval request, answer them by request identity, and observe resolution. Sending approval-shaped chat text does not pass.
5. Queue a follow-up, interrupt active work, and continue. Test mid-turn steering only where supported.
6. Have the agent execute a test command whose output is published once to a shared console and remains available to the agent.
7. Open a second viewer, close the first, and reconnect at an output offset. Verify that the command was not restarted and output was not duplicated or silently lost.
8. Run a managed background command, inspect output, observe a failing command's actual exit status, and cancel the background command.
9. Restart the adapter and attempt conversation resume. Record separately whether pending requests and in-flight tools can be recovered.
10. Simulate exhausted capacity, switch to another eligible profile, and continue the work. Verify native resume and checkpoint fallback separately where supported. With two real profiles available, verify actual credential isolation while a sibling execution stays active.

Execute the applicable cases against both agent families. A provider limitation remains visible and becomes an explicit decision about whether the adapter satisfies MVP requirements.

## Completion evidence

- One conformance suite runs against both implementations, with deterministic fake-provider cases for races and failures.
- Real-agent runs demonstrate the supported interactions at pinned versions; unavailable credentials or skipped paths are recorded.
- Output survives viewer disconnect and resumes by offset within the tested capture retention period.
- Slow viewers do not block agent control; buffers and local spooling have defined bounds and visible failure behavior.
- Queue acceptance, provider delivery, request resolution, turn end, process exit, and work completion remain distinguishable.
- Reused V1 code has scoped audit approval; writing new Go code does not remove the obligation to examine inherited behavior and known defects.
- Subscription continuation follows [the account contract](SUBSCRIPTIONS.md); unavailable real profiles leave switching unverified and do not prevent deterministic engine development.
- Launch/check commands and observed results are committed with the implementation, rather than documented as existing before it is built.

## Next deliverable

After this gate, build the native Desktop proof using [the selected stack](STACK.md): detach the live console into a Tauri window, close its original host, restore its view after reconnect, and adapt placement after a display change. The command and agent session keep running independently of the windows.
