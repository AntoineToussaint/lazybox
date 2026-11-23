import type { Mission, MissionEvidence, MissionTask } from '@lazybox/protocol';

/**
 * Pure presentation of a mission board. The Runner derives every status and
 * trust level from the mission's event log; nothing here decides them, it
 * only says them and arranges them.
 */

export type ColumnKey = 'ready' | 'working' | 'needs' | 'done';
export const columns: readonly { key: ColumnKey; title: string; empty: string }[] = [
  { key: 'ready', title: 'Ready', empty: 'Add a task, then assign it to a conversation.' },
  { key: 'working', title: 'Working', empty: 'Assigned tasks show here while their agent works.' },
  { key: 'needs', title: 'Needs you', empty: 'Nothing waits for you.' },
  { key: 'done', title: 'Done', empty: 'Tasks you accept land here.' },
];

/** The board column of a derived status. */
export function columnOf(status: string): ColumnKey {
  switch (status) {
    case 'working': case 'verifying': case 'changes_requested': return 'working';
    case 'needs_you': case 'failed': case 'claimed_done': return 'needs';
    case 'done': case 'canceled': return 'done';
    default: return 'ready';
  }
}

export const statusLabel: Record<string, string> = {
  ready: 'Ready', blocked: 'Blocked', working: 'Working', verifying: 'Verifying', changes_requested: 'Changes requested',
  needs_you: 'Needs you', claimed_done: 'Reported done', done: 'Done', failed: 'Failed', canceled: 'Canceled',
};

/** Trust, weakest first, and what each means to a person. */
export const trustLevels = ['claimed', 'observed', 'verified', 'accepted'] as const;
export type Trust = typeof trustLevels[number];
export const trustLabel: Record<string, string> = { claimed: 'Claimed', observed: 'Observed', verified: 'Verified', accepted: 'Accepted' };
export const trustMeaning: Record<string, string> = {
  claimed: 'Someone said so; nothing the Runner saw backs it yet.',
  observed: 'The Runner saw it: a lazybox run exited 0, or the agent is running.',
  verified: 'Checks passed on the newest commit the Runner saw.',
  accepted: 'You accepted it.',
};
export const flagLabel: Record<string, string> = {
  stale: 'Checks are on an older commit', failing: 'Last run failed', ci_failing: 'Checks failed', no_evidence: 'No evidence', blockers: 'Review found blockers',
};

/** Tasks arranged in columns, in the order the Runner gave them; the done column puts canceled last. */
export function arrange(tasks: readonly MissionTask[]): Record<ColumnKey, MissionTask[]> {
  const out: Record<ColumnKey, MissionTask[]> = { ready: [], working: [], needs: [], done: [] };
  for (const task of tasks) out[columnOf(task.status)].push(task);
  out.done.sort((a, b) => Number(a.status === 'canceled') - Number(b.status === 'canceled') || Number(b.updatedAtUnixMillis - a.updatedAtUnixMillis));
  out.needs.sort((a, b) => Number(!!b.gate) - Number(!!a.gate) || Number(a.updatedAtUnixMillis - b.updatedAtUnixMillis));
  return out;
}

/** A task waiting for a decision at a human gate: the inbox's "awaiting acceptance". */
export const awaiting = (task: MissionTask) => task.status === 'needs_you' && !!task.gate;

export type AwaitingRow = { mission: Mission; task: MissionTask };
/** Every task awaiting acceptance across missions, oldest report first (first come, first served). */
export function awaitingRows(missions: readonly Mission[]): AwaitingRow[] {
  return missions.filter(m => !m.canceled && !m.error).flatMap(mission => mission.tasks.filter(awaiting).map(task => ({ mission, task })))
    .sort((a, b) => Number(a.task.reportedAtUnixMillis - b.task.reportedAtUnixMillis));
}

/** One line summing up a mission: what needs the person first. */
export function missionSummary(mission: Mission): string {
  if (mission.error) return 'Unreadable: ' + mission.error;
  if (mission.canceled) return 'Canceled';
  const tasks = mission.tasks.filter(t => t.status !== 'canceled');
  if (!tasks.length) return 'No tasks yet';
  const count = (key: ColumnKey) => tasks.filter(t => columnOf(t.status) === key).length;
  const needs = count('needs'), working = count('working'), done = tasks.filter(t => t.status === 'done').length;
  const parts = [needs && `${needs} need${needs === 1 ? 's' : ''} you`, working && `${working} working`, `${done} of ${tasks.length} done`].filter(Boolean);
  return parts.join(' · ');
}

const short = (sha: string) => sha.slice(0, 7);

