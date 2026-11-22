import { useEffect, useRef, useState } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { readDraft, writeDraft, type FileReference } from './view-state';
import { draftFileId, loadDraftFiles, saveDraftFiles } from './draft-files';
import { DRAFT_TEXT_BYTES, pastedTextOf, restorePastedText } from './attachment-boxes';

/** The most pasted text one saved draft holds; the store refuses a draft over 1 MiB. */
const DRAFT_TEXT_TOTAL = 3 * DRAFT_TEXT_BYTES;

/**
 * The saved form of a draft's files: a pasted box with its text (within the
 * draft's budget), other files by metadata and a unique local byte-store reference.
 */
export function fileReferences(files: readonly File[]): FileReference[] {
  let budget = DRAFT_TEXT_TOTAL;
  return files.map(file => {
    const text = pastedTextOf(file);
    const keep = text !== undefined && text.length <= DRAFT_TEXT_BYTES && text.length <= budget;
    if (keep) budget -= text.length;
    return { name: file.name, size: file.size, lastModified: file.lastModified, ...(keep ? { text } : { blobId: draftFileId(file) }) };
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

  const [restoring, setRestoring] = useState(false);
  const saved = useRef<{ destination: string; files: File[] } | undefined>(undefined);
  useEffect(() => {
    let active = true;
    saved.current = undefined;
    async function restore() {
      const draft = readDraft(viewState, sessionID);
      setMessage(draft.text);
      setUncertainSend(draft.operationId);
      sendAttempt.current = null;
      let blobs = new Map<string, File>();
      if (viewState && draft.files.some(ref => ref.blobId)) {
        setRestoring(true);
        try { blobs = await loadDraftFiles(viewState, sessionID); } catch { /* Metadata still permits explicit reattachment. */ }
      }
      if (!active) return;
      const found: File[] = [], missing: FileReference[] = [];
      for (const ref of draft.files) {
        const file = ref.text !== undefined ? restorePastedText(ref.name, ref.text, ref.lastModified) : blobs.get(ref.blobId || '');
        if (file) found.push(file); else missing.push(ref);
      }
      setFiles(found); setMissingFiles(missing); setRestoring(false);
    }
    void restore().catch(error => { if (active) { setRestoring(false); onError(String(error)); } });
    return () => { active = false; };
  }, [viewState, sessionID]);

  const persistDraft = (text: string, selected: File[], missing: FileReference[], operationId = '', destination = sessionID) => {
    try {
      const refs = fileReferences(selected);
      writeDraft(viewState, destination, { text, files: [...refs, ...missing], operationId });
      if (!viewState) return;
      const binary = selected.filter((_, i) => refs[i].blobId);
      const previous = saved.current;
      if (previous?.destination === destination && previous.files.length === binary.length && binary.every((file, i) => previous.files[i] === file)) return;
      const snapshot = { destination, files: binary };
      saved.current = snapshot;
      void saveDraftFiles(viewState, destination, binary).catch(() => {
        if (saved.current !== snapshot) return;
        saved.current = undefined;
        if (binary.length) onError('Attachment available in this window only. Local draft storage is unavailable.');
      });
    } catch (e) { onError(`Could not save this window's draft: ${String(e)}`); }
  };

  return { restoring, message, setMessage, files, setFiles, missingFiles, setMissingFiles, uncertainSend, setUncertainSend, sendAttempt, persistDraft };
}
