import { createContext, lazy, Suspense, useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
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
  return createPortal(<dialog ref={dialog} aria-label="Drawing editor" style={{ width: 'min(96vw, 1200px)', height: '90dvh', padding: 8 }}
    onCancel={event => event.preventDefault()}>
    <div style={{ height: '100%', display: 'flex', flexDirection: 'column' }}>
      <div className="row"><strong>{diagram ? 'Annotate diagram' : 'Drawing'}</strong>
        <button type="button" onClick={onClose}>Cancel</button></div>
      {error && <p role="alert">{error}</p>}
      <Suspense fallback={<p role="status">Loading drawing editor…</p>}>
        <Editor diagram={diagram} file={file} onAttach={onAttach} onMask={onMask} onError={setError}/>
      </Suspense>
    </div>
  </dialog>, document.body);
}
