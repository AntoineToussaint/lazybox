# V2 audit — development from any device

Date: 2026-11-09. Scope: V2 source, roadmap, the unapplied coordination candidate,
focused regression tests, and browser fixtures. This is not a production security
certification or a claim that every native/provider path was exercised.

## Verdict

Keep the Runner, native agent adapters, journals, repository catalog, telemetry,
and desktop host. They provide a substantial local workbench. V2 is **not yet a
reliable remote development system**, and the staged workflow is not ready to be
admitted as one. A board and more agents do not close the delivery gap.

The user's clarified objective is to develop from a phone, with appropriate
experiences on iPad, laptop and multiple displays. This supersedes the November 3
decision to defer mobile. The first complete journey must work with the laptop
closed: start work, answer a question, inspect source/tests/preview, request a
revision, approve integration, and return later without reconstructing context.
The execution must be on an available remote Runner; a sleeping laptop cannot
continue running its local processes.

The Hub remains in **Obin platform V2**. Lazybox should implement its Runner and
clients against an explicit Hub contract, not create another canonical tracker.
No real Obin Hub API was validated by this audit. The proposed contract in
[Device experience](../../DEVICE_EXPERIENCE.md) is a design boundary, not an
assertion about an existing service.

## What was actually audited

Two separate code states exist:

| State | Observation |
| --- | --- |
| Main `v2/` | No workflow RPC/store or lifetime process guard. All 364 files in the saved November 3 main baseline still matched before audit documentation changes. |
| Staged candidate | `.cache/implementation/coordination-2026-11-03/source`, 44 changed/new files; manifest still says `applied:false`. Adds workflow SQLite state, launch receipts, reviewer roles, commit capture, single-writer admission and a process-group guard. |
| Running installation | Not restarted or identified as equivalent to either source in this audit. Source presence is not deployment evidence. |
| Version control | The entire `v2/` tree is still untracked in the parent repository. Existing V1 modifications were left alone. A local snapshot is not a published, reproducible release. |

Main source digest before this audit's additions:
`6f79100ca7d87c2bb4f1f8335224041a2562997be36427fa1250a7b97b4ace93`.
Staged digest, freshly verified:
`998d06e2effb4b98902b39b856639bb6cff41e4e14be5f889ab82ed12e84cc18`.
Digests use `scripts/environments.mjs::digestTree`, excluding runtime/build data.

## Capability assessment

“Implemented” below describes source, not universal acceptance evidence.

| Capability | Current source / evidence | Remaining boundary |
| --- | --- | --- |
| Claude and Codex | Provider adapters, normalized events, correlated questions/approvals, resume/checkpoint distinction. | Live-provider parity and model/account behavior require explicit current runs; fixture parity is insufficient. |
| Chat continuity | Retained reader/cursor and stable successor identity. 35 client tests pass; WebKit resume keeps existing transcript DOM. | Native lifecycle, app termination and cross-device continuation are different tests. |
| Repositories/workspaces | Unified logical catalog, personal-token or App discovery, incremental preparation, persistent worktrees, rename/delete. Collection/catalog suites pass. | Remote identity/permissions and an agent-scoped list/add API; do not give agents full Runner authority. |
| Files and changes | Monaco source editor, rich Markdown, file references, uncommitted diff with an explicit index/HEAD label. | Result-bound committed diffs, immutable artifact access, remote preview and phone review. |
| Navigation/snippets | V1 catalog adapter, configurable keyboard presets, compact single-conversation workspace row. | Full interactive PTY shell is not the captured-command viewer. Touch navigation needs a separate composition. |
| Usage/context | Durable scoped observations, configurable exact-model prices, deduplication, cumulative deltas, provider context details when available. Telemetry suite passes. | Task/result cost, long-range time queries, budget enforcement and invoice reconciliation are not delivered. Codex categories must stay unknown when not reported. |
| Process ownership | Main has normal process-group cancellation and conservative uncertain-owner recovery. Staged SIGKILL guard passes focused tests. | Guard not in main; detached groups/containers need separate ownership and reconciliation. |
| Coordination | Absent in main. Candidate has transactional board/receipts/events, role-tagged attempts and captured clean commits. Existing workflow tests pass. | Two newly reproduced correctness defects; no durable decision delivery, test provenance or integration receipts. |
| Multi-model policy | Per-attempt account/model/permissions and observed model fields. | Named role policies, effort support, scope/budget validation and requested-versus-observed role configuration. |
| Phone/iPad | iPhone package has a manifest and README only; no Expo app. Web layout stacks desktop panes. | Purpose-built flows, remote identity, lifecycle recovery, durable attention, notifications and real-device tests. |
| Hub/remote | Proposed interfaces and local bearer-protected Runner RPC. | Enrollment, principals, revocation, remote routing, artifact availability and actual Obin contract tests. |

