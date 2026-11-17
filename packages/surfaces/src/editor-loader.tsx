import { lazy, useEffect, useState } from 'react';

const load = () => import('./CodeEditor');
let started: Promise<unknown> | undefined;
/** Start fetching Monaco before anyone opens a file: the Files and Editor
 * panels call this when they mount, file links when pointed at. */
export function preloadEditor() {
 started ??= load().catch(() => { started = undefined; });
 return started;
}
/** The Monaco editor, loaded once and shared by the editor panel and the file window. */
export const CodeEditor = lazy(load);

/** What shows while Monaco or a file is still arriving: nothing for 300 ms (most
 * opens finish sooner), then a quiet outline of lines, never a "Loading" label. */
export function EditorSkeleton({ label = 'Opening editor' }: { label?: string }) {
 const [shown, setShown] = useState(false);
 useEffect(() => { const timer = setTimeout(() => setShown(true), 300); return () => clearTimeout(timer); }, []);
 return <div className="editor-skeleton" role="status" aria-busy="true" aria-label={label} data-shown={shown || undefined}>
  {shown && [62, 48, 71, 35, 56, 80, 44, 28].map((width, i) => <span key={i} style={{ width: `${width}%` }}/>)}
 </div>;
}
