import { useState, type DragEvent } from 'react';

const isFileDrag = (event: DragEvent) => Array.from(event.dataTransfer.types).includes('Files');

/**
 * Dropping files anywhere in a chat adds them to the draft; it never sends.
 * Folders are rejected, and so is any drop the conversation cannot accept.
 */
export function useFileDrop({ canAttach, onFiles, onError }: { canAttach: boolean; onFiles(files: FileList): void; onError(message: string): void }) {
  const [dragging, setDragging] = useState(false);
  const dragOver = (event: DragEvent) => {
    if (!isFileDrag(event)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = canAttach ? 'copy' : 'none';
    setDragging(canAttach);
  };
  const drop = (event: DragEvent) => {
    if (!isFileDrag(event)) return;
    // Suppress the webview's default navigation to the dropped file, including
    // when this conversation cannot accept an attachment.
    event.preventDefault();
    event.stopPropagation();
    setDragging(false);
    if (!canAttach) { onError('This conversation cannot accept files right now.'); return; }
    if (Array.from(event.dataTransfer.items).some(item => item.webkitGetAsEntry?.()?.isDirectory)) {
      onError('Attach files individually; folders are not supported.'); return;
    }
    onFiles(event.dataTransfer.files);
  };
  const dragLeave = (event: DragEvent) => {
    const next = event.relatedTarget;
    if (!(next instanceof Node) || !event.currentTarget.contains(next)) setDragging(false);
  };
  return { dragging, handlers: { onDragEnter: dragOver, onDragOver: dragOver, onDrop: drop, onDragLeave: dragLeave } };
}
