import { useCallback, useId, useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { placePreview, type PreviewPlacement } from './attachment-images';
import { PREVIEW_CHARS, languageLabel } from './attachment-boxes';
import { Icon } from './Icons';
import { useBackdropDismiss } from './dismiss';
import './attachments.css';

/** How long a pointer rests on a box before its preview opens; passing over one does not flash it. */
export const HOVER_DELAY_MS = 160;
/** How long a preview a pointer can enter stays open after the pointer leaves the box. */
const LINGER_MS = 220;

/**
 * The hover and focus behaviour every box shares: resting the pointer on it
 * (or moving keyboard focus to it) shows its preview, Esc hides it. An
 * interactive preview (text that scrolls) stays open while the pointer is on
 * it, so it can be scrolled.
 */
export function useBoxPreview() {
  const [preview, setPreview] = useState(false);
  const timer = useRef(0);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  const show = useCallback((delay: number) => { window.clearTimeout(timer.current); timer.current = window.setTimeout(() => setPreview(true), delay); }, []);
  const hide = useCallback(() => { window.clearTimeout(timer.current); setPreview(false); }, []);
  const linger = useCallback(() => { window.clearTimeout(timer.current); timer.current = window.setTimeout(() => setPreview(false), LINGER_MS); }, []);
  const keep = useCallback(() => window.clearTimeout(timer.current), []);
  return {
    preview, hide, keep, linger,
    anchorProps: (interactive = false) => ({
      onPointerEnter: (event: { pointerType: string }) => { if (event.pointerType === 'mouse') show(HOVER_DELAY_MS); },
      onPointerLeave: interactive ? linger : hide,
      onFocus: (event: { currentTarget: Element }) => { if (event.currentTarget.matches(':focus-visible')) show(0); },
      onBlur: hide,
    }),
  };
}

/**
 * The keys of a box in the message being written: Delete or Backspace
 * removes it, Alt+← and Alt+→ move it, Esc hides its preview. Enter opens it
 * (the button's own click).
 */
export function boxKeys({ onRemove, onMove, preview, hide }: { onRemove?(): void; onMove?(by: -1 | 1): void; preview: boolean; hide(): void }) {
  return (event: KeyboardEvent<HTMLElement>) => {
    if (event.key === 'Escape' && preview) { event.preventDefault(); event.stopPropagation(); hide(); return; }
    if ((event.key === 'Delete' || event.key === 'Backspace') && onRemove && !event.altKey && !event.metaKey && !event.ctrlKey) {
      event.preventDefault(); event.stopPropagation(); hide();
      // Focus goes to the box that takes its place, else to the message box.
      const item = event.currentTarget.closest('li');
      const next = (item?.nextElementSibling ?? item?.previousElementSibling)?.querySelector<HTMLElement>('.attachment-thumb, .attachment-box, .attachment-remove');
      const message = event.currentTarget.closest('form')?.querySelector<HTMLTextAreaElement>('textarea');
      onRemove();
      requestAnimationFrame(() => { if (next?.isConnected) next.focus(); else message?.focus(); });
      return;
    }
    if (event.altKey && (event.key === 'ArrowLeft' || event.key === 'ArrowRight') && onMove) {
      event.preventDefault(); event.stopPropagation();
      const target = event.currentTarget;
      onMove(event.key === 'ArrowLeft' ? -1 : 1);
      requestAnimationFrame(() => target.isConnected && target.focus());
    }
  };
}

/**
 * A preview card beside its box (placePreview says where), in this share of
 * the window. A plain one never takes the pointer, so it cannot cover what it
 * previews and trap the hover; an interactive one (text to scroll) does, and
 * keeps itself open while the pointer is on it. It follows the window's size
 * and closes when what is under it scrolls.
 */
export function BoxPreview({ id, anchor, size, interactive, onKeep, onLeave, onClose, className = '', children }: { id?: string; anchor: HTMLElement; size: { width: number; height: number }; interactive?: boolean; onKeep?(): void; onLeave?(): void; onClose(): void; className?: string; children: ReactNode }) {
  const [placement, setPlacement] = useState<PreviewPlacement | null>(null);
  const card = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const place = () => setPlacement(placePreview(anchor.getBoundingClientRect(), size, { width: window.innerWidth, height: window.innerHeight }, { share: 0.6, chrome: { width: 0, height: 0 }, minCardWidth: 0 }));
    place();
    window.addEventListener('resize', place);
    return () => window.removeEventListener('resize', place);
  }, [anchor, size.width, size.height]);
  useEffect(() => {
    const scrolled = (event: Event) => { if (event.target instanceof Node && event.target.contains(anchor) && !card.current?.contains(event.target)) onClose(); };
    window.addEventListener('scroll', scrolled, true);
    return () => window.removeEventListener('scroll', scrolled, true);
  }, [anchor, onClose]);
  return createPortal(<div id={id} ref={card} className={`attachment-preview attachment-card-preview ${className}`} role="tooltip" data-interactive={interactive || undefined} data-placed={placement ? placement.side : undefined}
    style={placement ? { left: placement.left, top: placement.top, width: placement.width, maxHeight: placement.height } : undefined}
    onPointerEnter={interactive ? onKeep : undefined} onPointerLeave={interactive ? onLeave : undefined}>
    {children}
  </div>, document.body);
}

