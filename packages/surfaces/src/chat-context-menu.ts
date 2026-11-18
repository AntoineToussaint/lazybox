/**
 * What a right-click offers in a conversation: a message, a code block, a
 * file an agent named. The menus are declared by `MessageList` and
 * `RichContent`; other features add to them through the `message`,
 * `code-block` and `file-link` scopes (see `ContextMenu.tsx`).
 */
import type { HandoffMessage } from './ConversationHandoff';
import { composeInConversation } from './compose';
import { copyText, showMenuNotice, type ContextMenuContext } from './ContextMenu';
import { fencedCode, quoteText, type MenuItem } from './context-menu-model';
import type { FileLink } from './file-reference';
import type { OpenModifiers } from './file-links';
import type { TranscriptMessage } from './transcript-model';
export { promptBefore } from './transcript-model';
import { isMac } from './workbench';

export type MessageRole = 'agent' | 'user' | 'notice';
export type MessageMenuDetail = { message: TranscriptMessage; role: MessageRole; sessionId: string };
export const messageRole = (message: TranscriptMessage): MessageRole => message.type === 'operation_accepted' && !message.origin ? 'user' : message.type === 'message_delta' ? 'agent' : 'notice';

/** Put text after the draft of a conversation's message box, as a quote to answer. */
export function quoteInReply(sessionId: string, text: string) {
  if (!composeInConversation({ sessionId, text: quoteText(text) })) showMenuNotice('Quoted: it appears in the message box when this conversation can take a message');
}

/** A message's own items: quoting, copying, finding its prompt; then its turn's checkpoint actions. */
export function messageItems(context: ContextMenuContext<MessageMenuDetail>, { prompt, onReveal, onHandoff, turn }: { prompt?: TranscriptMessage; onReveal?(id: string): void; onHandoff?(message: HandoffMessage): void; turn: MenuItem[] }): MenuItem[] {
  const { message, role, sessionId } = context.detail;
  const text = message.text;
  if (!text.trim()) return turn;
  // The text as read: rendered Markdown without its marks.
  const rendered = () => (context.element.querySelector('.rich-content') as HTMLElement | null)?.innerText.trim() || text;
  const quoted = context.selectionInside && context.selection ? context.selection : '';
  return [
    !!sessionId && (quoted
      ? { label: 'Quote selection in reply', icon: 'quote', run: () => quoteInReply(sessionId, quoted) }
      : { label: 'Quote in reply', icon: 'quote', run: () => quoteInReply(sessionId, rendered()) }),
    onHandoff && role !== 'notice' && { label: 'Send to conversation…', icon: 'send', run: () => onHandoff({ id: message.id, text: quoted || text }) },
    'separator',
    { label: 'Copy message', icon: 'copy', run: () => copyText(rendered(), 'Copied message') },
    { label: 'Copy as Markdown', icon: 'copy', run: () => copyText(text, 'Copied as Markdown') },
    role === 'agent' && prompt && onReveal && { label: 'Jump to prompt', icon: 'up', run: () => onReveal(prompt.id) },
    'separator',
    ...turn,
  ];
}


export const codeBlockItems = (text: string, language: string): MenuItem[] => [
  { label: 'Copy code', icon: 'copy', run: () => copyText(text, 'Copied code') },
  { label: 'Copy as Markdown', icon: 'copy', run: () => copyText(fencedCode(text, language), 'Copied as Markdown') },
];

/** A file an agent named: open it in every way a click can, show it, or copy where it is. */
export function fileLinkItems(file: FileLink, open: (file: FileLink, how: OpenModifiers) => void): MenuItem[] {
  return [
    { label: 'Open', icon: 'file', run: () => open(file, {}) },
    { label: 'Open to the side', icon: 'split-right', shortcut: 'Mod+Click', run: () => open(file, { metaKey: isMac, ctrlKey: !isMac }) },
    { label: 'Quick look', icon: 'eye', shortcut: 'Alt+Click', run: () => open(file, { altKey: true }) },
    { label: 'Reveal in Files', icon: 'target', run: () => open(file, { reveal: true }) },
    'separator',
    { label: 'Copy path', icon: 'copy', run: () => open(file, { copy: 'absolute' }) },
    { label: 'Copy relative path', icon: 'copy', run: () => open(file, { copy: 'relative' }) },
  ];
}
