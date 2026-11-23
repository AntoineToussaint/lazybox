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
  return plan.milestones.map(m => '## ' + m.title + '\n\n' + milestoneBrief(m)).join('\n\n');
}

export function typedMissionPlan(plan: Roadmap, title: string, id: string, version: number) {
  const tasks = plan.milestones.map(m => {
    if (!m.deliverable.trim() || !m.definitionOfDone.length) throw new Error('Add a deliverable and definition of done for ' + m.id);
    return { key: m.id, title: m.title, after: m.dependsOn, brief: milestoneBrief(m) + '\n\nSource roadmap: ' + id + ' v' + version };
  });
  return { summary: title, tasks };
}
