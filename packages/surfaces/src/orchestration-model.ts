import type { Mission, MissionAttempt, MissionEvent, MissionGrant, MissionMessage, MissionPlan, MissionTask, PlanOverlap, PlanTask, Session } from '@lazybox/protocol';

/**
 * The Orchestrator grant, as people see it (audits/2026-11-16/sota-agents.md
 * §6.3): one conversation the person lets run agents for one mission, within
 * a ceiling that only narrows down the tree. The Runner decides everything
 * here; these are the words and numbers the app shows.
 */

export type PermissionMode = 'read-only' | 'ask' | 'yolo';
export const PERMISSION_ORDER: readonly PermissionMode[] = ['read-only', 'ask', 'yolo'];
export const permissionLabel: Record<string, string> = { 'read-only': 'Read only', ask: 'Ask', yolo: 'Full access' };

/** The level a conversation runs at, for comparing: inherit (the provider's default) counts as ask. */
export function effectivePermission(mode: string | undefined): PermissionMode {
  return (PERMISSION_ORDER as readonly string[]).includes(mode || '') ? mode as PermissionMode : 'ask';
}

/** The permission choices a grant offers: at most the conversation's own. */
export function permissionChoices(own: string | undefined): { mode: PermissionMode; disabled?: string }[] {
  const limit = PERMISSION_ORDER.indexOf(effectivePermission(own));
  return PERMISSION_ORDER.map((mode, i) => ({ mode, disabled: i > limit ? `This conversation runs at ${permissionLabel[effectivePermission(own)]}; its agents cannot get more` : undefined }));
}

/** What the person grants, before it is sent. */
export type GrantDraft = { permission: PermissionMode; profileIds: string[]; maxChildren: number; budgetUsd: number; /** No agent starts before the person approves a plan (recommended). */ planRequired: boolean };

export function defaultGrant(session: Pick<Session, 'permission' | 'profileId' | 'rotationProfileIds'> | undefined): GrantDraft {
  return { permission: effectivePermission(session?.permission), profileIds: [...new Set([session?.profileId || '', ...(session?.rotationProfileIds || [])].filter(Boolean))], maxChildren: 3, budgetUsd: 0, planRequired: true };
}

/** Why a draft cannot be granted yet, or undefined. */
export function grantProblem(draft: GrantDraft): string | undefined {
  if (!draft.profileIds.length) return 'Choose at least one subscription its agents may use';
  if (draft.maxChildren < 1 || draft.maxChildren > 16) return 'Between 1 and 16 agents at once';
  if (draft.budgetUsd < 0 || draft.budgetUsd > 10000) return 'A budget between $0 (no cap) and $10,000';
  return undefined;
}

export function usd(value: number): string {
  return value >= 100 ? `$${Math.round(value)}` : `$${value.toFixed(2).replace(/\.00$/, '')}`;
}

/** One line: what a grant allows. */
export function ceilingLine(grant: Pick<MissionGrant, 'ceiling'>): string {
  const c = grant.ceiling;
  if (!c) return '';
  return [`Up to ${c.maxChildren} at once`, `${permissionLabel[c.permission] || c.permission} or narrower`, c.budgetUsd > 0 ? `${usd(c.budgetUsd)} cap` : '', c.depth > 1 ? `${c.depth} levels deep` : ''].filter(Boolean).join(' · ');
}

/** The live grant a conversation holds in a list of missions. */
export function grantOf(missions: readonly Mission[], conversationId: string): { mission: Mission; grant: MissionGrant } | undefined {
  for (const mission of missions) for (const grant of mission.grants) if (grant.live && grant.conversationId === conversationId) return { mission, grant };
  return undefined;
}

/** The children a grant started, as their tasks' latest attempts on the board. */
export function childrenOf(mission: Mission | undefined, conversationId: string): { task: MissionTask; attempt: MissionAttempt }[] {
  if (!mission) return [];
  const out: { task: MissionTask; attempt: MissionAttempt }[] = [];
  for (const task of mission.tasks) {
    const attempt = [...task.attempts].reverse().find(a => a.parentConversationId === conversationId);
    if (attempt) out.push({ task, attempt });
  }
  return out;
}

