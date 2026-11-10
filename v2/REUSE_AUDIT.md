# Lazybox V2 code reuse audit

Every piece of V1 code admitted into V2 must have a recorded review of its correctness, dependencies, and fit for its new responsibility. Source inspection and existing passing tests are inputs to that review. They do not constitute approval by themselves.

## Admission boundary

Review code before copying it, importing its crate, extracting it, or calling it through a compatibility layer. An adapter does not remove the obligation to understand the implementation behind it.

The selected Go service implementation also needs this review when porting V1 behavior or algorithms. Record the source behavior and known failures being carried forward, then verify the new implementation against its V2 contract. A language rewrite does not establish correctness. New code without inherited behavior follows normal design and verification requirements rather than being labeled an approved V1 reuse component.

The review covers the admitted internal dependency closure, including relevant startup behavior, build scripts, schema migrations, global configuration, platform branches, and runtime side effects. Record third-party dependencies and evaluate their use at the boundary. Importing a large V1 crate makes its reachable behavior part of the review scope.

The audit is incremental. Start with the components required by the [MVP](MVP.md), then repeat for later roadmap packages. V1 features outside a required boundary remain outside V2 until there is a product reason and an audit for bringing them forward.

## Review states

| State | Meaning | Production admission |
| --- | --- | --- |
| Candidate | A potentially useful source has been identified. | No |
| In review | Its boundary, dependencies, and evidence are being investigated. | No |
| Repair required | Correctness or architectural problems have been identified. | No |
| Approved for scope | The named source revision and dependency boundary satisfy the stated contract. | Yes, within that scope |
| Rejected | Rewrite, replace, or leave this implementation behind. | No |
| Revalidation required | Source, dependencies, contract, or failure assumptions changed after approval. | No new adoption until reviewed |

Approval of an idea or a data shape is separate from approval of the source implementation. Extracting a small useful component may be the correct disposition even when the containing crate is rejected.

## Review procedure

### 1 Define the V2 responsibility

State the behavior needed by the roadmap package. Specify the component that owns it, its inputs and outputs, authority, persistence promises, lifecycle, cancellation, error behavior, and supported platform/environment assumptions.

