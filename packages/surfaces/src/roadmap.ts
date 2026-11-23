export const milestoneStatuses = ['open', 'in-progress', 'blocked', 'awaiting-acceptance', 'accepted'] as const;
export interface Milestone {
  id: string;
  title: string;
  phase: string;
  owner?: string;
  status: typeof milestoneStatuses[number];
  deliverable: string;
  dependsOn: string[];
  definitionOfDone: string[];
  evidence: string[];
  openDecisions: string[];
}
export interface Roadmap { schema: 'roadmap.v1'; milestones: Milestone[] }

export function newMilestone(id: string, title: string): Milestone {
  return { id, title, phase: '', status: 'open', deliverable: '', dependsOn: [], definitionOfDone: [], evidence: [], openDecisions: [] };
}

function milestoneBrief(m: Milestone) {
  const lines = [m.deliverable, 'Milestone: ' + m.id, 'Status: ' + m.status];
  if (m.owner) lines.push('Owner: ' + m.owner);
  if (m.phase) lines.push('Phase: ' + m.phase);
  if (m.dependsOn.length) lines.push('Depends on: ' + m.dependsOn.join(', '));
  for (const [label, entries] of [['Definition of done', m.definitionOfDone], ['Verification evidence', m.evidence], ['Open decisions', m.openDecisions]] as const) {
    if (entries.length) lines.push(label + '\n\n' + entries.map(value => '- ' + value).join('\n'));
  }
  return lines.filter(Boolean).join('\n\n');
}

export function roadmapMarkdown(plan: Roadmap) {
  const phases = new Map<string, Milestone[]>();
  for (const milestone of plan.milestones) {
    const phase = milestone.phase.trim() || 'Unassigned phase';
    const group = phases.get(phase);
    if (group) group.push(milestone);
    else phases.set(phase, [milestone]);
  }
  return Array.from(phases, ([phase, milestones]) => {
    const accepted = milestones.filter(m => m.status === 'accepted').length;
    const waiting = milestones.filter(m => m.status === 'awaiting-acceptance').length;
    const progress = [accepted + ' of ' + milestones.length + ' accepted'];
    if (waiting) progress.push(waiting + ' awaiting acceptance');
    const blocked = milestones.filter(m => m.status === 'blocked').length;
    if (blocked) progress.push(blocked + ' blocked');
    const details = milestones.map(m => '### ' + m.title + '\n\n' + milestoneBrief(m));
    return ['## ' + phase, progress.join(' · '), ...details].join('\n\n');
  }).join('\n\n');
}

export function typedMissionPlan(plan: Roadmap, title: string, id: string, version: number) {
  const accepted = new Set(plan.milestones.filter(m => m.status === 'accepted').map(m => m.id));
  const remaining = plan.milestones.filter(m => !accepted.has(m.id));
  if (!remaining.length) throw new Error('Every milestone has already been accepted.');
  const tasks = remaining.map(m => {
    if (!m.deliverable.trim() || !m.definitionOfDone.length) throw new Error('Add a deliverable and definition of done for ' + m.id);
    return { key: m.id, title: m.title, after: m.dependsOn.filter(dep => !accepted.has(dep)), brief: milestoneBrief(m) + '\n\nSource roadmap: ' + id + ' v' + version };
  });
  return { summary: title, tasks };
}