/** "Python · 12 lines · 1 KiB": the language once, then the details. */
const caption = (language: string, details: string) => [language && !details.includes(languageLabel(language)) ? languageLabel(language) : '', details].filter(Boolean).join(' · ');

/** Text in a preview: monospace, at most PREVIEW_CHARS, its language named. */
export function TextPreview({ name, text, language, details }: { name: string; text: string; language: string; details: string }) {
  const shown = text.length > PREVIEW_CHARS ? text.slice(0, PREVIEW_CHARS) : text;
  return <>
    <pre className="attachment-text-preview" data-language={language || undefined} tabIndex={-1}>{shown}{shown.length < text.length && <span className="attachment-text-more">{'\n'}… {Math.ceil((text.length - shown.length) / 1024)} KiB more</span>}</pre>
    <div className="attachment-preview-caption"><span>{name}</span><small>{caption(language, details)}</small></div>
  </>;
}

/** A file's facts as a preview: its name, then one line each. */
export function DetailsPreview({ name, lines }: { name: string; lines: string[] }) {
  return <div className="attachment-details-preview"><strong>{name}</strong>{lines.filter(Boolean).map((line, i) => <span key={i}>{line}</span>)}</div>;
}

/**
 * A box for anything that is not a picture: its icon, a title (a paste's
 * first line, else the name) and a line under it ("Pasted text · 142 lines",
 * "PDF · 12 pages · 1.2 MiB"). The same size in the composer and in a sent
 * message; a remove button and a ⋯ menu in the composer.
 */
export function CardBox({ icon, title, subtitle, label, variant, state, preview, previewSize, interactive, onOpen, onRemove, onMove, menu, removeLabel, disabled, note }: {
  icon: string; title: string; subtitle: string; label: string; variant: 'composer' | 'sent'; state?: 'loading' | 'ready';
  preview?: ReactNode; previewSize?: { width: number; height: number }; interactive?: boolean;
  onOpen?(): void; onRemove?(): void; onMove?(by: -1 | 1): void; menu?: ReactNode; removeLabel?: string; disabled?: boolean; note?: string;
}) {
  const button = useRef<HTMLButtonElement>(null);
  const hover = useBoxPreview();
  const previewId = useId();
  const [pinned, setPinned] = useState(false);
  // A box with nothing to open shows its details on a click or a tap: the preview, held open.
  const open = onOpen ?? (() => setPinned(value => !value));
  useEffect(() => { if (!pinned) return; const close = (event: PointerEvent) => { if (!button.current?.contains(event.target as Node) && !document.getElementById(previewId)?.contains(event.target as Node)) setPinned(false); }; document.addEventListener('pointerdown', close, true); return () => document.removeEventListener('pointerdown', close, true); }, [pinned, previewId]);
  const showing = (hover.preview || pinned) && !!preview && !!button.current;
  return <>
    <button ref={button} type="button" className={`attachment-box ${variant === 'composer' ? 'draft-box' : 'sent-box'}`} data-state={state} aria-label={label} title={note}
      aria-describedby={showing ? previewId : undefined} aria-haspopup={onOpen ? 'dialog' : undefined} aria-expanded={onOpen ? undefined : pinned}
      {...hover.anchorProps(interactive)}
      onKeyDown={boxKeys({ onRemove: disabled ? undefined : onRemove, onMove: disabled ? undefined : onMove, preview: hover.preview || pinned, hide: () => { hover.hide(); setPinned(false); } })}
      onClick={event => { if (onOpen) event.currentTarget.focus({ preventScroll: true }); hover.hide(); open(); }}>
      <span className="attachment-box-icon" aria-hidden="true"><Icon name={icon} size={20}/></span>
      <span className="attachment-box-text"><span className="attachment-box-title">{title}</span><small>{subtitle}</small></span>
    </button>
    {menu}
    {onRemove && <button type="button" className="attachment-remove" aria-label={removeLabel} title="Remove" disabled={disabled} onClick={onRemove}><Icon name="close" size={12}/></button>}
    {showing && <BoxPreview id={previewId} anchor={button.current!} size={previewSize ?? { width: 320, height: 140 }} interactive={interactive} onKeep={hover.keep} onLeave={hover.linger} onClose={() => { hover.hide(); setPinned(false); }}>{preview}</BoxPreview>}
  </>;
}

