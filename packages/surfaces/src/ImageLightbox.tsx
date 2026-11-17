import { useEffect, useRef } from 'react';
import { createPortal } from 'react-dom';
import { formatBytes, type LoadedImage } from './attachment-images';
import { Icon } from './Icons';
import { useBackdropDismiss } from './dismiss';
import './attachments.css';

/**
 * An attached image at full size, over everything. A modal <dialog>: Esc,
 * the close button or a click outside the image closes it, focus stays inside
 * while it is open and returns to the thumbnail after. While it is open the
 * window's keyboard shortcuts stand down (keyboard.ts skips an open dialog).
 */
export function ImageLightbox({ image, name, onClose }: { image: LoadedImage; name: string; onClose(): void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const close = useRef<HTMLButtonElement>(null);
  // The dialog fills the window: around the picture, it and its figure are the backdrop.
  useBackdropDismiss(dialog, event => event.target === dialog.current || (event.target instanceof Element && event.target.matches('.image-lightbox > figure')));
  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const node = dialog.current;
    if (node && !node.open) node.showModal();
    close.current?.focus();
    return () => { if (node?.open) node.close(); if (opener?.isConnected) opener.focus({ preventScroll: true }); };
  }, []);
  const details = [image.width && image.height ? `${image.width} × ${image.height}` : '', formatBytes(image.size)].filter(Boolean).join(' · ');
  // On the document, not inside the composer or the message: no panel's styles reach it.
  return createPortal(<dialog ref={dialog} className="image-lightbox" aria-label={name}
    onCancel={event => { event.preventDefault(); onClose(); }}
    onKeyDown={event => { if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); onClose(); } }}>
    <figure>
      <img src={image.url} alt={name} decoding="async" width={image.width || undefined} height={image.height || undefined}/>
      <figcaption><span className="image-lightbox-name" title={name}>{name}</span>{details && <span className="image-lightbox-details">{details}</span>}<small>Esc to close</small></figcaption>
    </figure>
    <button ref={close} type="button" className="image-lightbox-close" aria-label="Close image" onClick={onClose}><Icon name="close" size={18}/></button>
  </dialog>, document.body);
}