Use the [architecture](ARCHITECTURE.md) and [MVP acceptance scenarios](MVP.md#acceptance-scenarios) as the target contract. Existing V1 call sites explain current behavior but do not establish the required V2 behavior.

### 2 Pin the source and dependencies

Record the repository, revision, files, relevant features, and internal/third-party dependencies. If uncommitted changes are included, record their exact patch or digest and ownership; a commit identifier alone does not describe them.

Map readers, writers, startup effects, environment variables, configuration resolution, credentials, path resolution, spawned processes, and state migrations. Make all imports and implicit lifecycle ownership visible.

### 3 Examine correctness and known failures

Read the implementation, relevant tests, and repository regression records. Trace a normal request and the failure cases that affect its contract. Investigate defect history where available; record references rather than assuming comments describe the current state.

Check concurrency, partial writes, cancellation, repeated requests, stale state, crash recovery, bounded memory, and platform differences where the component actually encounters them. For execution code, include child processes and uncertain network outcomes. For desktop code, include multiple windows and independent service lifetimes.

When a behavior is repaired, reproduce the failure and show that the regression test fails against the old behavior and passes with the fix. A symptom disappearing after a change is not sufficient diagnosis.

### 4 Examine architectural fit

Ask whether the implementation assumes one human, one client, one process, one repository, local filesystem access, mutable workspace identity, or a continuously attached terminal.

Check whether the component can provide an explicit contract without pulling orchestration, provider wiring, or UI behavior into the wrong layer. Dependency changes follow the repository's enforced layering rules and require a deliberate architectural decision.

### 5 Verify at the right level

Use unit and contract tests for deterministic behavior, integration tests for real git/process/storage boundaries, and manual native checks for windowing and visual interaction. Test the actual failing shape where a known bug requires it.

Record commands, environment, revision, and results. Include unavailable credentials, missing devices, skipped paths, simulated dependencies, and remaining uncertainty. A missing required verification keeps the candidate out of the scope that requires it.

### 6 Decide and admit

Choose reuse, extract and adapt, repair, rewrite, or omit. Record why, the admitted scope, source revision, dependency closure, evidence, reviewer, and unresolved limitations.

Link the implementation package and changed code to the audit. Repairs classify the change at its owning boundary under the repository's existing fix/hack convention. A deliberately labeled stopgap needs a precise owning issue or documented policy-authorized follow-up; it cannot silently become a permanent V2 dependency.

### 7 Revalidate changes

Reopen the audit when the reviewed source or reachable dependencies change, when V2 adds platforms or callers, or when the contract gains new authority, concurrency, or recovery promises. Retain the earlier evidence and explain what needs fresh verification.

## Initial candidate inventory

All rows below are **Candidate**. These are pointers for investigation, with no approval or defect finding implied. Read the nearest applicable area instructions when reviewing a component.

| ID | Candidate sources | Potential V2 destination | Review questions | First milestone |
| --- | --- | --- | --- | --- |
| R01 | [Core work model](../crates/core/src/work.rs), [work store](../crates/server/src/work_store.rs), [work calls](../crates/server/src/work_calls.rs) | Hub work and result records | Individual human identity, workspace-bound ownership, transactional updates, partial writes, lifecycle, revision conflicts, stable portable references. | M0/M1 |
| R02 | [Epics](../crates/server/src/epics.rs), [epic record](../crates/core/src/epic.rs), [policies](../crates/core/src/policy.rs) | Hub dependency and integration planning | Graph correctness, cycle behavior, contract freshness, merge ordering, source-of-truth boundaries, scheduler idempotency, lock duration, and side effects during reads. | M1/M3 |
| R03 | [Agent runs](../crates/server/src/agent_runs.rs), [agent stream](../crates/server/src/agent_stream.rs), [agent implementations](../crates/agents/src) | Runner structured adapters | Real Claude/Codex capability coverage, turn/process distinctions, questions/approvals, credential context, restart/resume, cancellation, and output backpressure. | M0/M2/M3 |
| R04 | [Git operations](../crates/git-ops/src) | Runner repository preparation | Locking, cancellation, worktree ownership, branch/revision resolution, configured roots, concurrent preparation, and retained user data. | M0/M2 |
| R05 | [Core review](../crates/core/src/review.rs), [review store](../crates/server/src/review_store.rs) | Hub evidence and review | Revision binding, finding identity, stale verdicts, durable dispositions, actor identity, and acceptance authority. | M3 |
| R06 | [Artifact spool](../crates/server/src/artifacts.rs) | Runner production and Hub artifact access | Filesystem trust, incomplete writes, traversal, ownership, transfer idempotency, retention, portability, and availability after the producer disappears. | M2 |
| R07 | [Store](../crates/store/src) and [core paths](../crates/core/src/paths.rs) | Hub persistence and configuration | Atomic revisions/events, durability, corrupt records, migrations, isolation of V1 state, resolved paths, and backup/restore. | M0/M1 |
| R08 | [IPC types](../crates/ipc/src), [API gateway](../crates/server/src/api_gateway.rs), [contract generation](../crates/server/src/bin/generate-desktop-contract.rs) | V2 protocol tooling and selected transport pieces | Product operation coverage, authentication, schema/version evolution, bounded subscriptions, replay gaps, and separation of high-volume streams. | M0/M1 |
| R09 | [Desktop frontend](../apps/desktop/src), [native shell](../apps/desktop/src-tauri/src), [client runtime](../crates/server/src/client_runtime.rs) | Desktop interaction and launch pieces | Multi-window ownership, personal/shared state, focus, rendering, accessibility, preview capture, and service shutdown coupled to client lifetime. | M0/M4 |
| R10 | [Auth](../crates/auth/src), [identity](../crates/identity/src), [gateway principal types](../crates/ipc/src/lib.rs) | Participant/device enrollment and scoped operations | Human/device/agent/Runner separation, credential ownership, revocation, authenticated attribution, and operation scope. | M0/M1 |
| R11 | [Relay](../crates/relay/src), [encrypted channel](../crates/e2e-channel/src) | Optional connection transport | Pairing, identity pinning, reconnect, backpressure, enrollment scope, version compatibility, and how routing relates to Hub authority. | M0 decision; later if selected |
| R12 | [Working claims](../crates/server/src/working_claims.rs), [registries](../crates/server/src/registries.rs), [sandbox](../crates/sandbox/src) | Selected Runner coordination mechanisms | Lease meaning under partition, duplicate starts, process ownership, authority fencing, capacities, and the actual environment isolation provided. | M2 |
| R13 | [GitHub provider](../crates/gh-provider/src), [provider state machines](../docs/provider-state-machines.md) | Hub tracker and integration adapter | Principal attribution, observed versus requested state, retry/reconcile semantics, record continuity, and uncertain merge outcomes. | M3 |
| R14 | [Agent guide](../crates/agents/src/guide.rs), [coordination design](../docs/mcp-coordination.md), [coordination V2 proposal](../docs/agent-coordination-v2.md) | Agent context and operation surface | Discoverability, bounded context, implicit session identity, durable receipts, and parity across agent families and CLI/MCP access. | M3 |
| R15 | [Log CLI](../crates/tui-boot/src/main.rs), [agent log instructions](../crates/agents/src/session_context.rs), [log terminal kinds](../crates/ipc/src/lib.rs) | Runner log publication and shared command consoles | Preserve agent-facing output, capture versus producer completion, actual upstream exit status, spawn acknowledgement, temp-file durability, replay, storage budgets, disconnected publication, and console close versus process cancellation. | M0/M2 |

The transport candidates are options, not prerequisites. The old relay is a connection broker; adopting it does not create the Hub's global work model or multi-human authority.

## Audit record template

Create a record under `v2/audits/` when a candidate enters review. The folder is created with the first actual audit. Use this template and retain actual findings and evidence there.

```markdown
# V2 reuse audit for Component name

Status: Candidate | In review | Repair required | Approved for scope | Rejected | Revalidation required
Candidate ID:
Roadmap package:
Reviewer:
Review date:

## Required V2 contract

Owning component, caller scope, invariants, authority, supported environments,
and persistence, concurrency, cancellation, and recovery promises.

## Source and dependency boundary

Repository, revision, exact files, dirty patch when applicable, internal
dependencies, third-party dependencies, build/startup effects, and migrations.

## Findings

For each finding: source location, trigger, observed behavior, required
behavior, owning component, severity, and repair or rejection disposition.

## Verification

Commands, source revision, environment, results, counterfactual regression
evidence, real versus simulated dependencies, and native/manual checks.

## Limits and unresolved work

Unverified paths, missing prerequisites, unsupported cases, and required
follow-up work with its effect on admission.

## Disposition

Reuse, extract and adapt, repair, rewrite, or omit; rationale; approved source
and scope; dependency closure; required changes; and revalidation triggers.

## Admission references

Implementation changes and the accepted V2 tests that defend this contract.
```

## Admission review

Before merging implementation that reuses V1 code, the reviewer should be able to identify every admitted source and dependency, its approved contract, the defects examined or repaired, the tests establishing its behavior, and what remains unverified. An audit record with unresolved required evidence remains open.

## V2 008 scoped review — 2026-10-27

Reviewer: Codex implementation session. Source revision: `c4f67e521595cb77d3849f4bc9cb77ada240ea55`; the inspected V1 agent/log sources had no local patch included in this review.

**R03, gateway routing only — approved for scope as a behavior port.** Reviewed `crates/agents/src/agent.rs`, `LlmProvider::base_url_env`, `Agent::gateway_injection`, and `codex_gateway_provider_flags`, plus their gateway regression tests. The admitted behavior is Claude's base-URL environment mapping and Codex's explicit custom Responses provider, native provider-auth forwarding, and disabled WebSockets. No Rust crate, PTY detector, registry, session briefing, credential store, or dependency closure is imported. New Go adapter owners implement the mapping. Tests exercise the options and the actual installed harnesses through local gateway fixtures. Production lazybox gateway cost attribution remains unverified.

**R15, log-pipe candidate — rewrite, source implementation not admitted.** Reviewed `crates/tui-boot/src/main.rs`, `run_log`, `log_open`, `create_log_file`, and `drain_stdin_to`, and `crates/agents/src/session_context.rs` log guidance. V1 registers a daemon terminal and drains stdin to a local tail file, deliberately keeping that output out of agent capture. V2 needs independently replayable command identities and continued agent access. The new command owner/CLI uses no V1 temp-file naming, socket resolution, terminal lifecycle, or store. It echoes producer input, records failure visibly, and distinguishes pipe EOF from managed exit status. Process/replay/publication tests and the two-viewer browser test establish the new boundary. V1 daemon and auth paths remain outside the executable.

**New implementation boundaries.** The engine, bounded fsynced journals, profile/checkpoint model, generic managed command service, and request registry are new code against V2 contracts. The Claude worker uses the supported pinned SDK; Codex uses the installed app-server protocol. Codefly supplies the pinned service scaffold, generated listeners, endpoint resolution, health, and lifecycle. Its listener-binding observation is recorded in runtime evidence; no consumer listener was substituted.

**Runner Collections.** The Collection manager is a new implementation against
[the V2 Collection contract](COLLECTIONS.md) and [Git's public worktree CLI](https://git-scm.com/docs/git-worktree). No V1
git-ops implementation, clone/auth chain, workspace cleanup algorithm, or Rust
dependency is admitted. Inspection of `crates/git-ops/src/lib.rs` identified
shared repository metadata and destructive-reclaim concerns; V2 serializes
provisioning, creates new branches, and offers nondestructive archive only.
Actual temporary Git repositories exercise provisioning and recovery; fake
adapters are explicitly limited to execution-lifecycle tests. This change owns
the behavior in the Runner; it is a fix at the owning boundary, with no stopgap
worktree wrapper or frontend-owned checkout logic.

**Native desktop host.** The V2 Tauri host is new composition code in an independent Cargo workspace; no V1 Rust, daemon wiring, frontend, or credential resolver is imported. Only `apps/desktop/src-tauri/icons/icon.png` and `icon.icns` are copied as existing brand assets. Connection/bootstrap and native window creation belong to this host; the generated Connect client accepts the official Tauri HTTP transport through its platform seam. This is an owning-boundary implementation, not a replacement browser window or a custom Runner listener. Native validation is recorded in runtime evidence separately from browser checks.

**Classification.** These changes implement behavior at its owning boundaries: agent protocol mapping in adapters, process/capture behavior in the command owner, queueing/receipts in the engine, and shared service composition through Codefly's supported seam. No fix is claimed for V1 behavior or Codefly's listener scope. The runnable experiment is not an unlabelled replacement for Hub or production recovery. Outstanding verification is explicit in `RUNTIME_EVIDENCE.md`.

## GitHub connection review — 2026-11-02

Inspected V1 `crates/gh-provider/src/oauth.rs`. Its OAuth registration/scopes,
ambient `gh` fallback and token lifecycle do not satisfy the V2 requirement for
isolated App authorization and selected repositories. Disposition: rewrite;
no V1 OAuth implementation, client ID, credential store or provider dependency
is admitted. The V1 provider skill's credential-chain guidance yields to the
user's explicit isolated-auth requirement here.

V2's independent `internal/sources/github` client owns GitHub wire behavior;
`runner/githubconnection` owns private credentials, refresh and enrollment;
`runner/repositories` owns the local Git operation; transport and shared
surfaces compose these boundaries. This is a fix at the behavior owners, with
no credential-chain stopgap. App registration is Runner configuration, separate
from each user's sign-in. Public device/refresh behavior follows GitHub's
published contract; no private OAuth endpoint, scraped session or browser
credential is reused.

Verification includes HTTP protocol fixtures, real temporary Git repositories,
race tests for connection/import/transport, and browser lifecycle tests. Live
GitHub registration, authorization and private clone remain unverified. This
admission covers read-on-request repository and issue/PR access only, not V1's
polling, role derivation, mutation/reconciliation or shared inbox semantics.
