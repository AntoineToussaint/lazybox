import { createContext } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import type { RevealMessageRequest } from './artifact-events';

const prefix = '#lazybox-source?';
export const ConversationSourceContext = createContext<((source: RevealMessageRequest) => void) | undefined>(undefined);

export function conversationSourceLink({ sessionId, messageId }: RevealMessageRequest): string {
  return prefix + new URLSearchParams({ session: sessionId, message: messageId }).toString();
}

export function conversationSourceFromLink(href: string): RevealMessageRequest | undefined {
  if (!href.startsWith(prefix)) return undefined;
  const parts = new URLSearchParams(href.slice(prefix.length));
  const sessionId = parts.get('session'), messageId = parts.get('message');
  if (sessionId && messageId) return { sessionId, messageId };
}

/** A resumed conversation keeps its transcript cursors under a new execution ID. */
export async function currentConversationSource(api: RunnerClient, workspaceId: string, source: RevealMessageRequest): Promise<RevealMessageRequest | undefined> {
  const signal = AbortSignal.timeout(5000);
  const seen = new Set<string>();
  for (let sessionId = source.sessionId; sessionId && !seen.has(sessionId);) {
    seen.add(sessionId);
    const { session } = await api.getSession({ sessionId }, { signal });
    if (!session || session.collectionId !== workspaceId) return undefined;
    if (!session.successorExecutionId) return { ...source, sessionId: session.id };
    sessionId = session.successorExecutionId;
  }
}