## Release-blocking findings

### F1 — Agent and human authority collapse at the general RPC boundary

**Reproduced against the staged candidate.** The workflow tool rejects agent plan
approval. The same shared Runner credential can call authenticated
`MutateWorkflow(action=approve)` and the task becomes `ready`.

The owning code is `internal/transport/auth.go` (one bearer), staged
`internal/transport/workflow.go` (passes actor `""`), and
`internal/runner/bench.go` (exports the endpoint locator to agent processes).
`work.go` writes the Runner token to that endpoint file. No distinct human
principal is established by the workflow transport.

This is a real RPC policy bypass, not an unauthenticated internet exploit.
Trusted same-user local agents already have broad filesystem authority; scoped
tokens alone will not isolate a hostile process that can read the owner's files.

**Fix at the authority owner:** separate human/device, Runner and attempt
principals; derive actor server-side; issue scoped, expiring/revocable attempt
capabilities. Stop exposing the control credential to agents. Put credentials
outside the agent's accessible environment when claiming process isolation.
Repository-add, workflow-report and command tools should use that limited plane.
Test negative authorization through the actual RPC interceptor, not only tool
callbacks. This is prerequisite to phone access and unattended orchestration.

### F2 — Accepted dependency state can outlive the source it certifies

**Reproduced against the staged candidate using a temporary real Git repository.**
Capture and accept task A's commit, reset the checkout to its parent, then launch
task B depending on A: the worker starts successfully.

`workflows.Ready` checks only the predecessor's `done` status. Capture/acceptance
correctly rejects a changed source at acceptance time, but dispatch does not
validate that accepted dependencies are still available in its input.

**Fix at assignment admission:** bind an assignment to objective/decision
versions, dependency result IDs and a source vector. Check the necessary accepted
commits are included in the assigned checkout (or resolve an explicit compatible
artifact), not merely that current HEAD exactly equals an earlier HEAD. Ordinary
descendant commits must remain valid. Reset, missing repository, changed decision
and revised criteria need explicit invalidation/replanning. The candidate's
`objective` mutation currently changes text without invalidating the old plan.

### F3 — Review is not a verifiable delivery record

**Source inspection.** Candidate `Result` stores attempt/assignment IDs, clean
commit vector and timestamps. `Evidence` remains worker-provided text. It stores
neither test execution receipts nor an integration outcome. The independent
reviewer is a separate chat, without a typed finding/verdict attached to a result.

The current `ChangesPanel` only renders unstaged/staged differences. Capturing a
clean committed result therefore produces no reviewable branch change in that
view. The reviewer prompt asks for exact-commit inspection, but the environment
is a shared mutable checkout; it is not a pinned review artifact. Read-only tools
do not make the observed files immutable.

**Fix at the result/review owner:** capture base and result commits per repository,
test command/run IDs, actual exit status and output artifacts. Offer committed
diffs against that exact base. Persist findings and human disposition against
that result version. Keep `accepted locally`, `integration pending`, `integrated`
and `integration uncertain` distinct. A PR URL is an external reference, not
proof of delivery. Multi-repository integration can partially succeed; reconcile
each external effect before retrying after a lost acknowledgement.

### F4 — Resource lifetime is unfinished, and limits are not an operating policy

**Fresh positive test plus source inspection.** The staged guard reclaims its
owned Unix process group after owner SIGKILL, cleans background descendants on
normal exit and preserves exit/start-failure distinctions. Main does not include
it. It deliberately does not cover detached process groups or containers.

Main's 32 execution and 128 managed-command limits are local hardcoded ceilings,
not a task queue, resource budget or complete count of native child agents.
Recovery conservatively asks for reconciliation rather than claiming old workers
were re-adopted. The quoted 171-session/tmux incident must not be represented as a
V2 incident proven fixed: V2's native adapters use supervised pipes, and the
incident itself was not reconstructed here.

**Fix at the resource owner:** admit the process guard independently after its
normal/loss/restart tests; persist attempt ownership and resource handles;
configure hard active limits and bounded queue/retention. Containers need scoped
labels/IDs and a reconciler. Delete/cancel must report unresolved owned resources
and keep evidence; do not use global tmux/Docker cleanup as lifecycle management.
An always-on Runner makes this more important, not less.

### F5 — A change log is not durable communication

**Source inspection.** The candidate's SQLite store atomically writes board,
command receipt and ordered changes. This is useful. Its `Changes` reader is only
consumed in tests; there is no durable recipient inbox, delivery acknowledgement
or coordinator wake-up loop. `ContextVersion: 1` identifies a format, not which
decisions have been received. Existing conversations keep their stored context.

