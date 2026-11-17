import { useCallback, useEffect, useLayoutEffect, useRef, useState, type CSSProperties, type HTMLAttributes } from 'react';
import { createPortal } from 'react-dom';
import { ImageCache, PREVIEWABLE, formatBytes, loadImage, placePreview, releaseImage, type LoadedImage, type PreviewPlacement } from './attachment-images';
import { SENT_TEXT_PREVIEW_BYTES, boxIcon, boxKind, boxSubtitle, deliveryNote, isPastedName, languageOfName, pastedTextOf, pdfPageCount, replaceText, textSummary } from './attachment-boxes';
import { ActionMenu } from './ActionMenu';
import { CardBox, DetailsPreview, HOVER_DELAY_MS, TextPreview, TextViewer, boxKeys } from './AttachmentBox';
import { ImageLightbox } from './ImageLightbox';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import './attachments.css';

/** The preview fits in this share of the window. */
const PREVIEW_SHARE = 0.6;
/** The preview card's padding, border and caption around the image (attachments.css). */
const CARD_CHROME = { width: 14, height: 40 };

type Loading = { state: 'loading' } | { state: 'ready'; image: LoadedImage } | { state: 'failed'; reason: string };

/**
 * A picture-sized button for one image. Resting the pointer on it, or moving
 * keyboard focus to it, shows the image larger in a floating card; clicking
 * (or Enter) opens it full size.
 */
function ImageThumbnail({ name, size, loading, variant, note, onRemove, onMove }: { name: string; size: number; loading: Loading; variant: 'composer' | 'sent'; note?: string; onRemove?(): void; onMove?(by: -1 | 1): void }) {
  const button = useRef<HTMLButtonElement>(null);
  const [preview, setPreview] = useState(false);
  const [open, setOpen] = useState(false);
  const timer = useRef(0);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  const show = (delay: number) => { window.clearTimeout(timer.current); timer.current = window.setTimeout(() => setPreview(true), delay); };
  const hide = useCallback(() => { window.clearTimeout(timer.current); setPreview(false); }, []);
  const image = loading.state === 'ready' ? loading.image : undefined;
  // A sent image keeps its shape at a fixed height; a composer chip is a square.
  const style: CSSProperties | undefined = variant === 'sent' && image?.width && image.height ? { width: `${Math.min(200, Math.max(48, Math.round(88 * image.width / image.height)))}px` } : undefined;
  const label = loading.state === 'failed' ? `${name}, ${formatBytes(size)} · preview unavailable` : `${name}, ${formatBytes(size)}`;
  return <>
    <button ref={button} type="button" className={`attachment-thumb ${variant === 'composer' ? 'draft-thumb' : 'sent-thumb'}`} data-state={loading.state} style={style}
      aria-label={`Open ${label}`} aria-haspopup="dialog" disabled={!image} title={note}
      onPointerEnter={event => { if (event.pointerType === 'mouse') show(HOVER_DELAY_MS); }} onPointerLeave={hide}
      onFocus={event => { if (event.currentTarget.matches(':focus-visible')) show(0); }} onBlur={hide}
      onKeyDown={boxKeys({ onRemove, onMove, preview, hide })}
      onClick={() => { hide(); if (image) setOpen(true); }}>
      {image ? <img src={image.thumb} alt="" decoding="async" draggable={false}/> : <span className="attachment-thumb-placeholder" aria-hidden="true"><Icon name="image" size={variant === 'sent' ? 20 : 18}/></span>}
    </button>
    {preview && image && button.current && <ImagePreview anchor={button.current} image={image} name={name} onClose={hide}/>}
    {open && image && <ImageLightbox image={image} name={name} onClose={() => setOpen(false)}/>}
  </>;
}

/**
 * The enlarged image beside its thumbnail (placePreview says where). It never
 * takes the pointer, so it cannot cover what it previews and trap the hover;
 * it follows the window's size and closes when what is under it scrolls.
 */
