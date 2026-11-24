import { useContext, useEffect, useRef, type ReactNode } from 'react';
import { CurrentChatContext } from './activity-detail';

/** What every request card (RequestCards.tsx, RepositoryRequestCard.tsx) shares: keys, focus, header. */

export type RequestPosition = { index: number; count: number };

export const editable = (target: EventTarget | null) => target instanceof HTMLElement && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName) || !!target.closest('.monaco-editor, .xterm'));

/**
 * Keys for the first waiting request of a chat, while focus is in that chat's
 * messages (not in a field) or on the page itself. The card handles its own.
 * A key on the page itself is the chat in front's (the one last clicked or
 * focused): with two chats side by side, `y` answers one approval, never both.
 */
export function useChatKeys(card: React.RefObject<HTMLElement | null>, onKey: (event: KeyboardEvent) => boolean) {
  const handler = useRef(onKey); handler.current = onKey;
  const current = useContext(CurrentChatContext);
  const front = useRef(current); front.current = current;
  useEffect(() => {
    const listen = (event: KeyboardEvent) => {
      const node = card.current;
      if (!node || event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey || editable(event.target)) return;
      if (node.contains(event.target as Node) || document.querySelector('dialog[open]')) return;
      const chat = node.closest('.messages');
      const inChat = event.target === document.body ? front.current() : !!chat && chat.contains(event.target as Node);
      if (!inChat || chat?.querySelector('.request') !== node) return;
      if (handler.current(event)) event.preventDefault();
    };
    document.addEventListener('keydown', listen);
    return () => document.removeEventListener('keydown', listen);
  }, [card]);
}

/**
 * Brings a card that just appeared to the keyboard, unless the person is typing
 * elsewhere. Focus goes to the card itself (it has tabIndex -1), never to one
 * of its choices: a click in the transcript leaves focus on the page, and a
 * Space or Enter that follows (to page down, say) would otherwise press "Allow
 * once". From the card its keys (1–9, y, n, the arrows) still work, focusing it
 * announces it to a screen reader, and Space still scrolls the transcript.
 * `field` names a text field to focus instead (a question answered only in
 * words): typing there cannot answer anything by accident, and what the person
 * types is not dropped.
 */
export function useInitialFocus(card: React.RefObject<HTMLElement | null>, field?: string) {
  useEffect(() => {
    const node = card.current, active = document.activeElement;
    if (!node || (active && active !== document.body && (editable(active) || !node.closest('.messages')?.contains(active)))) return;
    if (node.closest('.messages')?.querySelector('.request') !== node) return;
    ((field && node.querySelector<HTMLElement>(field)) || node).focus({ preventScroll: true });
  }, [card, field]);
}

/** ↓ or ↑ on the card itself: its first or last choice, as Tab would reach them. */
export function arrowIntoCard(event: React.KeyboardEvent<HTMLElement>, card: HTMLElement | null, choices: string): boolean {
  if (!card || event.target !== card || (event.key !== 'ArrowDown' && event.key !== 'ArrowUp')) return false;
  const all = [...card.querySelectorAll<HTMLElement>(choices)].filter(node => !node.matches(':disabled,[aria-disabled=true]'));
  const next = event.key === 'ArrowDown' ? all[0] : all.at(-1);
  if (!next) return false;
  event.preventDefault(); next.focus(); return true;
}

export function CardHead({ eyebrow, tool, position }: { eyebrow: string; tool?: string; position?: RequestPosition }) {
  return <header className="request-head">
    <span className="eyebrow">{eyebrow}</span>
    {tool && <span className="request-tool">{tool}</span>}
    {position && position.count > 1 && <span className="request-count" title={`${position.count} requests are waiting`}>{position.index + 1} of {position.count}</span>}
  </header>;
}

export function Kbd({ children }: { children: ReactNode }) { return <kbd className="request-key" aria-hidden="true">{children}</kbd>; }
