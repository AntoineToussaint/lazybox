import { createContext, lazy, Suspense, useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { Icon } from './Icons';
import './drawing.css';
import type { Rendered } from './diagram-svg';

declare global { interface Window { EXCALIDRAW_ASSET_PATH?: string } }

export const DiagramAnnotation = createContext<((file: File, source: string) => void) | undefined>(undefined);
const Editor = lazy(async () => {
  window.EXCALIDRAW_ASSET_PATH = new URL('./drawing-assets/', document.baseURI).href;
  return import('./DrawingEditor');
});

export function Drawing({ diagram, file, onAttach, onMask, onClose }: {
  diagram?: Rendered; file?: File; onAttach(file: File): void; onMask?(file: File): void; onClose(): void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [error, setError] = useState('');
  useEffect(() => {
    const node = dialog.current!;
    node.showModal();
    return () => node.close();
  }, []);
  return createPortal(<dialog ref={dialog} className="drawing-dialog" aria-label="Drawing editor"
    onCancel={event => event.preventDefault()}>
    <div className="drawing-layout">
      <strong className="drawing-title">{diagram ? 'Annotate diagram' : file ? 'Edit drawing' : 'Drawing'}</strong>
      <button type="button" className="drawing-close" aria-label="Cancel" title="Cancel" onClick={onClose}><Icon name="close"/></button>
      {error && <p className="drawing-error" role="alert">{error}</p>}
      <Suspense fallback={<p className="drawing-canvas" role="status">Loading drawing editor…</p>}>
        <Editor diagram={diagram} file={file} onAttach={onAttach} onMask={onMask} onError={setError}/>
      </Suspense>
    </div>
  </dialog>, document.body);
}