function ImagePreview({ anchor, image, name, onClose }: { anchor: HTMLElement; image: LoadedImage; name: string; onClose(): void }) {
  // An image that could not be decoded ahead is measured once it shows.
  const [natural, setNatural] = useState<{ width: number; height: number }>({ width: image.width, height: image.height });
  const [placement, setPlacement] = useState<PreviewPlacement | null>(null);
  useLayoutEffect(() => {
    if (!natural.width) return;
    const place = () => setPlacement(placePreview(anchor.getBoundingClientRect(), natural, { width: window.innerWidth, height: window.innerHeight }, { share: PREVIEW_SHARE, chrome: CARD_CHROME }));
    place();
    window.addEventListener('resize', place);
    return () => window.removeEventListener('resize', place);
  }, [anchor, natural]);
  useEffect(() => {
    const scrolled = (event: Event) => { if (event.target instanceof Node && event.target.contains(anchor)) onClose(); };
    window.addEventListener('scroll', scrolled, true);
    return () => window.removeEventListener('scroll', scrolled, true);
  }, [anchor, onClose]);
  const details = [natural.width && natural.height ? `${natural.width} × ${natural.height}` : '', formatBytes(image.size)].filter(Boolean).join(' · ');
  return createPortal(<div className="attachment-preview" role="tooltip" data-placed={placement ? placement.side : undefined}
    style={placement ? { left: placement.left, top: placement.top } : undefined}>
    <img src={image.url} alt="" decoding="async" style={placement ? { width: placement.width, height: placement.height } : undefined}
      onLoad={event => { if (!natural.width) setNatural({ width: event.currentTarget.naturalWidth, height: event.currentTarget.naturalHeight }); }}/>
    <div className="attachment-preview-caption"><span>{name}</span><small>{details}</small></div>
  </div>, document.body);
}

/** What a composer box can do besides being removed. */
type ComposerBoxActions = { item?: HTMLAttributes<HTMLLIElement> & { 'data-dragging'?: boolean }; disabled?: boolean; onRemove(): void; onMove?(by: -1 | 1): void; onReplace?(file: File): void; onInline?(text: string): void };

/**
 * One file attached to the message being written, as its box: an image as
 * its thumbnail, text (a large paste, a source file) with its first line and
 * a preview, a PDF with its pages, anything else by name and size.
 */
export function ComposerAttachment({ file, ...actions }: { file: File } & ComposerBoxActions) {
  const kind = boxKind(file);
  if (kind === 'image') return <ComposerImage file={file} {...actions}/>;
  if (kind === 'text') return <ComposerText file={file} {...actions}/>;
  return <ComposerFile file={file} kind={kind} {...actions}/>;
}

function ComposerImage({ file, item, disabled, onRemove, onMove }: { file: File } & ComposerBoxActions) {
  const [loading, setLoading] = useState<Loading>({ state: 'loading' });
  useEffect(() => {
    let alive = true, loaded: LoadedImage | undefined;
    setLoading({ state: 'loading' });
    void loadImage(file).then(value => { loaded = value; if (alive) setLoading({ state: 'ready', image: value }); else releaseImage(value); });
    return () => { alive = false; if (loaded) releaseImage(loaded); };
  }, [file]);
  // A file that says it is an image but does not decode is a named box, not a broken picture.
  if (loading.state === 'ready' && !loading.image.width) return <li {...item} className="attachment-chip box" data-kind="file">
    <CardBox icon="image" title={file.name} subtitle={formatBytes(file.size)} label={`${file.name}, ${formatBytes(file.size)}`} variant="composer" note="This image could not be previewed; it is still sent"
      preview={<DetailsPreview name={file.name} lines={[`${file.type || 'Image'} · ${formatBytes(file.size)}`, 'It could not be previewed; it is still sent.']}/>}
      disabled={disabled} onRemove={onRemove} onMove={onMove} removeLabel={`Remove ${file.name}`}/>
  </li>;
  const remove = <button type="button" className="attachment-remove" aria-label={`Remove ${file.name}`} title="Remove" disabled={disabled} onClick={onRemove}><Icon name="close" size={12}/></button>;
  return <li {...item} className="attachment-chip image" data-kind="image"><ImageThumbnail name={file.name} size={file.size} loading={loading} variant="composer" onRemove={disabled ? undefined : onRemove} onMove={disabled ? undefined : onMove}/>{remove}</li>;
}

/** A text box's text: known for a paste, read from the file (up to a bound) for a dropped one. */
function useFileText(file: File): string | undefined {
  const [text, setText] = useState<string | undefined>(() => pastedTextOf(file));
  useEffect(() => {
    const known = pastedTextOf(file);
    if (known !== undefined || file.size > SENT_TEXT_PREVIEW_BYTES) { setText(known); return; }
    let alive = true;
    setText(undefined);
    void file.text().then(value => { if (alive) setText(value); }, () => {});
    return () => { alive = false; };
  }, [file]);
  return text;
}

/** The size of a text preview card: wide enough for code, as tall as its lines up to a bound that sits above the composer. */
const textPreviewSize = (lines = 0) => ({ width: 560, height: Math.min(340, 54 + Math.max(1, lines) * 17) });