/** How a child reads in a list: its liveness, or what its task says. */
export function childState(task: MissionTask, attempt: MissionAttempt): { label: string; tone: 'run' | 'wait' | 'idle' | 'done' | 'off' } {
  if (task.status === 'done') return { label: 'Accepted', tone: 'done' };
  if (task.status === 'needs_you' && task.gate) return { label: 'Reported · needs you', tone: 'wait' };
  if (attempt.endedAtUnixMillis && attempt.endedAtUnixMillis > 0n) return { label: attempt.outcome === 'stopped' ? 'Stopped' : 'Ended', tone: 'off' };
  switch (attempt.live) {
    case 'running': return { label: 'Working', tone: 'run' };
    case 'waiting': return { label: 'Needs you', tone: 'wait' };
    case 'idle': return { label: 'Idle', tone: 'idle' };
    case 'closed': return { label: 'Closed', tone: 'off' };
  }
  return { label: 'Gone', tone: 'off' };
}

/** A subscription by its name: its label, else its provider. */
export function profileLabel(profiles: readonly { id: string; provider: string; label: string }[], id: string): string {
  const p = profiles.find(v => v.id === id);
  if (!p) return id;
  return (p.provider === 'codex' ? 'Codex' : 'Claude') + (p.label ? ` · ${p.label}` : '');
}

/** Whether two path globs can name the same file (a trailing / is a folder). */
export function pathsMeet(a: string, b: string): boolean {
  const ta = a.replace(/\*+$/, ''), tb = b.replace(/\*+$/, '');
  return ta === tb || (ta.endsWith('/') && tb.startsWith(ta)) || (tb.endsWith('/') && ta.startsWith(tb));
}

/** The other tasks of a plan that share one of a task's paths. */
export function overlapsOf(overlaps: readonly PlanOverlap[], key: string, path: string): string[] {
  const out = new Set<string>();
  for (const o of overlaps) if (o.keys.includes(key) && pathsMeet(o.path, path)) for (const k of o.keys) if (k !== key) out.add(k);
  return [...out];
}

/** A plan's tasks in the order they can start, each with its step (tasks at the same step run side by side). */
export function planOrder(tasks: readonly PlanTask[]): { task: PlanTask; step: number }[] {
  const step = new Map<string, number>();
  const at = (task: PlanTask, seen = new Set<string>()): number => {
    if (step.has(task.key)) return step.get(task.key)!;
    if (seen.has(task.key)) return 1;
    seen.add(task.key);
    const before = task.after.map(k => tasks.find(t => t.key === k)).filter((t): t is PlanTask => !!t);
    const value = before.length ? Math.max(...before.map(t => at(t, seen))) + 1 : 1;
    step.set(task.key, value);
    return value;
  };
  return tasks.map(task => ({ task, step: at(task) })).sort((a, b) => a.step - b.step);
}

/** The cap a plan sheet offers: the grant's when it has one, else the estimate and a quarter, rounded up. */
export function defaultCap(total: number, grantBudget: number): number {
  if (grantBudget > 0) return Math.max(grantBudget, Math.ceil(total));
  return total > 0 ? Math.ceil(total * 1.25) : 0;
}

/** The plan of a mission waiting for the person, newest first. */
export function pendingPlans(missions: readonly Mission[]): { mission: Mission; plan: MissionPlan }[] {
  return missions.flatMap(mission => mission.canceled ? [] : mission.plans.filter(p => p.state === 'proposed').map(plan => ({ mission, plan })));
}

/** One line on a tree's budget: "$3.10 spent · $5 reserved · $12 cap". */
export function budgetLine(grant: Pick<MissionGrant, 'spentUsd' | 'reservedUsd' | 'ceiling'>): string {
  const cap = grant.ceiling?.budgetUsd || 0;
  return [`${usd(grant.spentUsd)} spent`, grant.reservedUsd > 0 ? `${usd(grant.reservedUsd)} reserved` : '', cap > 0 ? `${usd(cap)} cap` : 'no cap'].filter(Boolean).join(' · ');
}

