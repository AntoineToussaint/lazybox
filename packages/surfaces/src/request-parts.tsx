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

/** Focus a card's control when it appears, unless the person is typing elsewhere. */
export function useInitialFocus(card: React.RefObject<HTMLElement | null>, selector: string) {
  useEffect(() => {
    const node = card.current, active = document.activeElement;
    if (!node || (active && active !== document.body && (editable(active) || !node.closest('.messages')?.contains(active)))) return;
    if (node.closest('.messages')?.querySelector('.request') !== node) return;
    node.querySelector<HTMLElement>(selector)?.focus({ preventScroll: true });
  }, [card, selector]);
}

export function CardHead({ eyebrow, tool, position }: { eyebrow: string; tool?: string; position?: RequestPosition }) {
  return <header className="request-head">
    <span className="eyebrow">{eyebrow}</span>
    {tool && <span className="request-tool">{tool}</span>}
    {position && position.count > 1 && <span className="request-count" title={`${position.count} requests are waiting`}>{position.index + 1} of {position.count}</span>}
  </header>;
}

export function Kbd({ children }: { children: ReactNode }) { return <kbd className="request-key" aria-hidden="true">{children}</kbd>; }
