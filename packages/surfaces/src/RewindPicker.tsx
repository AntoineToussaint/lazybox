import { useEffect, useRef, useState } from 'react';
import { DialogHeader, DraggableDialog } from './DraggableDialog';
import { canRestartPrompt, type TranscriptMessage } from './transcript-model';

export function RewindPicker({ messages, loading, onClose, onPick }: {
  loading: boolean;
  messages: readonly TranscriptMessage[];
  onClose(): void;
  onPick(message: TranscriptMessage): void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [query, setQuery] = useState('');
  const prompts = messages.filter(m => m.type === 'operation_accepted' && !m.origin && !m.command && m.text.toLowerCase().includes(query.toLowerCase())).reverse();
  useEffect(() => {
    dialog.current?.showModal();
    dialog.current?.querySelector<HTMLButtonElement>('li button:enabled')?.focus();
  }, []);
  return <DraggableDialog ref={dialog} aria-label="Restart from a prompt" className="prompt-history-dialog" onCancel={onClose}>
    <DialogHeader><h2>Restart from a prompt</h2><button onClick={onClose} aria-label="Close">×</button></DialogHeader>
    <input type="search" aria-label="Find a prompt" value={query} onChange={event => setQuery(event.target.value)}/>
    {loading && <p role="status">Loading earlier prompts…</p>}
    <ul onKeyDown={event => {
      if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return;
      event.preventDefault();
      const items = Array.from(event.currentTarget.querySelectorAll<HTMLButtonElement>('button:enabled'));
      const next = items.indexOf(document.activeElement as HTMLButtonElement) + (event.key === 'ArrowDown' ? 1 : -1);
      items[Math.max(0, Math.min(next, items.length - 1))]?.focus();
    }}>{prompts.map(prompt => <li key={prompt.id}><button disabled={!canRestartPrompt(prompt)} onClick={() => onPick(prompt)}>{prompt.text}</button>
      <small>{prompt.at ? new Date(prompt.at).toLocaleString() + ' · ' : ''}Message {prompt.id}
        {prompt.steered ? ' · Sent during another turn' : prompt.files?.length ? ' · Contains attachments' : !prompt.operationId ? ' · Missing operation ID' : ''}</small></li>)}</ul>
    {!prompts.length && <p>No eligible prompts in the loaded conversation.</p>}
  </DraggableDialog>;
}
