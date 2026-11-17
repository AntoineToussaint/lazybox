import { forwardRef, useLayoutEffect, useImperativeHandle, useRef, useState, type ComponentPropsWithoutRef } from 'react';
import { createPortal } from 'react-dom';
import { bindBackdropDismiss, isEditField, nudge, requestDialogClose } from './dismiss';
import './dialog.css';

type DismissProps = {
  /**
   * What happens to what the person typed when the dialog closes. `guard`
   * (the default): it would be lost, so once a field was changed a click
   * outside keeps the dialog open and nudges its footer; Escape, Cancel and ×
   * still close it. `kept`: the dialog keeps its draft (New task does), so a
   * click outside closes it like Escape.
   */
  draft?: 'guard' | 'kept';
  /** Replaces the `cancel` a click outside sends, for a dialog whose Escape does something else first (clears a search, say). */
  onDismiss?(): void;
};

// Preserve native dialog focus, modality and Escape behavior. Position belongs
// to this presentation only, never to a session or saved workspace layout.
// A click on the backdrop closes the dialog as Escape does (`dismiss.ts`).
export const DraggableDialog = forwardRef<HTMLDialogElement, ComponentPropsWithoutRef<'dialog'> & DismissProps>(function DraggableDialog({ className = '', children, draft = 'guard', onDismiss, ...props }, forwardedRef) {
  const dialog = useRef<HTMLDialogElement>(null);
  useImperativeHandle(forwardedRef, () => dialog.current!, []);
  const dismissal = useRef({ draft, onDismiss }); dismissal.current = { draft, onDismiss };
  /** A field of this dialog changed since it opened: its work would be lost. */
  const edited = useRef(false);
  const drag = useRef<{ id: number; x: number; y: number; left: number; top: number } | null>(null);
  const moved = useRef(false);
  const [dragging, setDragging] = useState(false);

  const stop = () => {
    const id = drag.current?.id;
    drag.current = null;
    if (id !== undefined && dialog.current?.hasPointerCapture(id)) dialog.current.releasePointerCapture(id);
    setDragging(false);
  };
  const center = () => {
    stop(); moved.current = false;
    for (const property of ['left', 'top', 'right', 'bottom', 'margin']) dialog.current?.style.removeProperty(property);
  };
  const move = (left: number, top: number) => {
    const node = dialog.current;
    if (!node?.open) return;
    const bounds = node.getBoundingClientRect();
    const width = document.documentElement.clientWidth, height = document.documentElement.clientHeight;
    const x = Math.max(8, Math.min(left, width - bounds.width - 8));
    const y = Math.max(8, Math.min(top, height - bounds.height - 8));
    Object.assign(node.style, { margin: '0', left: `${x}px`, top: `${y}px`, right: 'auto', bottom: 'auto' });
    moved.current = true;
  };
  useLayoutEffect(() => {
    const node = dialog.current!;
    // Where the focus was before the dialog took it: it goes back there when the dialog closes or goes.
    let opener = document.activeElement instanceof HTMLElement && !node.contains(document.activeElement) ? document.activeElement : null;
    const giveBack = () => {
      const target = opener;
      if (!target) return;
      // After the dialog is gone; only when nothing else took the focus meanwhile (the close action may have placed it).
      queueMicrotask(() => {
        const now = document.activeElement;
        if ((now && now !== document.body && now.isConnected && !node.contains(now)) || !target.isConnected || target.closest('[inert]')) return;
        target.focus({ preventScroll: true });
      });
    };
    const remember = (event: FocusEvent) => { if (!node.open && event.target instanceof HTMLElement && !node.contains(event.target)) opener = event.target; };
    document.addEventListener('focusin', remember, true);
    const fit = () => {
      if (!moved.current || !node.open) return;
      const bounds = node.getBoundingClientRect(); move(bounds.left, bounds.top);
    };
    const open = new MutationObserver(()=>{
      center();
      edited.current = false;
      if(!node.open){ giveBack(); return; }
      // A field the dialog's owner focused as it opened stands (New workspace starts on its name);
      // otherwise the native focus, on the drag handle, moves to the designated or first field.
      const active=document.activeElement;
      if(active instanceof HTMLElement&&active!==node&&node.contains(active)&&!active.hasAttribute('data-dialog-drag-handle'))return;
      const designated=node.querySelector<HTMLElement>('[data-autofocus]');
      if(designated)designated.focus();
      else if(document.activeElement?.hasAttribute('data-dialog-drag-handle'))
        node.querySelector<HTMLElement>('[autofocus],input:not([disabled]),textarea:not([disabled]),select:not([disabled]),footer button:not([disabled]),button.primary:not([disabled])')?.focus();
    });
    open.observe(node, { attributes: true, attributeFilter: ['open'] });
    const size = new ResizeObserver(fit); size.observe(node);
    window.addEventListener('resize', fit);
    window.addEventListener('blur', stop);
    const change = (event: Event) => { if (event.isTrusted && isEditField(event.target)) edited.current = true; };
    node.addEventListener('input', change); node.addEventListener('change', change);
    const unbind = bindBackdropDismiss(node, () => {
      const { draft, onDismiss } = dismissal.current;
      if (draft === 'guard' && edited.current) { nudge(node); return; }
      if (onDismiss) onDismiss(); else requestDialogClose(node);
    });
    return () => {
      open.disconnect(); size.disconnect(); window.removeEventListener('resize', fit); window.removeEventListener('blur', stop); node.removeEventListener('input', change); node.removeEventListener('change', change); unbind();
      document.removeEventListener('focusin', remember, true);
      // Unmounted while open (most dialogs close by going away): the focus goes back too.
      if (node.open || node.contains(document.activeElement)) giveBack();
    };
  }, []);

  const isHandle = (target: EventTarget) => target instanceof Element && !!target.closest('[data-dialog-drag-handle]') && !target.closest('button, input, select, textarea, a, [contenteditable="true"]');
  // Dialog lifetime belongs to its caller, but presentation must survive a
  // responsive panel becoming hidden. Keep shared shell styling on the layer.
  return createPortal(<div className="workspace-shell dialog-layer"><dialog {...props} ref={dialog} className={`${className} draggable-dialog${dragging ? ' dialog-dragging' : ''}`}
    onCancel={event => {
      // React portal events follow the component tree. Cancelling a nested
      // dialog must not also cancel the dialog that opened it.
      event.stopPropagation();
      props.onCancel?.(event);
    }}
    onPointerDown={event => {
      props.onPointerDown?.(event);
      if (event.defaultPrevented || event.button !== 0 || !event.isPrimary || !isHandle(event.target)) return;
      const bounds = event.currentTarget.getBoundingClientRect();
      drag.current = { id: event.pointerId, x: event.clientX, y: event.clientY, left: bounds.left, top: bounds.top };
      event.currentTarget.setPointerCapture(event.pointerId); setDragging(true); event.preventDefault();
    }}
    onPointerMove={event => {
      props.onPointerMove?.(event);
      const start = drag.current;
      if (start?.id === event.pointerId) move(start.left + event.clientX - start.x, start.top + event.clientY - start.y);
    }}
    onPointerUp={event => { props.onPointerUp?.(event); if (drag.current?.id === event.pointerId) stop(); }}
    onPointerCancel={event => { props.onPointerCancel?.(event); stop(); }}
    onLostPointerCapture={event => { props.onLostPointerCapture?.(event); stop(); }}
    onDoubleClick={event => { props.onDoubleClick?.(event); if (!event.defaultPrevented && isHandle(event.target)) center(); }}
    onKeyDown={event => {
      props.onKeyDown?.(event);
      if (event.defaultPrevented || !(event.target instanceof HTMLElement) || !event.target.hasAttribute('data-dialog-drag-handle')) return;
      if (event.key === 'Home') { event.preventDefault(); center(); return; }
      const direction = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] }[event.key];
      if (!direction) return;
      event.preventDefault();
      const bounds = event.currentTarget.getBoundingClientRect(), step = event.shiftKey ? 40 : 10;
      move(bounds.left + direction[0] * step, bounds.top + direction[1] * step);
    }}>{children}</dialog></div>, document.body);
});

export function DialogHeader(props: ComponentPropsWithoutRef<'header'>) {
  return <header {...props} data-dialog-drag-handle tabIndex={0} aria-label="Move dialog" aria-description="Drag to move. Arrow keys move the dialog; Home or double-click centers it."/>;
}
