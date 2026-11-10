import { forwardRef, useEffect, useImperativeHandle, useRef, type ReactNode } from 'react';
import { Icon } from './Icons';

/** Keep actions in the browser top layer, outside scrolling panel clipping. */
export const ActionMenu = forwardRef<HTMLDetailsElement, { label: string; className?: string; children: ReactNode }>(function ActionMenu({ label, className = '', children }, forwardedRef) {
  const details = useRef<HTMLDetailsElement>(null);
  const popup = useRef<HTMLDivElement>(null);
  useImperativeHandle(forwardedRef, () => details.current!, []);
  useEffect(() => {
    const owner = details.current!, panel = popup.current!;
    const position = () => {
      if (!owner.open || !panel.matches(':popover-open')) return;
      const anchor = owner.querySelector('summary')!.getBoundingClientRect();
      const bounds = panel.getBoundingClientRect();
      const width = document.documentElement.clientWidth, height = document.documentElement.clientHeight;
      const top = anchor.bottom + 4 + bounds.height <= height - 8 ? anchor.bottom + 4 : anchor.top - bounds.height - 4;
      panel.style.left = `${Math.max(8, Math.min(anchor.right - bounds.width, width - bounds.width - 8))}px`;
      panel.style.top = `${Math.max(8, Math.min(top, height - bounds.height - 8))}px`;
    };
    const sync = () => {
      if (!owner.open) { panel.hidePopover(); return; }
      panel.showPopover();
      position();
      panel.querySelector<HTMLButtonElement>('button:not(:disabled)')?.focus({ preventScroll: true });
    };
    const observer = new MutationObserver(sync);
    observer.observe(owner, { attributes: true, attributeFilter: ['open'] });
    const scroll = (event: Event) => { if (!(event.target instanceof Node) || !panel.contains(event.target)) position(); };
    window.addEventListener('resize', position);
    document.addEventListener('scroll', scroll, true);
    return () => { observer.disconnect(); window.removeEventListener('resize', position); document.removeEventListener('scroll', scroll, true); };
  }, []);
  return <details ref={details} className={`workspace-actions ${className}`} onClickCapture={event => {
    // Dismiss before an action opens a modal: deferred native focus restoration
    // must not steal the new dialog's initial focus.
    if (event.target instanceof Element && event.target.closest('button') && popup.current?.contains(event.target)) {
      popup.current.hidePopover(); details.current!.open = false;
    }
  }} onKeyDown={event => {
    if (event.key === 'Escape') {
      if (!details.current?.open) return;
      event.preventDefault(); event.stopPropagation(); details.current!.open = false;
      popup.current?.hidePopover(); details.current?.querySelector('summary')?.focus();
    }
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    if (!details.current!.open) { details.current!.open = true; return; }
    const buttons = [...popup.current!.querySelectorAll<HTMLButtonElement>('button:not(:disabled)')];
    const index = buttons.indexOf(document.activeElement as HTMLButtonElement);
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : (index + (event.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length;
    buttons[next]?.focus();
  }}>
    <summary aria-label={label} title={label}><Icon name="more" /></summary>
    <div ref={popup} popover="auto" className="action-popover" role="group" aria-label={`${label} menu`} onToggle={event => {
      if (event.target === event.currentTarget && event.newState === 'closed' && !event.currentTarget.matches(':popover-open')) details.current!.open = false;
    }}>{children}</div>
  </details>;
});
