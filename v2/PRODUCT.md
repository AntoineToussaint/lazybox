# Lazybox V2 product definition

Lazybox V2 helps humans and agents deliver complex work together while preserving its context across people, devices, locations, and interruptions. The central product problem is how much coordination remains in a human's head as the amount of simultaneous work grows.

## The problem

One initiative can involve several repositories, agents, applications, reviewers, and machines. The human has to understand ownership, dependencies, decisions, and integration while the underlying work keeps changing. Opening another session increases available execution without necessarily making the whole effort easier to direct.

V1 already contains substantial coordination machinery. Its [epic implementation](../crates/server/src/epics.rs), [work model](../crates/core/src/work.rs), and [review model](../crates/core/src/review.rs) are relevant inputs. V2 needs a coherent product experience around these concerns, with all reused implementation reviewed against its new responsibility.

The user's reported pain includes the terminal interaction model, difficult UI development workflows, ineffective multi-window use, and supervising many agents. V2 must make that work visible and actionable without requiring continual reconstruction of individual conversations.

## Product outcomes

| Outcome | What a person can do |
| --- | --- |
| Understand the shared effort | See what is being attempted, who owns each part, what depends on it, and what changed. |
| Direct work at scale | Change priorities, assign work, resolve blockers, and inspect exceptions from shared work views. |
| Collaborate with people | Contribute under an individual identity, review evidence, make attributable decisions, and hand over responsibility. |
| Collaborate with agents | Delegate bounded work, exchange findings, answer questions, and follow results through review and integration. |
| Watch real execution | Read the same build, test, and service output the agent can inspect, in independent live consoles. |
| Develop visually | Inspect the running application, capture and annotate problems, and route evidence to the relevant work and agent. |
| Continue elsewhere | Reconnect from another computer and recover the work, decisions, artifacts, and useful personal context. |
| Trust reported status | Distinguish declared progress, observed execution, verified results, and uncertain state. |

## Participants

**Human contributors** plan, implement, inspect, review, and make decisions. A person may change activity without changing identity. Different people can act on the same project concurrently.

**Agents** have identifiable capabilities and assignments. An agent can coordinate, implement, investigate, review, or integrate within its assigned scope. Its role is explicit, and its execution attempts remain distinguishable from its identity and responsibility.

**Project maintainers** manage membership, execution authority, and final integration. The MVP uses a small role model; it must still attribute actions to the actual person or agent.

**Runners** supply execution environments. They are infrastructure, with capabilities and capacity, rather than additional members of the reasoning team.

## Immediate dogfood goal — 2026-11-09

One person completes development from a phone while the laptop is closed: start
work across repositories, direct agents, answer questions, inspect changes/tests/
preview, request revisions and approve integration. Continue on iPad or desktop
windows without reconstructing context. An available remote Runner executes;
devices present different experiences over the same work. Start with one writer
and an independent reviewer; parallel writers follow measured evidence. The Hub
belongs in Obin platform V2. Its contract and remote integration are required,
while its implementation stays outside this repository. See
[Device experience](DEVICE_EXPERIENCE.md) and [delivery order](ROADMAP.md).

## The reference journey

This remains the broader collaborative target; the immediate phone dogfood goal
above and the revised roadmap determine the current release scope.

Two humans work on a change spanning a frontend and a backend repository. A Hub holds their shared project. Two Runners are registered: a developer machine and a remote execution host.

1. The first human records the objective, existing tracker links, acceptance criteria, and initial work graph.
2. The team agrees an interface contract. Two agents receive related assignments with explicit inputs and expected outputs.
3. The first human keeps the work graph in one window and a live frontend preview in another. Each window has independent navigation and focus.
4. The second human opens the same work, inspects the preview, and attaches a screenshot with a pinned comment. They assign the feedback to the relevant agent.
5. An agent asks a question whose answer affects both assignments. The answer becomes a decision linked to the affected work, and both agents can retrieve it.
6. The first human closes the desktop and leaves. Work on the remote Runner continues. The local Runner's availability is shown accurately if its machine sleeps.
7. The first human signs in from a different computer. They see the shared work, accessible artifacts, outstanding questions, and changes since their last visit.
8. Reviewers inspect diffs and test evidence. A revision marks old review evidence appropriately. The team accepts the result and lands changes in dependency order through explicit integration actions.

