import type { SDKUserMessage } from '@anthropic-ai/claude-agent-sdk';
import { randomUUID } from 'node:crypto';

/**
 * Steering a running Claude turn.
 *
 * In streaming-input mode every user message the worker yields is written to
 * Claude Code at once, and the CLI queues it with a priority. `next` (the
 * CLI's default) is folded into the running turn between two steps, after the
 * tool results the turn is waiting for, exactly as a message typed in Claude
 * Code's own terminal during a turn; `later` waits for the turn to end. A
 * message the Runner sends as a turn of its own goes only while no turn runs,
 * so it needs no priority; a steered one says `next` explicitly, so a change
 * of the CLI's default cannot turn a steer into a queued turn unnoticed.
 *
 * A steer that reaches the CLI after its turn's last step starts the next
 * turn instead: it is delivered either way, never dropped.
 */
export const steerPriority: NonNullable<SDKUserMessage['priority']> = 'next';

/** A user message for Claude Code: a turn of its own, or, with a priority, into the running one. */
export function userMessage(content: any[], sessionId: string, priority?: SDKUserMessage['priority']): SDKUserMessage {
  return { type: 'user', uuid: randomUUID(), session_id: sessionId, message: { role: 'user', content }, parent_tool_use_id: null, ...(priority ? { priority } : {}) };
}