/** An evidence chip: a mark, a short label and its outcome tone. */
export function evidenceChip(e: MissionEvidence): { mark: string; label: string; tone: 'pass' | 'fail' | 'pending' | 'neutral'; title: string } {
  const tone = e.outcome === 'pass' ? 'pass' : e.outcome === 'fail' ? 'fail' : e.outcome === 'pending' ? 'pending' : 'neutral';
  const mark = tone === 'pass' ? '✓' : tone === 'fail' ? '✗' : tone === 'pending' ? '◌' : '•';
  if (e.kind === 'run') {
    const exit = e.exitCode === undefined ? (e.piped ? 'piped' : '') : `exit ${e.exitCode}`;
    return { mark, label: [e.title || 'run', exit].filter(Boolean).join(' · '), tone, title: [e.piped ? 'Piped through lazybox log: the producer’s exit status is unknown.' : 'lazybox run, exit status observed by the Runner.', e.detail, e.outputPath && `Full output: ${e.outputPath}`].filter(Boolean).join('\n') };
  }
  if (e.kind === 'pr') {
    if (e.ref.startsWith('pr-link:')) return { mark, label: `Linked PR #${e.number}`, tone, title: `Delivered ${e.repository}#${e.number}` };
    return { mark, label: `PR #${e.number}${e.head ? ' · ' + short(e.head) : ''}`, tone, title: [`${e.repository}#${e.number}${e.state ? ` (${e.state})` : ''}`, e.detail].filter(Boolean).join('\n') };
  }
  // An agent review: failing while it has open blockers.
  if (e.kind === 'review' && e.contractDigest) {
    const revisions = Object.entries(e.revisions).map(([member, head]) => member + ' ' + head);
    const label = e.state === 'stale' ? 'Review needs repeating' : e.title || 'Mission review';
    return { mark, tone, label, title: [e.detail, 'Contract ' + e.contractDigest, ...revisions].filter(Boolean).join('\n') };
  }
  if (e.kind === 'review') return { mark, label: [e.title || 'Agent review', e.detail].filter(Boolean).join(' · '), tone, title: [e.title, e.outcome === 'fail' ? 'Open blockers keep this task from verified.' : e.outcome === 'pending' ? 'The reviewer is still working.' : 'No open blockers.', e.ref].filter(Boolean).join('\n') };
  if (e.kind === 'commit') return { mark: '•', label: `${e.member ? e.member + ' ' : ''}${short(e.head)}`, tone: 'neutral', title: e.title };
  return { mark, label: e.title || e.ref, tone, title: e.ref };
}

/**
 * Keep the latest checks for each PR, and show its delivery marker until checks arrive.
 * Keep the newest unresolved command failure beside the latest run: a failure stays
 * until a later run of the same command passes, since another passing command can test a different part of the work.
 */
export function headlineEvidence(evidence: readonly MissionEvidence[]): MissionEvidence[] {
  const seen = new Set<string>(), passed = new Set<string>(), out: MissionEvidence[] = [];
  const prKey = (e: MissionEvidence) => `pr:${e.repository.toLowerCase()}#${e.number}`;
  const current = evidence.filter(e => !e.historical);
  const checked = new Set(current.filter(e => e.kind === 'pr' && !e.ref.startsWith('pr-link:')).map(prKey));
  let failure = false;
  for (const e of current) {
    if (e.kind === 'run' && e.outcome === 'fail' && !failure && !(e.title && passed.has(e.title))) {
      failure = true;
      if (seen.has('run')) { out.push(e); continue; }
    }
    if (e.kind === 'run' && e.outcome === 'pass' && e.title) passed.add(e.title);
    const key = e.kind === 'pr' ? prKey(e) : e.kind;
    if (seen.has(key) || (e.kind === 'pr' && e.ref.startsWith('pr-link:') && checked.has(key))) continue;
    seen.add(key);
    out.push(e);
  }
  return out;
}

/** What a person can do next on a task, primary first. */
export type TaskAction = 'assign' | 'report' | 'accept' | 'reject' | 'open';
export function taskActions(task: MissionTask): TaskAction[] {
  const live = !!task.attempt && !task.attempt.endedAtUnixMillis;
  if (task.attempt?.live === 'reserved' && !task.attempt.endedAtUnixMillis) return [];
  switch (task.status) {
    case 'ready': return live ? ['open', 'report', 'assign'] : ['assign'];
    case 'blocked': return ['assign'];
    case 'working': return ['open', 'report'];
    case 'changes_requested': return live ? ['open', 'report'] : ['assign'];
    case 'needs_you': return task.gate ? ['accept', 'reject', 'open'] : ['open'];
    case 'claimed_done': return ['open'];
    case 'failed': return ['assign'];
    default: return task.attempt ? ['open'] : [];
  }
}

/** A conversation's liveness, said briefly. */
export function liveLabel(live: string, detail = ''): string {
  const base = { reserved: 'Reserved', running: 'Running', waiting: 'Waiting for you', idle: 'Idle', closed: 'Stopped', missing: 'Deleted' }[live] || '';
  return base && detail && live !== 'closed' ? `${base} · ${detail}` : base;
}

/** The text a task's brief starts from when a person adds one. */
export const briefTemplate = 'Objective: \nDone when: \nBoundaries: \nOutput: ';

export const ago = (at: bigint | number, now = Date.now()) => {
  const ms = Number(at); if (!ms) return '';
  const s = Math.max(0, Math.round((now - ms) / 1000));
  if (s < 45) return 'now';
  if (s < 3600) return `${Math.round(s / 60)}m`;
  if (s < 86400) return `${Math.round(s / 3600)}h`;
  return `${Math.round(s / 86400)}d`;
};
