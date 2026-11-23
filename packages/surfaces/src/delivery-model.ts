import type { DeliveryCandidateOutcome, DeliveryCriterionOutcome, DeliveryEvidence, DeliveryOutcome } from '@lazybox/protocol';

/**
 * Pure presentation of a delivery outcome (the Runner's delivery.Project): the
 * verdict on the workspace's files, what blocks it, and the evidence behind
 * each criterion. The Runner decides every state; nothing here judges.
 */

export type Tone = 'pass' | 'fail' | 'pending' | 'neutral';

export const verdictLabel: Record<string, string> = {
  passed: 'Delivered', failed: 'Failing', blocked: 'Blocked', incomplete: 'Incomplete', unverified: 'Unverified',
};
export const stateLabel: Record<string, string> = {
  passed: 'passed', failed: 'failed', blocked: 'blocked', unverified: 'unverified', unrun: 'not run',
};
export const toneOf = (state: string): Tone =>
  state === 'passed' ? 'pass' : state === 'failed' || state === 'blocked' ? 'fail' : state === 'incomplete' || state === 'unverified' ? 'pending' : 'neutral';
export const markOf = (tone: Tone) => tone === 'pass' ? '✓' : tone === 'fail' ? '✗' : tone === 'pending' ? '◌' : '•';

/** The candidate a surface leads with: the workspace's current files, else the newest checked. */
export function headline(o: DeliveryOutcome): DeliveryCandidateOutcome | undefined {
  return o.candidates[0];
}

/** Required criteria keeping the candidate from passing, worst first. */
export function blocking(c: DeliveryCandidateOutcome): DeliveryCriterionOutcome[] {
  const rank = ['failed', 'blocked', 'unverified', 'unrun'];
  return c.criteria.filter(x => x.required && rank.includes(x.state)).sort((a, b) => rank.indexOf(a.state) - rank.indexOf(b.state));
}

/** How a person reads the pinned spec: its title, else its artifact ID. */
export const specName = (c: { title?: string; artifactId?: string; artifact?: string; version: number } | undefined) =>
  !c ? 'the spec' : c.title ? `“${c.title}” v${c.version}` : `spec ${c.artifactId ?? c.artifact} v${c.version}`;

/** One line for a card or an inbox row: the verdict and what blocks it. */
export function summary(o: DeliveryOutcome): string {
  const c = headline(o);
  if (!c) return 'Nothing checked yet';
  const parts = [verdictLabel[c.verdict] || c.verdict];
  const open = blocking(c);
  if (open.length) parts.push(open.map(x => `${x.id} ${stateLabel[x.state] || x.state}`).join(', '));
  if (!c.current) parts.push('on earlier files');
  if (o.decision) parts.push(`you ${o.decision === 'accepted' ? 'accepted it' : 'asked for changes'}`);
  return parts.join(' · ');
}

/** What one observation says, for a chip: what ran, its state and exit. */
export function evidenceLine(e: DeliveryEvidence): string {
  const ran = e.title ? `${e.title} · ` : '';
  const what = e.source === 'review' ? 'review' : e.prerequisite ? (e.prerequisite === 'passed' ? 'prerequisite met' : 'prerequisite') : 'check';
  const exit = e.exitCode === undefined ? '' : ` · exit ${e.exitCode}`;
  const output = e.output === 'unavailable' ? ' · output no longer kept' : '';
  return `${ran}${stateLabel[e.state] || e.state} · ${what}${exit}${output}`;
}

/** A failure a later observation of the same criterion resolved. */
export const resolved = (criterion: DeliveryCriterionOutcome, index: number) =>
  (criterion.evidence[index].state === 'failed' || criterion.evidence[index].state === 'blocked') && criterion.state !== criterion.evidence[index].state;

const short = (candidate: string) => candidate.replace(/^candidate:/, '').slice(0, 8);

type OutcomeJson = {
  schema: string;
  contract: { artifact: string; title?: string; version: number; criteria?: { id: string; expected: string; required: boolean }[] | null };
  subject: { conversation: string; mission?: string; task?: string };
  current?: string; decision?: string;
  candidates: { candidate: string; current?: boolean; verdict: string; criteria: { id: string; expected: string; required: boolean; state: string; evidence?: { reference: string; title?: string; source: string; state: string; exitCode?: number; prerequisite?: string; output?: string }[] }[] }[];
};

/** A published verdict snapshot (schema delivery-outcome.v1) as a document. */
export function deliveryOutcomeMarkdown(o: OutcomeJson): string {
  const lines = [`Checked against ${specName(o.contract)}${o.subject.mission ? ` · Mission task ${o.subject.task}` : ''}`];
  if (o.decision) lines.push('', `**Decision:** ${o.decision}. The technical verdict below is unchanged.`);
  if (!o.candidates.length) lines.push('', 'Nothing was checked.');
  o.candidates.forEach((c, i) => {
    const which = c.current ? 'the workspace files when published' : i === 0 ? 'the latest files checked' : 'earlier files';
    lines.push('', `## ${verdictLabel[c.verdict] || c.verdict} on ${which} (${short(c.candidate)})`, '', '| Criterion | State | Evidence |', '|---|---|---|');
    for (const criterion of c.criteria) {
      const evidence = (criterion.evidence || []).map(e => `${stateLabel[e.state] || e.state} ${e.title ? `“${e.title}”` : e.reference}${e.exitCode === undefined ? '' : ` (exit ${e.exitCode})`}${e.output === 'unavailable' ? ', output no longer kept' : ''}`).join('; ') || '—';
      lines.push(`| **${criterion.id}**${criterion.required ? '' : ' (optional)'}: ${criterion.expected.replace(/\|/g, '\\|')} | ${stateLabel[criterion.state] || criterion.state} | ${evidence.replace(/\|/g, '\\|')} |`);
    }
  });
  return lines.join('\n');
}
