import { useEffect, useRef, useState, type RefObject } from 'react';
import { inheritKey } from './attachment-images';
import { admitFiles, insertText, moveItem, pastedTextFile, type AttachmentLimits, type RejectedFile } from './attachment-boxes';
import type { FileReference } from './view-state';

/** The draft state the boxes change, owned by the chat (useComposerDraft). */
type Draft = {
  message: string; setMessage(text: string): void;
  files: File[]; setFiles(files: File[]): void;
  missingFiles: FileReference[]; setMissingFiles(files: FileReference[]): void;
  uncertainSend: string; setUncertainSend(id: string): void;
  sendAttempt: RefObject<unknown>;
  persistDraft(text: string, files: File[], missing: FileReference[], operationId?: string): void;
};

/** A paste just made into a box, which ⌘Z right after puts back as text. */
type PasteUndo = { file: File; text: string; start: number; end: number; message: string };

/**
 * The attachment boxes of a message being written: files chosen, dropped or
 * pasted, a large text paste as a box of its own, the error boxes of files
 * over a limit, and the box actions (remove, move, edit, insert inline, undo
 * a paste). The Runner's limits apply to every box alike: a pasted text is
 * one attachment.
 */
export function useComposerAttachments({ draft, limits, canAttach, textarea, sessionID, onError }: { draft: Draft; limits?: { maxFiles: number; maxFileBytes: bigint; maxMessageBytes: bigint }; canAttach: boolean; textarea: RefObject<HTMLTextAreaElement | null>; sessionID: string; onError(message: string): void }) {
  const { message, setMessage, files, setFiles, missingFiles, setMissingFiles, uncertainSend, setUncertainSend, sendAttempt, persistDraft } = draft;
  const [rejected, setRejected] = useState<RejectedFile[]>([]);
  const pasteUndo = useRef<PasteUndo | null>(null);
  useEffect(() => { setRejected([]); pasteUndo.current = null; }, [sessionID]);
  const bounds: AttachmentLimits | undefined = limits && { maxFiles: limits.maxFiles, maxFileBytes: Number(limits.maxFileBytes), maxMessageBytes: Number(limits.maxMessageBytes) };
  // A changed set of files is a new message: a retry of the last send no longer applies.
  const changed = (next: File[], missing = missingFiles, text = message) => { setFiles(next); setUncertainSend(''); sendAttempt.current = null; persistDraft(text, next, missing); };
  const caret = (at: number) => requestAnimationFrame(() => { const node = textarea.current; if (node) { node.focus(); node.setSelectionRange(at, at); } });

  /** Files chosen, dropped or pasted: those that fit are attached, the rest become error boxes. */
  const select = (selected: FileList | File[] | null) => {
    if (!bounds) return;
    const incoming = Array.from(selected || []);
    // A file a reloaded draft names, chosen again, takes its place.
    const remaining = [...missingFiles];
    let fresh = false;
    for (const file of incoming) {
      const index = remaining.findIndex(saved => saved.name === file.name && saved.size === file.size && saved.lastModified === file.lastModified);
      if (index >= 0) remaining.splice(index, 1); else fresh = true;
    }
    // Different client files can share a name, size, and modification time.
    // Keep the selected bytes; the Runner deduplicates by content.
    const { accepted, rejected: over } = admitFiles([...files, ...remaining], incoming, bounds);
    setRejected(current => [...current, ...over]);
    if (!accepted.length && remaining.length === missingFiles.length) return;
    const next = [...files, ...accepted];
    if (fresh) { setUncertainSend(''); sendAttempt.current = null; }
    setFiles(next); setMissingFiles(remaining); onError(''); persistDraft(message, next, remaining, fresh ? '' : uncertainSend);
  };

  /**
   * A large text paste as a box; false leaves the paste to the message box
   * (the conversation takes no file now, or one more would cross a limit).
   */
  const pasteText = (text: string, start: number, end: number): boolean => {
    if (!bounds || !canAttach) return false;
    const file = pastedTextFile(text);
    if (admitFiles([...files, ...missingFiles], [file], bounds).rejected.length) return false;
    changed([...files, file]);
    pasteUndo.current = { file, text, start, end, message };
    return true;
  };

  /** ⌘Z right after a paste became a box: the text goes where it was pasted instead. */
  const undoPaste = (): boolean => {
    const last = pasteUndo.current;
    pasteUndo.current = null;
    if (!last || last.message !== message || !files.includes(last.file)) return false;
    const { message: next, caret: at } = insertText(message, last.text, last.start, last.end);
    setMessage(next); changed(files.filter(file => file !== last.file), missingFiles, next); caret(at);
    return true;
  };

  /** A box's text put in the message at the caret, instead of attached. */
  const inline = (index: number, text: string) => {
    const node = textarea.current;
    const { message: next, caret: at } = insertText(message, text, node?.selectionStart ?? message.length, node?.selectionEnd ?? message.length);
    setMessage(next); changed(files.filter((_, i) => i !== index), missingFiles, next); caret(at);
  };

  return {
    files, rejected, select, pasteText, undoPaste, inline,
    remove: (index: number) => changed(files.filter((_, i) => i !== index)),
    move: (from: number, to: number) => changed(moveItem(files, from, to)),
    replace: (index: number, file: File) => { inheritKey(files[index], file); changed(files.map((current, i) => i === index ? file : current)); },
    dismiss: (index: number) => setRejected(current => current.filter((_, i) => i !== index)),
    /** After a send: the error boxes went with the message that left them out. */
    clearRejected: () => { setRejected([]); pasteUndo.current = null; },
  };
}

export type ComposerAttachments = ReturnType<typeof useComposerAttachments>;
