import { useEffect, useRef, useState } from 'react';
import { Excalidraw, convertToExcalidrawElements, exportToBlob, exportToCanvas, getCommonBounds, loadFromBlob, MainMenu } from '@excalidraw/excalidraw';
import type { BinaryFileData, ExcalidrawImperativeAPI, ExcalidrawInitialDataState } from '@excalidraw/excalidraw/types';
import '@excalidraw/excalidraw/index.css';
import { getAppearance } from './appearance/apply';
import type { Rendered } from './diagram-svg';

async function initialScene(diagram?: Rendered, file?: File): Promise<ExcalidrawInitialDataState> {
  if (file?.name.endsWith('.excalidraw.png')) return { ...await loadFromBlob(file, null, null), scrollToContent: true };
  if (!diagram && !file) return {};
  const image = new Image();
  image.src = diagram?.src ?? URL.createObjectURL(file!);
  try {
    await image.decode();
    const scale = Math.min(1, 4096 / Math.max(image.naturalWidth, image.naturalHeight));
    const canvas = document.createElement('canvas');
    canvas.width = Math.max(1, Math.round(image.naturalWidth * scale));
    canvas.height = Math.max(1, Math.round(image.naturalHeight * scale));
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('Image canvas unavailable.');
    if (diagram) {
      ctx.fillStyle = getAppearance().theme.tokens['surface-sunken'];
      ctx.fillRect(0, 0, canvas.width, canvas.height);
    }
    ctx.drawImage(image, 0, 0, canvas.width, canvas.height);
    const id = crypto.randomUUID() as BinaryFileData['id'];
    const dataURL = canvas.toDataURL('image/png') as BinaryFileData['dataURL'];
    return { elements: convertToExcalidrawElements([{ type: 'image', x: 0, y: 0, width: canvas.width, height: canvas.height, fileId: id, locked: true, status: 'saved' }]),
      appState: { currentItemStrokeColor: '#e03131' },
      files: { [id]: { id, dataURL, mimeType: 'image/png', created: Date.now() } }, scrollToContent: true };
  } finally { if (file) URL.revokeObjectURL(image.src); }
}

export default function DrawingEditor({ diagram, file, onAttach, onMask, onError }: {
  diagram?: Rendered; file?: File; onAttach(file: File): void; onMask?(file: File): void; onError(message: string): void;
}) {
  const [scene, setScene] = useState<ExcalidrawInitialDataState>();
  const [api, setApi] = useState<ExcalidrawImperativeAPI>();
  const [saving, setSaving] = useState(false);
  const mounted = useRef(false);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);
  useEffect(() => {
    let active = true;
    initialScene(diagram, file).then(data => { if (active) setScene(data); }, error => { if (active) onError(String(error)); });
    return () => { active = false; };
  }, [diagram, file, onError]);
  useEffect(() => {
    api?.setActiveTool({ type: 'freedraw' });
  }, [api]);
  async function attach() {
    if (!api || saving) return;
    const elements = api.getSceneElements();
    if (!elements.length) { onError('Draw something before attaching.'); return; }
    setSaving(true);
    try {
      const blob = await exportToBlob({ elements, files: api.getFiles(), appState: { ...api.getAppState(), exportEmbedScene: true, exportBackground: true }, maxWidthOrHeight: 4096 });
      if (mounted.current) onAttach(new File([blob], diagram ? 'diagram.excalidraw.png' : 'drawing.excalidraw.png', { type: 'image/png' }));
    } catch (error) { onError(String(error)); } finally { setSaving(false); }
  }
  async function attachMask() {
    if (!api || !file || !onMask || saving) return;
    setSaving(true);
    const original = new Image();
    original.src = URL.createObjectURL(file);
    try {
      const elements = api.getSceneElements();
      const base = elements.find(element => element.type === 'image' && element.locked);
      const marks = elements.filter(element => element !== base);
      if (!base || !marks.length) throw new Error('Brush over the area to regenerate first.');
      await original.decode();
      const mask = document.createElement('canvas');
      mask.width = original.naturalWidth;
      mask.height = original.naturalHeight;
      const ctx = mask.getContext('2d');
      if (!ctx) throw new Error('Image canvas unavailable.');
      ctx.fillRect(0, 0, mask.width, mask.height);
      ctx.scale(mask.width / base.width, mask.height / base.height);
      ctx.globalCompositeOperation = 'destination-out';
      const [left, top, right, bottom] = getCommonBounds(marks);
      const ink = await exportToCanvas({ elements: marks, files: api.getFiles(), exportPadding: 0,
        maxWidthOrHeight: 4096, appState: { exportBackground: false } });
      ctx.drawImage(ink, left - base.x, top - base.y, right - left, bottom - top);
      const blob = await new Promise<Blob | null>(resolve => mask.toBlob(resolve, 'image/png'));
      if (!blob) throw new Error('Could not export the edit mask.');
      if (mounted.current) onMask(new File([blob], 'edit-mask.png', { type: 'image/png' }));
    } catch (error) { onError(String(error)); }
    finally { URL.revokeObjectURL(original.src); setSaving(false); }
  }
  return <><div style={{ flex: 1, minHeight: 0 }}>{scene && <Excalidraw initialData={scene} excalidrawAPI={setApi}
    UIOptions={{ canvasActions: { export: false, saveAsImage: false } }}><MainMenu><MainMenu.DefaultItems.ClearCanvas/></MainMenu></Excalidraw>}</div>
    {onMask && <button type="button" disabled={!api || saving} onClick={() => void attachMask()}
      title="Keep the original picture and attach a same-size PNG mask. Marks identify pixels to regenerate.">Attach marks as edit mask</button>}
    <button type="button" className="primary" disabled={!api || saving} onClick={() => void attach()}>{saving ? 'Preparing image…' : 'Attach drawing'}</button></>;
}