/**
 * A text attachment full size, over everything: a modal <dialog> like the
 * image lightbox. In the composer it can be edited (Save keeps the edit, ⌘↵
 * too) or put back in the message as text; a sent one is read only. Esc,
 * Cancel or a click outside closes it without changing anything, and focus
 * returns to the box.
 */
export function TextViewer({ name, text, language, details, editable, onSave, onInline, onClose }: { name: string; text: string; language: string; details: string; editable?: boolean; onSave?(text: string): void; onInline?(text: string): void; onClose(): void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const field = useRef<HTMLTextAreaElement>(null);
  const [draft, setDraft] = useState(text);
  const [copied, setCopied] = useState(false);
  useBackdropDismiss(dialog);
  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const node = dialog.current;
    if (node && !node.open) node.showModal();
    field.current?.focus(); field.current?.setSelectionRange(0, 0); if (field.current) field.current.scrollTop = 0;
    return () => { if (node?.open) node.close(); if (opener?.isConnected) opener.focus({ preventScroll: true }); };
  }, []);
  const copy = () => { void navigator.clipboard?.writeText(draft).then(() => { setCopied(true); window.setTimeout(() => setCopied(false), 1200); }, () => {}); };
  return createPortal(<dialog ref={dialog} className="text-viewer" aria-label={name}
    onCancel={event => { event.preventDefault(); onClose(); }}
    onKeyDown={event => {
      if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); onClose(); }
      if (editable && event.key === 'Enter' && (event.metaKey || event.ctrlKey)) { event.preventDefault(); event.stopPropagation(); onSave?.(draft); }
    }}>
    <header><span className="text-viewer-name" title={name}>{name}</span><small>{caption(language, details)}</small>
      <button type="button" className="icon-button" aria-label="Close" onClick={onClose}><Icon name="close"/></button></header>
    {editable
      ? <textarea ref={field} className="text-viewer-body" aria-label={`Text of ${name}`} spellCheck={false} value={draft} onChange={event => setDraft(event.target.value)} data-language={language || undefined}/>
      : <pre className="text-viewer-body" tabIndex={0} aria-label={`Text of ${name}`} data-language={language || undefined}>{text}</pre>}
    <footer>
      <button type="button" onClick={copy}>{copied ? 'Copied' : 'Copy'}</button>
      {onInline && <button type="button" onClick={() => onInline(draft)} title="Put this text in the message instead of attaching it">Insert inline</button>}
      <span className="text-viewer-spacer"/>
      {editable ? <>
        <button type="button" onClick={onClose}>Cancel</button>
        <button type="button" className="primary" disabled={draft === text} onClick={() => onSave?.(draft)}>Save</button>
      </> : <button type="button" className="primary" onClick={onClose}>Close</button>}
    </footer>
  </dialog>, document.body);
}

/** A file that was not attached, and why ("Too large: 25 MiB of 20"): dismissed with × or Delete. */
export function ErrorBox({ name, reason, onDismiss }: { name: string; reason: string; onDismiss(): void }) {
  const keys = boxKeys({ onRemove: onDismiss, preview: false, hide: () => {} });
  return <li className="attachment-chip box error" role="group" aria-label={`${name} was not attached: ${reason}`}>
    <span className="attachment-box error-box" aria-hidden="true">
      <span className="attachment-box-icon"><Icon name="alert" size={20}/></span>
      <span className="attachment-box-text"><span className="attachment-box-title">{name}</span><small>{reason}</small></span>
    </span>
    <button type="button" className="attachment-remove" aria-label={`Dismiss ${name}`} title="Dismiss" onClick={onDismiss} onKeyDown={keys}><Icon name="close" size={12}/></button>
  </li>;
}