**Fix at work/communication ownership:** persist questions, decisions and
attention items independently of provider requests. Deliver bounded versioned
updates at safe attempt boundaries with deduplication and receipts. Distinguish
recorded, delivered, received and resolved. A push notification should link to
that record; it is not the record. Do not solve this by copying whole transcripts
between agents. Reopening an old coordinator cannot imply it read new decisions.

### F6 — Phone support requires a different experience and remote lifecycle

**Reproduced with a fresh WebKit fixture at 390 × 844.** Document width is 661;
the chat viewport is about 112 pixels tall. Desktop actions overflow, and the
workspace list occupies the top of the same scroll composition. See
[phone capture](phone-current.png). A compact laptop fixture is materially better
than the old screenshots, but still repeats navigation/session chrome; see
[desktop capture](desktop-current.png).

`apps/iphone` contains no application. `client-core` wraps Runner RPC and streaming;
it is not yet the shared work/query/command layer promised by the design.
`WorkspaceHost` mostly models desktop window operations. Local in-memory reader
caches cannot establish continuation after phone process termination.

**Fix at client composition and shared state:** use phone routes, adaptive iPad
columns and desktop panels over shared typed models/actions. Start with durable
attention and work detail, then result review/preview. Preserve separate device
navigation and explicit handoff. Test suspension, process termination, reconnection,
expired credentials and competing answers from two devices. See the concrete
[experience contract](../../DEVICE_EXPERIENCE.md).

### F7 — The release gate does not cover the failures users have been reporting

**Source inspection.** `stageCandidate` runs `make check`, native Rust checks and
a selected list of browser scripts. It omits resume-continuity, chat-scroll,
sidebar-polish and navigation-snippets; the staged workflow browser test is also
not in that admission list. More scripts existing does not make them a release
gate. Current tests emphasize local fixtures; there is no phone-to-remote delivery
journey, no notification lifecycle proof and no two-device stale approval test.

**Fix at release tooling:** one named gate catalog used by local checking,
staging and CI; each result records source digest, topology, provider shape and
skips. Promote a pinned candidate only after that gate. Keep deterministic
protocol tests, real native-harness fixtures and opt-in real model/account tests
distinct. Add fault injection at each durable write/dispatch/ack boundary.

### F8 — Roadmap and ownership documents disagree with the target

**Fresh inventory.** The old M1 catalog says build a Hub here; November 3 demotes
it and mobile; the user now makes phone development central. README/MVP and test
directory READMEs also contain scaffold-era claims. The November 3 description
of the staged workflow says it lacks a change log; the preserved candidate now
has one. It still lacks a consumer and integration evidence.

**Fix at documentation ownership:** the November 9 roadmap and experience
contract are authoritative for delivery order. Retain the old milestone inventory
as historical scope, not a second active plan. Track implemented, staged,
verified and released separately. Do not mark L1/L2 or the phone journey complete.

## What not to rebuild

Source entry points for the findings:

| Area | Main source | Staged source where applicable |
| --- | --- | --- |
| Authority | [RPC interceptor](../../services/runner/code/internal/transport/auth.go), [Runner bootstrap](../../services/runner/code/work.go) | [Workflow transport](../../.cache/implementation/coordination-2026-11-03/source/services/runner/code/internal/transport/workflow.go) |
| Assignment and result | [Runner lifecycle](../../services/runner/code/internal/runner/bench.go) | [Transitions](../../.cache/implementation/coordination-2026-11-03/source/services/runner/code/internal/runner/workflows/store.go), [launch](../../.cache/implementation/coordination-2026-11-03/source/services/runner/code/internal/runner/workflow.go), [review](../../.cache/implementation/coordination-2026-11-03/source/services/runner/code/internal/runner/workflow_review.go) |
| Process cleanup | Main lacks lifetime guard | [Guard](../../.cache/implementation/coordination-2026-11-03/source/services/runner/code/internal/processguard/guard.go) |
| Source review | [Changes surface](../../packages/surfaces/src/ChangesPanel.tsx), [RPC](../../services/runner/code/internal/transport/changes.go) | Same uncommitted diff boundary |
| Continuity and device composition | [Reader](../../packages/surfaces/src/transcript-cache.ts), [client core](../../packages/client-core/src/index.ts), [host](../../packages/platform/src/index.ts), [workspace CSS](../../packages/surfaces/src/workspace.css) | Workflow panel is an additional desktop surface |
| Release coverage | [Staging](../../scripts/environments.mjs), [gates](../../Makefile) | Candidate manifest is not release admission |

The staged links refer to the preserved local candidate, not tracked main files.

