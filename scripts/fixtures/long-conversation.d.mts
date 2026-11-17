// Types for long-conversation.mjs, for TypeScript callers (scenarios).
import type { JournalEvent } from './runner-ui.mjs';

/** Runner journal events (without cursors) for a conversation of `turns` turns. */
export function longConversation(options?: { turns?: number; deltaSize?: number; toolOutputBytes?: number; model?: string }): JournalEvent[];
/** A busy Claude conversation's journal after a Runner restart, with a message delivered by Send now. */
export function deliveredQueuedTurn(): { events: JournalEvent[]; operations: { delivered: string; queued: string } };