The [MVP](MVP.md) turns this journey into repeatable acceptance scenarios.

## Desktop working surfaces

A working surface is a view over shared work. It can live in a pane or an independent native window and can be restored on another device. It has its own selection, navigation, and purpose.

| Surface | Useful actions |
| --- | --- |
| Work map | Inspect dependencies, assign responsibility, change priority, and understand why work is held. |
| Work detail | Read the objective, constraints, discussion, decisions, evidence, and result in context. |
| Execution | See structured progress, tool activity, questions, and failures; inspect a terminal when needed. |
| Command console | Watch actual command output, scroll and search retained logs, and return to the associated work. |
| Application preview | Interact with the running application, select a preview instance, and capture feedback. |
| Review | Inspect diffs, screenshots, test results, findings, and the revisions they refer to. |
| Decisions and attention | Resolve questions and exceptions with their affected work already attached. |
| Integration | See review readiness, dependency order, outstanding failures, and landing outcomes. |

The MVP implements a focused subset of these surfaces. Later releases expand their depth without creating separate copies of shared work.

Repository entry is a unified, searchable picker of logical repository identities.
Choose repositories to create a workspace, then add more directly to that
workspace. There is no separate user-facing registration/library step. A workspace
is the user's stable working context and screen; its physical working copies
remain Runner-managed resources. Existing conversations remain attached when its
name changes. [Workspaces](WORKSPACES.md) describes the current product flow.

Claude Code and Codex also serve application features such as help, planning,
and summaries through a shared provider integration. These assistant requests
can use supplied application or Collection context without creating a coding
environment. A suggested plan becomes repository work through an explicit
user action. Existing permissions and the selected subscription/account remain
authoritative.

## Collaboration behavior

### Shared work and personal arrangement

Objectives, assignments, dependencies, decisions, comments, artifacts, and execution facts are shared. Window positions, navigation, drafts, and personal attention state belong to an individual. Opening or selecting an item in one window does not redirect another person's window.

A deliberate follow action may link views in a later release. Shared presentation is separate from shared editing authority.

### Evidence and handoffs

A handoff includes the current objective, completed work, relevant decisions, outstanding questions, evidence references, and the receiving participant. The receiver can inspect the referenced material without needing the sender's terminal or device.

Feedback stays attached to the item and revision it describes. A screenshot annotation includes its preview and work context; a code finding includes its diff or commit context.

### Roles and authority

Planning, coordination, implementation, review, and integration are explicit responsibilities. Product roles must map to actual allowed operations where they constrain behavior. Human participation and automation can coexist at different points in the same workflow.

The system shows who requested an action, who performed it, and whether it completed. A model's statement that work is done is distinct from the evidence used to accept it.

## Design principles

1. Shared work has a stable identity across conversations, executions, tracker transitions, and machines.
2. Humans and agents use the same underlying work and evidence model.
3. Local and remote deployments expose the same product operations.
4. Multiple windows are an early acceptance requirement, with independent interaction state.
5. Continuity includes artifacts and decisions as well as running processes.
6. Status is explainable from recorded facts, including missing or stale observations.
7. Visual tools support actions such as inspecting, annotating, assigning, and accepting work.
8. Context is bounded and available by reference so collaboration does not require copying whole transcripts.
9. Reuse is earned through the [audit process](REUSE_AUDIT.md).

## Measuring product progress

During dogfooding, record how often a user must open an individual transcript to determine status, manually repeat context between participants, recover lost window arrangements, or locate an artifact on another machine. Record the actual intervention needed to finish the reference journey.

Measure completion and recovery as well as speed. Additional agents are useful only when the group can still explain ownership, decisions, current status, and accepted results. Initial performance targets and test sizes are proposed in [MVP validation](MVP.md#scale-and-responsiveness).

## Expansion boundaries

The first release targets software work across phone, iPad and desktop. Richer
visual composition, additional providers, multiple organizations and more
autonomous planning can follow. Mobile interaction and lifecycle are requirements
now; the [roadmap](ROADMAP.md) distinguishes those from later expansion.
