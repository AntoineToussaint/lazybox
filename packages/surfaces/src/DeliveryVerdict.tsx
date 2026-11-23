import type { DeliveryEvidence, DeliveryOutcome } from '@lazybox/protocol';
import { blocking, evidenceLine, headline, markOf, resolved, stateLabel, summary, toneOf, verdictLabel } from './delivery-model';
import './missions.css';
import './delivery.css';

type Props = {
  outcome: DeliveryOutcome;
  /** One line (a Mission card, an inbox row) or every criterion with its evidence. */
  compact?: boolean;
  /** The full view starts folded to its verdict line (a conversation, where the messages come first). */
  folded?: boolean;
  /** Opens a command run's output; absent where evidence is read-only. */
  onEvidence?(e: DeliveryEvidence): void;
};

/** The delivery verdict on the workspace's files, what blocks it and the evidence behind it. */
export function DeliveryVerdict({ outcome, compact, folded, onEvidence }: Props) {
  const c = headline(outcome);
  const tone = toneOf(c?.verdict || '');
  const spec = `spec ${outcome.contract?.artifactId} v${outcome.contract?.version}`;
  if (compact || !c) {
    return <p className={`delivery-verdict compact tone-${tone}`} data-verdict={c?.verdict || 'none'} title={`Delivery against ${spec}`}>
      <span aria-hidden="true">{markOf(tone)}</span><span>{summary(outcome)}</span>
    </p>;
  }
  const open = blocking(c);
  return <details className={`delivery-verdict tone-${tone}`} data-verdict={c.verdict} aria-label={`Delivery: ${verdictLabel[c.verdict] || c.verdict}`} open={!folded}>
    <summary>
      <span aria-hidden="true">{markOf(tone)}</span>
      <strong>{verdictLabel[c.verdict] || c.verdict}</strong>
      {open.length > 0 && <span className="delivery-blocking">Open: {open.map(x => `${x.id} (${stateLabel[x.state] || x.state})`).join(', ')}</span>}
      <span className="delivery-where">{c.current ? 'on the current files' : 'on earlier files: the current ones are not checked'} · {spec}</span>
      {outcome.decision && <span className="delivery-decision">You {outcome.decision === 'accepted' ? 'accepted it' : 'asked for changes'}; the verdict stands</span>}
    </summary>
    <ul className="delivery-criteria">{c.criteria.map(criterion => <li key={criterion.id} className={`tone-${toneOf(criterion.state)}`}>
      <span className="delivery-state">{markOf(toneOf(criterion.state))} {stateLabel[criterion.state] || criterion.state}</span>
      <span className="delivery-criterion"><strong>{criterion.id}</strong>{criterion.required ? '' : ' (optional)'}: {criterion.expected}</span>
      {criterion.evidence.length > 0 && <span className="delivery-evidence">{criterion.evidence.map((e, i) => {
        const opens = !!onEvidence && !!e.commandId && e.output !== 'unavailable';
        return <button key={e.id} type="button" className={`evidence-chip tone-${toneOf(e.state)}${resolved(criterion, i) ? ' resolved' : ''}`} disabled={!opens}
          title={`${e.reference}${resolved(criterion, i) ? '\nResolved by a later observation' : ''}${opens ? '\n\nOpen its output' : ''}`} onClick={() => onEvidence?.(e)}>
          <span aria-hidden="true">{markOf(toneOf(e.state))}</span><span className="evidence-label">{evidenceLine(e)}</span>
        </button>;
      })}</span>}
    </li>)}</ul>
    {outcome.candidates.length > 1 && <p className="delivery-history">Earlier files: {outcome.candidates.slice(1).map(x => verdictLabel[x.verdict] || x.verdict).join(', ')}</p>}
  </details>;
}
