# Agent sessions and live command consoles

V2 requires interactive communication with agents and live visibility into the shell commands they run. The proposed agent interface is a structured bidirectional headless session. Shell output remains available both to the agent and to humans in real streaming consoles, preserving the useful `lazybox log` workflow.

This is a contract proposal. The first runtime experiment must verify it against real agent harnesses before the desktop prototype establishes assumptions about execution.

The [first deliverable](FIRST_DELIVERABLE.md) specifies the runnable Go test bench. [Interfaces](INTERFACES.md) separates the generic session engine from minimal Claude/Codex protocol adapters and proposes the RPC boundary.

## Two connected interfaces

| Interface | Into the execution | Back from the execution |
| --- | --- | --- |
| Agent session | Messages, images, queued follow-ups, supported steering, interruption, answers, and approval decisions. | Structured messages, tool events, pending requests, acknowledgements, turn outcomes, and session identity. |
| Command execution | Command and environment; cancellation; deliberate stdin when an interactive command supports it. | Live output, retained output references, process observations, actual exit status where observable, and capture errors. |

The agent harness continues to own its agent loop. The Runner owns its adapter and the registered command streams. The Hub records shared metadata and routes authorized access. The Desktop can show an agent conversation and a command console in separate windows.

Terminal rendering is appropriate for shell output and interactive shell programs. Agent lifecycle and pending approvals come from the agent protocol, rather than the console's visual appearance.

## Preserve the log workflow

The V1 [log command](../crates/tui-boot/src/main.rs) registers a log window, drains piped stdin into a temporary file, and displays it through a `LogTail` terminal. Its [agent briefing](../crates/agents/src/session_context.rs) teaches agents how to publish noisy command output. These are reuse candidates and require an audit.

V2 preserves the agent's ability to publish command output with a simple shell invocation. The CLI name and syntax remain to be decided; the required behavior is:

- Execute the original command once.
- Publish its output to an identified command stream attached to the work and execution attempt.
- Preserve output for the invoking agent, either directly through a transparent pipe or through the registered command's wait/read operations.
- Let authorized humans open that same stream in a console, including a separate native window and from another computer.
- Keep output inspectable after completion and across Desktop reconnects, with explicit retention and availability.
- Keep closing a console separate from canceling or deleting the command.

Agent output can be bounded without losing access to the underlying evidence: return a useful result and stable log reference, with reads/search by offset or range. A log reference alone is insufficient if the agent has no operation that can retrieve its output.

## Capturing a pipe and owning a process

**Pipe capture** receives bytes from an existing producer. Its default must preserve the agent-facing stream and report publication failures visibly. Once stdout and stderr have been merged upstream, the capture cannot reconstruct their separate identities. End of the input stream establishes capture EOF, not the upstream command's exit status. Shell pipeline success cannot stand in for that status.

**Runner-owned command execution** starts the process, records its command metadata, captures output, reports the actual exit status or signal, and provides wait, read, and cancel operations. Agent harness tools can call this surface through the supported extension mechanism. The adapter must prove that permission policy, working directory, environment, and cancellation semantics survive the integration.

Support pipe publication as part of the MVP. Use Runner-owned execution where full process lifecycle or background services are required. Whether this replaces or complements a particular harness's built-in shell tool is an experiment outcome, not an assumed capability.

## Command identity and output

A registered command has a stable identity distinct from its work item, agent session, tool call, and console window. Store the requesting participant, execution attempt, Runner, source revision, working-directory reference, start/completion observations, capture mode, and available output ranges.

Output chunks have stream identity, byte offsets or sequence numbers, and capture timestamps. Preserve stdout/stderr channels when capture provides them. Their merged display order is the order observed by the capture system; it cannot promise an ordering the producer did not expose.

PTY commands also need explicit terminal dimensions and replay semantics. Plain piped output can retain ANSI content without creating an agent TUI. Clients negotiate how to display or replay each capture mode.

Native consoles support scrolling, selection, copying, search, following live output, and opening related work. Read-only logs expose no typed-input control. An interactive console has explicit input ownership so several viewers cannot accidentally interleave keystrokes.

## Flow control and recovery

The Runner spools output independently of Desktop rendering. A slow viewer cannot block the agent's command or lose another viewer's place. Define finite disk and buffer budgets and a visible policy for exhausted capture storage; never silently discard output or make a failed capture look complete.

The agent-facing output remains useful if live publication disconnects. Pending publication can be journaled on the Runner; the Hub reports incomplete availability until synchronization succeeds. Claim capture success only for output actually retained under the stated contract.

Desktop subscribers resume from offsets. A rotated or expired range reports an explicit gap. Completed test/build evidence needed for review is promoted to durable Hub-accessible artifacts; a temporary file on a sleeping Runner cannot satisfy cross-device evidence access.

## Agent communication requirements

Queueing, steering, interrupting, and replying to an approval are separate operations. Each request carries its actual provider/session/turn identifiers. Answer a pending approval through the protocol's response mechanism; an ordinary message saying an operation is approved is not a protocol approval.

Record message authorship, queued/delivered status, and resolved request status. Serialize conflicting answers from multiple participants. Advertise unsupported adapter capabilities explicitly; do not infer success from a successful pipe write.

Provider session persistence and reattachment require real verification. A stable Lazybox execution identity alone does not prove that a provider can resume an interrupted tool call or recover a pending approval after process loss. [Session hosts](SESSION_HOSTS.md) avoid the process loss on a Runner restart; a tool call whose outcome cannot be known is reported as unknown, never re-run.

## First runtime experiment

Before implementing the main Desktop prototype, verify each proposed adapter can:

1. Exchange several messages in one logical conversation, including an image.
2. Report progress and a real pending question/approval, then accept a correlated answer.
3. Queue a follow-up and interrupt active work; test steering if advertised.
4. Publish a real test command through the log pipe while preserving output for the agent and a human console viewer.
5. Run a managed background command, read its output, observe failure, and cancel it.
6. Keep command capture independent of attached viewers and reconnect a second viewer without rerunning the command.
7. Resume the conversation after adapter restart where supported, and report the precise limits of request recovery.

Record the exact runtime version, adapter path, capabilities exercised, raw evidence, and failures. The experiment is an admission gate for the integration approach and does not approve the whole V1 runtime for reuse.