function ComposerText({ file, item, disabled, onRemove, onMove, onReplace, onInline }: { file: File } & ComposerBoxActions) {
  const text = useFileText(file);
  const [viewing, setViewing] = useState(false);
  const pasted = isPastedName(file.name), language = languageOfName(file.name);
  const summary = text !== undefined ? textSummary(text) : undefined;
  const subtitle = boxSubtitle({ name: file.name, size: file.size, kind: 'text', lines: summary?.lines });
  const title = pasted ? summary?.title || 'Pasted text' : file.name;
  const inline = onInline && text !== undefined ? () => onInline(text) : undefined;
  const menu = <ActionMenu className="attachment-menu" label={`More for ${pasted ? 'the pasted text' : file.name}`}>
    {inline && <button type="button" disabled={disabled} onClick={inline}>Insert inline</button>}
    {text !== undefined && <button type="button" onClick={() => setViewing(true)}>{onReplace && !disabled ? 'Edit…' : 'Open…'}</button>}
    <button type="button" className="danger" disabled={disabled} onClick={onRemove}>Remove</button>
  </ActionMenu>;
  return <li {...item} className="attachment-chip box" data-kind="text">
    <CardBox icon={boxIcon('text', file.name)} title={title} subtitle={subtitle} label={`Open ${pasted ? subtitle : `${file.name}, ${subtitle}`}`} variant="composer" state={text === undefined && file.size <= SENT_TEXT_PREVIEW_BYTES ? 'loading' : 'ready'}
      preview={text !== undefined ? <TextPreview name={file.name} text={text} language={language} details={subtitle}/> : <DetailsPreview name={file.name} lines={[subtitle, 'Too large to preview here; it is sent whole.']}/>}
      previewSize={text !== undefined ? textPreviewSize(summary?.lines) : undefined} interactive={text !== undefined}
      onOpen={text !== undefined ? () => setViewing(true) : undefined} onRemove={onRemove} onMove={onMove} menu={menu} removeLabel={`Remove ${file.name}`} disabled={disabled}/>
    {viewing && text !== undefined && <TextViewer name={file.name} text={text} language={language} details={subtitle} editable={!!onReplace && !disabled}
      onSave={value => { setViewing(false); onReplace?.(replaceText(file, value)); }}
      onInline={onInline && !disabled ? value => { setViewing(false); onInline(value); } : undefined}
      onClose={() => setViewing(false)}/>}
  </li>;
}

function ComposerFile({ file, kind, item, disabled, onRemove, onMove }: { file: File; kind: 'pdf' | 'file' } & ComposerBoxActions) {
  const [pages, setPages] = useState<number>();
  useEffect(() => {
    if (kind !== 'pdf') return;
    let alive = true;
    void file.arrayBuffer().then(buffer => { if (alive) setPages(pdfPageCount(new Uint8Array(buffer))); }, () => {});
    return () => { alive = false; };
  }, [file, kind]);
  const subtitle = boxSubtitle({ name: file.name, size: file.size, kind, pages });
  return <li {...item} className="attachment-chip box" data-kind={kind}>
    <CardBox icon={boxIcon(kind, file.name)} title={file.name} subtitle={subtitle} label={`${file.name}, ${subtitle}`} variant="composer"
      preview={<DetailsPreview name={file.name} lines={[kind === 'pdf' ? subtitle : `${file.type || 'File'} · ${formatBytes(file.size)}`, file.lastModified ? `Modified ${new Date(file.lastModified).toLocaleString()}` : '']}/>}
      onRemove={onRemove} onMove={onMove} removeLabel={`Remove ${file.name}`} disabled={disabled}/>
  </li>;
}

export type SentFile = { name: string; size: number; mediaType?: string; path?: string; delivery?: string };

/** Sent images of every conversation in this window, by Runner path. */
const sentImages = new ImageCache();

/** Whether a holder has scrolled near the view: a sent attachment is read from the Runner only then. */
function useNearView<T extends Element>() {
  const holder = useRef<T>(null);
  const [visible, setVisible] = useState(false);
  useEffect(() => {
    const node = holder.current; if (!node || visible) return;
    const observer = new IntersectionObserver(entries => { if (entries.some(entry => entry.isIntersecting)) setVisible(true); }, { rootMargin: '200px 0px' });
    observer.observe(node);
    return () => observer.disconnect();
  }, [visible]);
  return { holder, visible };
}

/** One image a sent message carried, fetched from the Runner once it scrolls into view. */
function SentImage({ file }: { file: SentFile & { path: string } }) {
  const { api } = useRunner();
  const { holder, visible } = useNearView<HTMLLIElement>();
  const [loading, setLoading] = useState<Loading>({ state: 'loading' });
  useEffect(() => {
    if (!visible) return;
    let alive = true;
    sentImages.acquire(file.path, async () => {
      const content = await api.readAttachment({ path: file.path });
      return new Blob([content.data as Uint8Array<ArrayBuffer>], { type: content.mediaType });
    }).then(image => { if (alive) setLoading(image.width ? { state: 'ready', image } : { state: 'failed', reason: 'the image could not be decoded' }); }, error => { if (alive) setLoading({ state: 'failed', reason: error instanceof Error ? error.message : String(error) }); });
    return () => { alive = false; sentImages.release(file.path); };
  }, [visible, file.path, api]);
  const note = deliveryNote(file.delivery);
  if (loading.state === 'failed') return <li ref={holder} className="attachment-chip box" data-kind="file" title={`Preview unavailable: ${loading.reason}`}>
    <CardBox icon="image" title={file.name} subtitle={formatBytes(file.size)} label={`${file.name}, ${formatBytes(file.size)} · preview unavailable`} variant="sent" note={note}
      preview={<DetailsPreview name={file.name} lines={[`${file.mediaType || 'Image'} · ${formatBytes(file.size)}`, `Preview unavailable: ${loading.reason}`, note]}/>}/>
  </li>;
  return <li ref={holder} className="attachment-chip image" data-kind="image"><ImageThumbnail name={file.name} size={file.size} loading={loading} variant="sent" note={note}/></li>;
}

