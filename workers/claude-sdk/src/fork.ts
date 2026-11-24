import type { SessionMessage } from '@anthropic-ai/claude-agent-sdk';

/**
 * The transcript entry a fork keeps through, for a turn recorded without its
 * fork point (NativeStatus `forkPoint`): the turn's last message, by the id
 * the API gave it, which the turn's usage events carry. One API message can
 * be several entries (thinking, text, tool calls): the last of them. As for a
 * recorded fork point, only a turn that ended on its own is a boundary.
 * Undefined when the transcript no longer holds the message (compacted away).
 */
export function endOfTurn(messages: SessionMessage[], messageId: string): string | undefined {
  const entry = messages.findLast(m => m.type === 'assistant' && !m.parent_tool_use_id && (m.message as { id?: unknown } | null)?.id === messageId);
  return (entry?.message as { stop_reason?: unknown } | undefined)?.stop_reason === 'end_turn' ? entry!.uuid : undefined;
}
