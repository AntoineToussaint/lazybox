import { useLayoutEffect, useRef, type KeyboardEvent, type RefObject } from 'react';

/**
 * A card that asks once before something happens (a restart to switch the
 * model or the permissions), worked from the keyboard alone: it takes the
 * keyboard when it appears, on its primary button, so Enter confirms; Esc
 * cancels; and when it goes (or once its answer leaves nothing to press),
 * the keyboard goes back to the message box. A restart's new agent is the
 * chat's business: `focusComposerOf` asks its message box to take the
 * keyboard once it can.
 */
export function useConfirmCard(onCancel: () => void, returnTo?: RefObject<HTMLElement | null>, onConfirm?: () => void) {
  const card = useRef<HTMLDivElement>(null);
  const cancel = useRef(onCancel); cancel.current = onCancel;
  const confirm = useRef(onConfirm); confirm.current = onConfirm;
  /** Back to the message box, unless the person has moved on to something else. */
  const giveBack = (unmounted = false) => requestAnimationFrame(() => {
    // Mounted again (React's development double mount): the card keeps the keyboard.
    if (unmounted && card.current?.isConnected) return;
    const active = document.activeElement;
    if (active && active !== document.body && !card.current?.contains(active)) return;
    const box = returnTo?.current as HTMLTextAreaElement | null | undefined;
    if (box?.isConnected && !box.disabled) box.focus({ preventScroll: true });
  });
  const latest = useRef(giveBack); latest.current = giveBack;
  useLayoutEffect(() => {
    const take = () => { const node = card.current; (node?.querySelector<HTMLElement>('button.primary:not(:disabled)') ?? node)?.focus({ preventScroll: true }); };
    take();
    // The command that asked put the message box's caret back a frame later, which takes focus in
    // WebKit: take it again then, unless the person has gone somewhere else meanwhile.
    const frame = requestAnimationFrame(() => {
      const active = document.activeElement;
      if (!active || active === document.body || active === returnTo?.current) take();
    });
    return () => { cancelAnimationFrame(frame); latest.current(true); };
  }, []);
  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    // Enter on the card itself (its button was not ready when it appeared: the turn was still running).
    if (event.key === 'Enter' && event.target === event.currentTarget && confirm.current) { event.preventDefault(); confirm.current(); return; }
    if (event.key !== 'Escape') return;
    event.preventDefault(); event.stopPropagation();
    cancel.current();
  };
  return { cardProps: { ref: card, tabIndex: -1, onKeyDown }, giveBack: () => giveBack() };
}

/**
 * A conversation that continues in a new agent (a restart to switch model or
 * permissions) is shown by a chat that may mount again for it: its message
 * box takes the keyboard once it can take a message (AgentWindow.tsx).
 */
const wanted = new Map<string, number>();
export function focusComposerOf(sessionId: string) { wanted.set(sessionId, Date.now()); }
/**
 * This conversation's message box was asked to take the keyboard, in the
 * last few seconds: a chat that mounts for it then (and once more, if its
 * panel mounts it again) takes it.
 */
export function wantsComposerFocus(sessionId: string): boolean {
  const at = wanted.get(sessionId);
  if (at !== undefined && Date.now() - at > 5000) wanted.delete(sessionId);
  return at !== undefined && Date.now() - at <= 5000;
}
