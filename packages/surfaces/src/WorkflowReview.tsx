import { useEffect, useState } from 'react';
import { artifactsChanged } from './artifact-client';
import { useRunner } from './RunnerContext';

export function WorkflowReview({ workspace, snippet }: { workspace: string; snippet?: string }) {
  const { api } = useRunner();
  const [open, setOpen] = useState(false);
  const [keys, setKeys] = useState<string[]>([]);
  const [hourly, setHourly] = useState<boolean>();
  const [key, setKey] = useState(snippet || '');
  const [feedback, setFeedback] = useState('');
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState('');
  const [catalogError, setCatalogError] = useState('');
  useEffect(() => {
    if (!open || snippet) return;
    const controller = new AbortController();
    api.getCollection({ collectionId: workspace }, { signal: controller.signal }).then(result => {
      if (!controller.signal.aborted) setHourly(result.collection?.hourlyWorkflowReview);
    }, error => { if (!controller.signal.aborted) setMessage(String(error)); });
    api.listSnippets({}, { signal: controller.signal }).then(result => {
      if (!controller.signal.aborted) setKeys(result.snippets.map(s => s.key));
    }, error => { if (!controller.signal.aborted) setCatalogError(String(error)); });
    return () => controller.abort();
  }, [api, open, workspace, snippet]);
  const schedule = async (enabled: boolean) => {
    setBusy(true);
    try {
      const result = await api.setWorkflowReviewSchedule({collectionId: workspace, hourly: enabled});
      setHourly(result.collection?.hourlyWorkflowReview);
    } catch (error) { setMessage(String(error)); }
    finally { setBusy(false); }
  };
  const review = async () => {
    setBusy(true);
    try {
      const result = await api.reviewWorkflow({ collectionId: workspace, snippetKey: key, feedback: key ? feedback : '' });
      artifactsChanged(workspace);
      setMessage(result.proposalTitles.length ? 'See workspace Artifacts for ' + result.proposalTitles.join(', ') + '.' : 'No repeated requests found in the retained workspace history.');
      setFeedback(''); setKey(snippet || '');
    } catch (error) { setMessage(String(error)); }
    finally { setBusy(false); }
  };
  return <details className="workflow-review" open={open} onToggle={event => setOpen(event.currentTarget.open)}>
    <summary>{snippet ? 'Give feedback on this snippet' : 'Review recent work'}</summary>
    {!snippet && <>
    <p>Review human prompts in this workspace for snippet and AGENTS.md proposals. Results stay in workspace Artifacts.</p>
    <label><input type="checkbox" checked={hourly ?? false} disabled={hourly === undefined || busy} onChange={e => void schedule(e.target.checked)}/> Review this workspace hourly</label>
    <label>Feedback on a snippet <select value={key} onChange={event => setKey(event.target.value)}>
      <option value="">Review prompts only</option>{keys.map(k => <option key={k}>{k}</option>)}
    </select></label>
    {catalogError && <p role="status">{catalogError}</p>}
    </>}
    {key && <label>Suggested improvement <textarea value={feedback} onChange={event => setFeedback(event.target.value)}/></label>}
    <button type="button" disabled={busy || (!!key && !feedback.trim())} onClick={() => void review()}>{busy ? 'Reviewing…' : snippet ? 'Save feedback' : 'Run workflow review'}</button>
    {message && <p role="status">{message}</p>}
  </details>;
}
