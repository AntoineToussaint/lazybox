import { providerData } from '@lazybox/client-core';
import type { AgentEvent } from '@lazybox/protocol';

/**
 * An agent's request that the person start a conversation (`lazybox
 * conversation new`), as the Runner lists it among the asking conversation's
 * pending requests: type "conversation_request", the request itself as
 * provider data (runner/conversation_starts.go).
 */
export type StartRequestView = {
  id: string; session: string; conversation: string; workspace: string;
  /** The asking conversation's title. */
  from: string;
  title: string; brief: string; reason: string;
  /** "same" or "new". */
  mode: 'same' | 'new';
  /** mode "new": the branch of origin it starts from; empty for this workspace's branches. */
  startFrom?: string;
  provider: string; profile: string; model: string; permission: string;
  artifact?: { id: string; title: string; version: number };
  /** Conversations live in the asking workspace now, the asking one included. */
  sharedWith: number;
  createdAt: number; expiresAt: number;
};

const text = (value: unknown) => typeof value === 'string' ? value : '';
const number = (value: unknown) => Number(value) || 0;

export function startRequestOf(request: AgentEvent): StartRequestView {
  const data = providerData(request.providerDataJson);
  const artifact = text(data.briefArtifactId);
  return {
    id: text(data.id) || request.requestId, session: text(data.sessionId), conversation: text(data.conversationId), workspace: text(data.collectionId),
    from: text(data.fromTitle), title: text(data.title) || request.text.replace(/^Start a conversation: /, ''), brief: text(data.brief), reason: text(data.reason),
    mode: data.workspace === 'new' ? 'new' : 'same', startFrom: text(data.startFrom) || undefined,
    provider: text(data.provider), profile: text(data.profileId), model: text(data.model), permission: text(data.permission),
    artifact: artifact ? { id: artifact, title: text(data.briefArtifactTitle) || artifact, version: number(data.briefArtifactVersion) } : undefined,
    sharedWith: number(data.sharedWith), createdAt: number(data.createdAtUnixMillis), expiresAt: number(data.expiresAtUnixMillis),
  };
}

/** The brief's first lines, for the card: at most `lines` lines and `limit` characters. */
export function briefPreview(brief: string, lines = 3, limit = 280): { text: string; more: boolean } {
  const all = brief.trim().split('\n').filter(line => line.trim());
  let shown = all.slice(0, lines).join('\n');
  let more = all.length > lines;
  if (shown.length > limit) { shown = shown.slice(0, limit - 1).trimEnd() + '…'; more = true; }
  return { text: shown, more };
}

/** Where the new conversation works, in the person's words. */
export function workspaceWords(view: Pick<StartRequestView, 'mode' | 'sharedWith' | 'startFrom'>): { label: string; detail: string } {
  if (view.mode === 'new') return { label: 'A new workspace', detail: view.startFrom ? `Its own branches, started from origin’s ${view.startFrom}` : 'Its own branches, started from this workspace’s' };
  const others = Math.max(0, view.sharedWith);
  return { label: 'This one', detail: others > 1 ? `Shared with ${others} running conversations: they edit the same files` : others === 1 ? 'Shared with this conversation: they edit the same files' : 'This workspace’s files' };
}

/** One line for the Inbox and status: what the agent asks for. */
export function startRequestSummary(request: AgentEvent): string {
  return `Start a conversation: ${startRequestOf(request).title}`;
}