- Keep native Claude/Codex harnesses. Lazybox owns assignment, lifecycle, policy
  and evidence; it should not duplicate their reasoning/tool loops.
- Keep transactional workflow receipts and launch identity from the candidate,
  but do not import all 44 files as an accepted architecture.
- Keep the unified repository picker, Monaco editor, rich renderer, transcript
  cache and telemetry store. Extract reusable behavior where needed for mobile;
  sharing DOM layout with React Native is not the goal.
- Keep desktop windows independent. Synchronize work and explicit handoff, not
  every device's selected tab or scroll position.
- Retain a direct single-agent path. Coordination is explicit and bounded.

The original “reviewer continuation becomes writable” hypothesis was not
established: the public continuation path retains the original options, and
native resume reloads saved options. It is not listed as a confirmed defect.

## Revised sequence

1. **Establish the portable contract and UX together.** Define the phone journey,
   typed work/attention/result references, authority and failure states with the
   external Hub owner. Prototype phone/iPad/desktop navigation against the same
   scenario. Fix F1/F2/F4 and make the failure probes admission tests.
2. **Ship a complete phone development loop.** One human on two devices, one
   always-on Runner, one writer, an independent review, durable questions,
   committed diff/test artifacts/preview and explicit integration. Real Obin
   authentication and command delivery must pass before calling this remote
   dogfooding; a mock Hub cannot satisfy it.
3. **Deepen the device experiences and bounded coordination.** iPad split view,
   desktop multi-display restoration, scoped editable files/interactive shell,
   richer review and cost timelines; then isolated parallel assignments measured
   against the sequential baseline. No unattended fleet before cleanup and
   admission limits are proven.

These are deliverable packages, not a promised three-week calendar estimate.
The acceptance gates and proposed owner boundaries are in [Roadmap](../../ROADMAP.md).

## Validation performed in this audit

| Check | Outcome |
| --- | --- |
| Saved main baseline, 364 hashes | All matched before audit edits. |
| Candidate source digest | Matched the staged manifest. |
| Main `npm run test:client` | 35 passed; no skipped tests. |
| Main Go telemetry, repositorycatalog, collections | Passed with `GOMAXPROCS=1`, `GOFLAGS=-p=1`. |
| Main WebKit resume continuity | Passed: existing transcript nodes retained and cursor continues. |
| Main WebKit sidebar polish | Passed: repository identity, geometry, dialog focus, compact layout. Screenshot inspected. |
| Phone viewport diagnostic | Failed usability expectations: width 661 for viewport 390; 112-pixel chat area. Screenshot inspected. |
| Candidate workflow store suite | Passed. |
| Candidate runner workflow tests | Passed. |
| Candidate process guard focused cases | SIGKILL/normal exit/start failure cases passed. |
| New source-dependency acceptance probe | **Fails**: rolled-back dependency still launches. |
| New authenticated authority acceptance probe | **Fails**: shared agent credential approves. |

The two failures are preserved with a [reproducer](../../tests/audit/2026-11-09/reproduce.mjs)
and [output](negative-probes.txt). They run in a fresh copy of the staged source;
no user's repository is reset. A failing exit is intentional until fixed.

Not rerun: the full aggregate gate, native desktop, real Claude/Codex inference,
real multi-account rotation, actual org discovery, real iPhone/iPad, remote TLS,
Hub integration, push delivery, remote preview, or multi-repository publication.
No app restart, deployment, process-fleet cleanup, external issue or push was
performed. Historical passing checks are not counted as fresh audit evidence.

## Research used to challenge the architecture

These sources inform recommendations; they do not prove this implementation.

- Anthropic distinguishes predictable code-driven workflows from agent-directed
  execution and recommends adding complexity when the benefit warrants it.
  Apply that distinction to a small delivery state machine around native agents.
  [Building effective agents](https://www.anthropic.com/research/building-effective-agents).
- Anthropic's multi-agent research gains concern decomposable research. They are
  not evidence that simultaneous writers in one checkout improve coding delivery.
  [Multi-agent research system](https://www.anthropic.com/research/multi-agent-research-system).
- Temporal's activity contract still requires application-level idempotency.
  Adopting a workflow engine cannot itself make publication or process starts
  exactly once. Keep destination reconciliation regardless of framework choice.
  [Activity definition](https://docs.temporal.io/activity-definition).
- Apple describes adaptive layouts across window sizes and multitasking; device
  names alone should not select a fixed pane count.
  [Layout](https://developer.apple.com/design/human-interface-guidelines/layout).
- Expo documents different notification/lifecycle paths. Build durable attention
  first and fetch current authority/state when a notification is opened.
  [Notification behavior](https://docs.expo.dev/push-notifications/what-you-need-to-know/).
