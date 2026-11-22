import { useEffect, useRef, useState } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { readDraft, writeDraft, type FileReference } from './view-state';
import { draftFileId, loadDraftFiles, saveDraftFiles } from './draft-files';
import { DRAFT_TEXT_BYTES, pastedTextOf, restorePastedText } from './attachment-boxes';

/** Keep small pastes inline as long as the serialized draft fits the native view store. */
function fileReferences(files: readonly File[], text: string, missing: FileReference[], operationId: string): FileReference[] {
  const refs: FileReference[] = files.map(file => ({
    name: file.name, size: file.size, lastModified: file.lastModified, blobId: draftFileId(file),
  }));
  if (!files.some(file => pastedTextOf(file) !== undefined)) return refs;
  const encoder = new TextEncoder();
  const encodedSize = (value: unknown) => encoder.encode(JSON.stringify(value)).length;
  let available = DRAFT_TEXT_BYTES - encodedSize({ text, files: [...refs, ...missing], operationId });
  for (const [index, file] of files.entries()) {
    const pasted = pastedTextOf(file);
    if (pasted === undefined || pasted.length > available) continue;
    const { blobId, ...metadata } = refs[index];
    const inline = { ...metadata, text: pasted };
    const added = encodedSize(inline) - encodedSize(refs[index]);
    if (added <= available) { refs[index] = inline; available -= added; }
  }
  return refs;
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
      setFiles([]); setMissingFiles([]);
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
      if (!missing.length) {
        saved.current = { destination: sessionID, files: found.filter((_, i) => draft.files[i].blobId) };
      }
      setFiles(found); setMissingFiles(missing); setRestoring(false);
    }
    void restore().catch(error => {
      if (!active) return;
      setMessage(''); setFiles([]); setMissingFiles([]); setUncertainSend('');
      sendAttempt.current = null;
      setRestoring(false); onError(String(error));
    });
    return () => { active = false; };
  }, [viewState, sessionID]);

  const persistDraft = (text: string, selected: File[], missing: FileReference[], operationId = '', destination = sessionID) => {
    try {
      const refs = fileReferences(selected, text, missing, operationId);
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