/** Sent text by Runner path, shared by every row of a window; a few of the latest are kept. */
const sentTexts = new Map<string, Promise<string>>();
function readSentText(path: string, read: () => Promise<string>): Promise<string> {
  let entry = sentTexts.get(path);
  if (!entry) {
    entry = read();
    entry.catch(() => sentTexts.delete(path));
    sentTexts.set(path, entry);
    while (sentTexts.size > 32) sentTexts.delete(sentTexts.keys().next().value!);
  }
  return entry;
}

/** One text file a sent message carried (a pasted box, a source file): read once it scrolls into view, to preview it. */
function SentText({ file }: { file: SentFile & { path: string } }) {
  const { api } = useRunner();
  const { holder, visible } = useNearView<HTMLLIElement>();
  const [text, setText] = useState<string>();
  const [failed, setFailed] = useState('');
  const [viewing, setViewing] = useState(false);
  useEffect(() => {
    if (!visible) return;
    let alive = true;
    readSentText(file.path, async () => new TextDecoder().decode((await api.readAttachment({ path: file.path })).data))
      .then(value => { if (alive) setText(value); }, error => { if (alive) setFailed(error instanceof Error ? error.message : String(error)); });
    return () => { alive = false; };
  }, [visible, file.path, api]);
  const pasted = isPastedName(file.name), language = languageOfName(file.name), note = deliveryNote(file.delivery);
  const summary = text !== undefined ? textSummary(text) : undefined;
  const subtitle = boxSubtitle({ name: file.name, size: file.size, kind: 'text', lines: summary?.lines });
  return <li ref={holder} className="attachment-chip box" data-kind="text" title={failed ? `Preview unavailable: ${failed}` : undefined}>
    <CardBox icon={boxIcon('text', file.name)} title={pasted ? summary?.title || 'Pasted text' : file.name} subtitle={subtitle} label={`Open ${pasted ? subtitle : `${file.name}, ${subtitle}`}`} variant="sent" note={note}
      state={text === undefined && !failed ? 'loading' : 'ready'}
      preview={text !== undefined ? <TextPreview name={file.name} text={text} language={language} details={[subtitle, note].filter(Boolean).join(' · ')}/> : <DetailsPreview name={file.name} lines={[subtitle, failed ? `Preview unavailable: ${failed}` : 'Loading…', note]}/>}
      previewSize={text !== undefined ? textPreviewSize(summary?.lines) : undefined} interactive={text !== undefined}
      onOpen={text !== undefined ? () => setViewing(true) : undefined}/>
    {viewing && text !== undefined && <TextViewer name={file.name} text={text} language={language} details={subtitle} onClose={() => setViewing(false)}/>}
  </li>;
}

/** A sent PDF or other file: named, with its size and how the agent received it. */
function SentFileBox({ file }: { file: SentFile }) {
  const kind = boxKind(file), note = deliveryNote(file.delivery);
  const subtitle = boxSubtitle({ name: file.name, size: file.size, kind });
  return <li className="attachment-chip box" data-kind={kind}>
    <CardBox icon={boxIcon(kind, file.name)} title={file.name} subtitle={subtitle} label={`${file.name}, ${subtitle}`} variant="sent" note={note}
      preview={<DetailsPreview name={file.name} lines={[kind === 'pdf' ? subtitle : `${file.mediaType || 'File'} · ${formatBytes(file.size)}`, note]}/>}/>
  </li>;
}

/** The files a sent message carried, each as its box, in the order they were sent. */
export function SentAttachments({ files }: { files: SentFile[] }) {
  return <ul className="sent-files" aria-label="Sent attachments">{files.map((file, i) => file.path && PREVIEWABLE.test(file.mediaType || '')
    ? <SentImage key={i} file={file as SentFile & { path: string }}/>
    : file.path && boxKind(file) === 'text' && file.size <= SENT_TEXT_PREVIEW_BYTES
      ? <SentText key={i} file={file as SentFile & { path: string }}/>
      : <SentFileBox key={i} file={file}/>)}</ul>;
}
