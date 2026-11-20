import { useEffect, useRef, useState } from 'react';
import { ActivityTime, validActivityTime } from './ActivityTime';
import { DialogHeader, DraggableDialog } from './DraggableDialog';
import { isSentPrompt } from './prompt-history';
import { useRunner } from './RunnerContext';
import type { TranscriptMessage } from './transcript-model';
import './prompt-history.css';

type Entry = { id: string; text: string; at: number };

export function PromptHistoryDialog({ conversationId, messages, onClose, onPick }: {
  conversationId: string;
  messages: readonly TranscriptMessage[];
  onClose(): void;
  onPick(text: string): void;
}) {
  const { api } = useRunner();
  const dialog = useRef<HTMLDialogElement>(null);
  const [remote, setRemote] = useState<Entry[]>([]);
  const [query, setQuery] = useState('');
  const [loading, setLoading] = useState(true);
  const [failed, setFailed] = useState(false);
  useEffect(() => { dialog.current?.showModal(); }, []);
  useEffect(() => {
    const controller = new AbortController();
    void api.listPromptHistory({ conversationId, limit: 200 }, { signal: controller.signal, timeoutMs: 5000 })
      .then(result => { if (!controller.signal.aborted) setRemote(result.prompts.filter(prompt => isSentPrompt(prompt.source?.kind)).map((prompt, index) => ({
        id: `runner-${index}`, text: prompt.text, at: Number(prompt.atUnixMillis),
      }))); }, () => { if (!controller.signal.aborted) setFailed(true); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [api, conversationId]);
  const sent = messages.filter(message => message.type === 'operation_accepted' && !message.origin && !message.command)
    .map(message => ({ id: message.id, text: message.text, at: message.at || 0 }));
  const logged = new Set(remote.map(entry => JSON.stringify([entry.text, entry.at])));
  const results = [...remote, ...sent.filter(entry => !logged.has(JSON.stringify([entry.text, entry.at])))]
    .filter(entry => entry.text.trim() && entry.text.toLowerCase().includes(query.trim().toLowerCase()))
    .sort((a, b) => b.at - a.at);
  return <DraggableDialog ref={dialog} className="prompt-history-dialog" aria-label="Prompt history" onCancel={onClose}>
    <DialogHeader><h2>Prompt history</h2><button type="button" aria-label="Close prompt history" onClick={onClose}>×</button></DialogHeader>
    <input type="search" aria-label="Search prompts" placeholder="Search this conversation’s prompts…" value={query} onChange={event => setQuery(event.target.value)} autoFocus/>
    {failed && <p role="status">Showing prompts from the loaded transcript. The Runner’s history could not be loaded.</p>}
    <ul>{results.map(entry => <li key={entry.id}>
      <button type="button" onClick={() => onPick(entry.text)} title="Use this prompt in the composer">{entry.text}</button>
      {validActivityTime(entry.at) && <ActivityTime at={entry.at}/>}
    </li>)}</ul>
    {!results.length && <p role="status">{loading ? 'Loading prompts…' : query ? 'No matching prompts.' : 'No prompts sent yet.'}</p>}
  </DraggableDialog>;
}
