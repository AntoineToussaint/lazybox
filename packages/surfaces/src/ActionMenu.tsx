import { forwardRef, useEffect, useImperativeHandle, useRef, type ReactNode } from 'react';
import { Icon } from './Icons';

/** The menu's buttons a person can reach: enabled, and not hidden (a menu may hold items only narrow layouts show). */
const shownButtons = (panel: HTMLElement) => [...panel.querySelectorAll<HTMLButtonElement>('button:not(:disabled)')].filter(button => button.getClientRects().length > 0);

/** Keep actions in the browser top layer, outside scrolling panel clipping. */
export const ActionMenu = forwardRef<HTMLDetailsElement, { label: string; className?: string; /** Replaces the ⋯ icon of the menu button. */ summary?: ReactNode; icon?: string; title?: string; /** Which edge of the button the menu lines up with: its end (a ⋯ at the right), or its start (a picker at the left). */ align?: 'start' | 'end'; onToggle?(open: boolean): void; children: ReactNode }>(function ActionMenu({ label, className = '', summary, icon = 'more', title, align = 'end', onToggle, children }, forwardedRef) {
  const details = useRef<HTMLDetailsElement>(null);
  const popup = useRef<HTMLDivElement>(null);
  const onToggleRef = useRef(onToggle); onToggleRef.current = onToggle;
  const alignRef = useRef(align); alignRef.current = align;
  useImperativeHandle(forwardedRef, () => details.current!, []);
  useEffect(() => {
    const owner = details.current!, panel = popup.current!;
    const position = () => {
      if (!owner.open || !panel.matches(':popover-open')) return;
      const anchor = owner.querySelector('summary')!.getBoundingClientRect();
      const bounds = panel.getBoundingClientRect();
      // In a panel popped out into its own window, the menu measures that window.
      const root = owner.ownerDocument.documentElement, width = root.clientWidth, height = root.clientHeight;
      const top = anchor.bottom + 4 + bounds.height <= height - 8 ? anchor.bottom + 4 : anchor.top - bounds.height - 4;
      panel.style.left = `${Math.max(8, Math.min(alignRef.current === 'start' ? anchor.left : anchor.right - bounds.width, width - bounds.width - 8))}px`;
      panel.style.top = `${Math.max(8, Math.min(top, height - bounds.height - 8))}px`;
    };
    const sync = () => {
      onToggleRef.current?.(owner.open);
      if (!owner.open) { panel.hidePopover(); return; }
      panel.showPopover();
      position();
      shownButtons(panel)[0]?.focus({ preventScroll: true });
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
    // A button marked data-keep-open (an inline form, say) leaves the menu open.
    if (event.target instanceof Element && event.target.closest('button') && !event.target.closest('[data-keep-open]') && popup.current?.contains(event.target)) {
      popup.current.hidePopover(); details.current!.open = false;
    }
  }} onKeyDown={event => {
    if (event.key === 'Escape') {
      if (!details.current?.open) return;
      event.preventDefault(); event.stopPropagation(); details.current!.open = false;
      popup.current?.hidePopover(); details.current?.querySelector('summary')?.focus();
    }
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key) || (event.target as HTMLElement).matches('input, textarea')) return;
    event.preventDefault();
    if (!details.current!.open) { details.current!.open = true; return; }
    const buttons = shownButtons(popup.current!);
    const index = buttons.indexOf(document.activeElement as HTMLButtonElement);
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : (index + (event.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length;
    buttons[next]?.focus();
  }}>
    <summary aria-label={label} title={title || label}>{summary ?? <Icon name={icon} />}</summary>
    <div ref={popup} popover="auto" className="action-popover" role="group" aria-label={`${label} menu`} onToggle={event => {
      if (event.target === event.currentTarget && event.newState === 'closed' && !event.currentTarget.matches(':popover-open')) details.current!.open = false;
    }}>{children}</div>
  </details>;
});
