import { useEffect, useRef } from 'react';
import { CollectionPanel } from './CollectionPanel';
import type { ComponentProps } from 'react';

/** Mobile navigation overlays the workspace; it never replaces the chat tree. */
export function WorkspaceNavigator({ onClose, ...props }: ComponentProps<typeof CollectionPanel> & { onClose(): void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const close = useRef(onClose); close.current = onClose;
  useEffect(() => {
    dialog.current?.showModal();
    dialog.current?.querySelector<HTMLButtonElement>('[data-close-navigation]')?.focus();
    const desktop = window.matchMedia('(min-width: 761px)');
    const closeOnDesktop = () => { if (desktop.matches && document.querySelectorAll('dialog[open]').length === 1) close.current(); };
    desktop.addEventListener('change', closeOnDesktop);
    return () => desktop.removeEventListener('change', closeOnDesktop);
  }, []);
  return <dialog ref={dialog} className="workspace-navigator" aria-label="Workspace navigator" onCancel={onClose}
    onClick={event => { if (event.target === event.currentTarget) { const r = event.currentTarget.getBoundingClientRect(); if (event.clientX < r.left || event.clientX > r.right || event.clientY < r.top || event.clientY > r.bottom) onClose(); } }}>
    <header><span>Navigate</span><button data-close-navigation aria-label="Close workspace navigator" onClick={onClose}>×</button></header>
    <CollectionPanel {...props} onTarget={target => { props.onTarget(target); onClose(); }} />
  </dialog>;
}