/** One lane of the mission timeline: an actor. */
export type Lane = { key: string; short: string; title: string; tone: 'person' | 'orchestrator' | 'agent' | 'system'; conversation?: string; match(e: MissionEvent): boolean };

const initials = (title: string) => (title.match(/[\p{L}\p{N}]+/gu) || ['?']).slice(0, 2).map(w => w[0]).join('').toUpperCase();

/** The timeline's lanes: you, each orchestrator, each agent, and Lazybox (its rules and checks). */
export function lanesOf(mission: Mission, events: readonly MissionEvent[]): Lane[] {
  const lanes: Lane[] = [{ key: 'you', short: 'You', title: 'You', tone: 'person', match: e => e.actorKind === 'human' }];
  const seen = new Set<string>();
  const add = (conversation: string, title: string, tone: Lane['tone']) => {
    if (!conversation || seen.has(conversation)) return;
    seen.add(conversation);
    lanes.push({ key: conversation, short: initials(title), title, tone, conversation, match: e => e.conversationId === conversation && e.actorKind === 'agent' });
  };
  for (const g of mission.grants) add(g.conversationId, g.title || 'Orchestrator', 'orchestrator');
  for (const t of mission.tasks) for (const a of t.attempts) if (a.parentConversationId) add(a.conversationId, a.title || t.title, 'agent');
  for (const e of events) if (e.actorKind === 'agent') add(e.conversationId, 'Agent', 'agent');
  lanes.push({ key: 'lazybox', short: 'LB', title: 'Lazybox: rules, checks and what it observed', tone: 'system', match: () => true });
  return lanes;
}

export type Thread = { key: string; a: { conversation: string; label: string }; b: { conversation: string; label: string }; messages: MissionMessage[]; last: bigint };

/** Messages between agents, one thread per pair (oldest first), the busiest-latest first. */
export function messageThreads(messages: readonly MissionMessage[]): Thread[] {
  const threads = new Map<string, Thread>();
  for (const m of [...messages].sort((x, y) => Number(x.seq - y.seq))) {
    const key = [m.fromConversationId, m.toConversationId].sort().join('|');
    let t = threads.get(key);
    if (!t) { t = { key, a: { conversation: m.fromConversationId, label: m.fromLabel || 'Agent' }, b: { conversation: m.toConversationId, label: m.toLabel || 'Agent' }, messages: [], last: 0n }; threads.set(key, t); }
    t.messages.push(m); t.last = m.atUnixMillis;
  }
  return [...threads.values()].sort((x, y) => Number(y.last - x.last));
}

/** The other tasks a task overlapped with, and on which paths. */
export function overlapsOfTask(mission: Mission, taskId: string): { other: string; paths: string[]; kind: string }[] {
  return mission.overlaps.filter(o => o.taskIds.includes(taskId)).map(o => ({ other: mission.tasks.find(t => t.id === o.taskIds.find(id => id !== taskId))?.title || 'another task', paths: o.paths, kind: o.kind }));
}

/** New agents are held: as many tasks wait for the person as the live grant allows. */
export function reviewHeld(mission: Mission): { waiting: number; limit: number } | undefined {
  const g = mission.grants.find(v => v.live);
  if (!g) return undefined;
  const limit = g.ceiling?.reviewLimit || 3;
  return mission.awaitingReview >= limit ? { waiting: mission.awaitingReview, limit } : undefined;
}

/** Lineage lines Lazybox records in a conversation (runner/orchestrator.go). */
export const LINEAGE_EVENTS = new Set(['agent_spawned', 'agent_message', 'agent_stopped', 'orchestration_granted', 'orchestration_revoked', 'plan_proposed', 'plan_decided', 'budget_exhausted', 'budget_raised', 'overlap_detected', 'wip_held', 'conversation_started', 'conversation_declined']);
export const lineageIcon: Record<string, string> = { agent_spawned: 'subagent', agent_message: 'send', agent_stopped: 'stop', orchestration_granted: 'shield', orchestration_revoked: 'lock', plan_proposed: 'checklist', plan_decided: 'check', budget_exhausted: 'alert', budget_raised: 'card', overlap_detected: 'alert', wip_held: 'pending', conversation_started: 'chat', conversation_declined: 'close' };
