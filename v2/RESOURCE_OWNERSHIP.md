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
- An evidence-based reconciliation operation for uncertain managed resources;
  no automatic slot release based only on process names or historical PIDs.
- Per-attempt quotas, resource accounting and Linux runtime qualification.

Codefly remains the native service supervisor. Hub agent/assignment state remains
owned by Obin. This helper implements a child-lifetime capability, not a second
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
