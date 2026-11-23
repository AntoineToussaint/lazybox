import { useMemo, type ComponentProps } from 'react';
import { DocumentBody } from './ArtifactReader';
import { plainText, slugger, type TocEntry } from './artifact-reader';
import { roadmapPhases, type Roadmap, type Milestone } from './roadmap';

const statusLabel: Record<Milestone['status'], string> = {
  open: 'Open', 'in-progress': 'In progress', blocked: 'Blocked', 'awaiting-acceptance': 'Awaiting acceptance', accepted: 'Accepted',
};

export function roadmapOutline(plan: Roadmap): TocEntry[] {
  const slug = slugger(), entries: TocEntry[] = [];
  const add = (label: string, level: number) => {
    const text = plainText(label) || label;
    entries.push({ text, level, id: slug(text), line: entries.length + 1 });
  };
  for (const [phase, milestones] of roadmapPhases(plan)) {
    add(phase, 2);
    for (const m of milestones) add(m.title, 3);
  }
  return entries;
}

type ReaderProps = Pick<ComponentProps<typeof DocumentBody>, 'scroller' | 'onReviewShell'>;

export function RoadmapDocument({ plan, headings, ...reader }: ReaderProps & { plan: Roadmap; headings: TocEntry[] }) {
  const phases = useMemo(() => roadmapPhases(plan), [plan]);
  const targets = new Map<string, TocEntry>();
  const phaseHeadings = headings.filter(h => h.level === 2);
  const milestoneHeadings = headings.filter(h => h.level === 3);
  let index = 0;
  for (const milestones of phases.values()) for (const m of milestones) targets.set(m.id, milestoneHeadings[index++]);
  return <div className="doc-body roadmap-document">{Array.from(phases, ([phase, milestones], phaseIndex) => <section key={phase} aria-label={phase}>
    <header className="roadmap-phase">
      <h2 className="doc-heading" tabIndex={-1} id={phaseHeadings[phaseIndex].id}>{phase}</h2>
      <span>{milestones.filter(m => m.status === 'accepted').length} of {milestones.length} accepted</span>
    </header>
    {milestones.map(m => <article className="roadmap-milestone" key={m.id} aria-labelledby={targets.get(m.id)?.id}>
      <header><h3 className="doc-heading" id={targets.get(m.id)?.id} tabIndex={-1}>{m.title}</h3><span className={'roadmap-state state-' + m.status}>{statusLabel[m.status]}</span></header>
      <p className="roadmap-identity">{[m.id, m.owner].filter(Boolean).join(' · ')}</p>
      {m.deliverable && <DocumentBody markdown={m.deliverable} idPrefix={'deliverable-' + m.id + '-'} {...reader}/>}
      {m.dependsOn.length > 0 && <p className="roadmap-dependencies">Depends on {m.dependsOn.map((id, i) => {
        const target = targets.get(id);
        return <span key={id + i}>{i > 0 && ', '}{target ? <a href={'#' + target.id} onClick={event => {
          event.preventDefault();
          const heading = reader.scroller?.current?.querySelector<HTMLElement>('#' + CSS.escape(target.id));
          heading?.scrollIntoView({ block: 'start' });
          heading?.focus({ preventScroll: true });
        }}>{target.text}</a> : id}</span>;
      })}</p>}
      {([['Definition of done', m.definitionOfDone], ['Verification evidence', m.evidence], ['Open decisions', m.openDecisions]] as const).map(([label, entries], i) =>
        entries.length > 0 && <section className="roadmap-detail" key={label} aria-label={label}><h4>{label}</h4>
          <DocumentBody markdown={entries.map(v => '- ' + v).join('\n')} idPrefix={'detail-' + m.id + '-' + i + '-'} {...reader}/>
        </section>)}
    </article>)}
  </section>)}</div>;
}
