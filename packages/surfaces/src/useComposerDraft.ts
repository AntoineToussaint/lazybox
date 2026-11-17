import { useEffect, useRef, useState } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { readDraft, writeDraft, type FileReference } from './view-state';
import { DRAFT_TEXT_BYTES, pastedTextOf, restorePastedText } from './attachment-boxes';

/** The most pasted text one saved draft holds; the store refuses a draft over 1 MiB. */
const DRAFT_TEXT_TOTAL = 3 * DRAFT_TEXT_BYTES;

/**
 * The saved form of a draft's files: a pasted box with its text (within the
 * draft's budget), any other file by name, size and time.
 */
export function fileReferences(files: readonly File[]): FileReference[] {
  let budget = DRAFT_TEXT_TOTAL;
  return files.map(file => {
    const text = pastedTextOf(file);
    const keep = text !== undefined && text.length <= DRAFT_TEXT_BYTES && text.length <= budget;
    if (keep) budget -= text.length;
    return { name: file.name, size: file.size, lastModified: file.lastModified, ...(keep ? { text } : {}) };
  });
}

/** The message and files of a send whose delivery is not yet confirmed. */
export type SendAttempt = { id: string; text: string; files: File[] };

/**
 * The composer's draft for one conversation: its text, the files attached in
 * this window, the files a restored draft names but this window no longer
 * holds, and the operation identity of a send that was not confirmed. Every
 * edit persists it, so the draft survives this view remounting.
 */
export function useComposerDraft(viewState: ViewStateStore | undefined, sessionID: string, onError: (message: string) => void) {
  const [message, setMessage] = useState('');
  const [files, setFiles] = useState<File[]>([]);
  const [missingFiles, setMissingFiles] = useState<FileReference[]>([]);
  const [uncertainSend, setUncertainSend] = useState('');
  const sendAttempt = useRef<SendAttempt | null>(null);

  useEffect(() => {
    try {
      const draft = readDraft(viewState, sessionID);
      // A pasted box comes back with its text; other files are attached again.
      setMessage(draft.text); setMissingFiles(draft.files.filter(file => file.text === undefined));
      setFiles(draft.files.flatMap(file => file.text === undefined ? [] : [restorePastedText(file.name, file.text, file.lastModified)])); setUncertainSend(draft.operationId); sendAttempt.current = null;
    } catch (e) { onError(`Could not restore this window's draft: ${String(e)}`); }
  }, [viewState, sessionID]);

  const persistDraft = (text: string, selected: File[], missing: FileReference[], operationId = '') => {
    try { writeDraft(viewState, sessionID, { text, files: [...fileReferences(selected), ...missing], operationId }); }
    catch (e) { onError(`Could not save this window's draft: ${String(e)}`); }
  };

  return { message, setMessage, files, setFiles, missingFiles, setMissingFiles, uncertainSend, setUncertainSend, sendAttempt, persistDraft };
}
