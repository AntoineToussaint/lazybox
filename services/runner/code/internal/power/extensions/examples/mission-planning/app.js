const goal = document.getElementById('goal');
let workspaceId;
workflow.conversationActions(async workspace => {
  if (workspaceId && workspace.id !== workspaceId) goal.value = '';
  workspaceId = workspace.id;
}, () => [['Plan for approval', () => {
  if (!goal.value.trim()) throw Error('Enter the outcome to plan.');
  return `Plan the following outcome in this workspace. Stay in planning: inspect existing behavior and publish planning artifacts. Leave implementation and worker dispatch pending human approval.

Outcome: ${goal.value.trim()}

Trace the current entry point to its owning code, tests and interfaces at the current member commits. Record one observable baseline example. Distinguish observations from assumptions. Ask about unanswered product decisions.
For each proposed behavior change, define the actor, input, observable result, relevant failure or compatibility case and the check that will prove it. Publish a delivery-spec (spec.v2) with stable required criterion IDs. Keep blockers explicit and the plan in draft while any required outcome or verification method is unresolved.
Assess feature/code continuity: predict which independent owners, contracts, persistence boundaries and migrations must change and why. Separate generated files and test churn. Compare with a similar prior change when available. If a small behavior change spreads across many owners, trace the coupling, compare a localized design, a bounded refactor and deferral, estimate each, and resolve the design choice before proposing execution.
Publish a plan artifact with the baseline, scope, alternatives, task dependencies, expected touched paths, effort ranges, basis and confidence, risk, and the criterion IDs each task proves. Separate elapsed effort from Mission spend estimates. State what would trigger re-estimation.
When the criteria and architecture decisions are settled, use the Mission orchestrator's plan-required grant and lazybox mission plan to propose the tasks with pinned workspaces and the exact artifact ID and version as the acceptance contract. If this conversation has no appropriate Mission grant, leave the artifacts ready, identify the missing grant and do not start work. Return the artifact IDs and approval state.`;
}]], true);
