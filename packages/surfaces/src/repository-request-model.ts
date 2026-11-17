import { providerData } from '@lazybox/client-core';
import type { AgentEvent } from '@lazybox/protocol';

/**
 * An agent's request to add a repository to its workspace, as the Runner
 * lists it among a conversation's pending requests (type
 * "repository_request", the request itself as provider data).
 */
export type RepositoryCandidateView = { identity: string; name: string; source: string; localPath: string; score: number };
export type RepositoryRequestView = {
  id: string; workspace: string; query: string; reason: string; branch: string;
  candidates: RepositoryCandidateView[];
  createdAt: number; expiresAt: number;
};

const text = (value: unknown) => typeof value === 'string' ? value : '';
const number = (value: unknown) => Number(value) || 0;

export function repositoryRequestOf(request: AgentEvent): RepositoryRequestView {
  const data = providerData(request.providerDataJson);
  const candidates: RepositoryCandidateView[] = (Array.isArray(data.candidates) ? data.candidates : []).map((c: any) => ({
    identity: text(c?.identity), name: text(c?.name) || text(c?.identity) || text(c?.localPath), source: text(c?.source), localPath: text(c?.localPath), score: number(c?.score),
  })).filter((c: RepositoryCandidateView) => c.name);
  return { id: text(data.id) || request.requestId, workspace: text(data.collectionId), query: text(data.query), reason: text(data.reason), branch: text(data.branch), candidates,
    createdAt: number(data.createdAtUnixMillis), expiresAt: number(data.expiresAtUnixMillis) };
}

/** Where a candidate comes from, in the person's words. */
export function sourceLabel(candidate: RepositoryCandidateView): string {
  switch (candidate.source) {
    case 'registered': return 'Already on this computer';
    case 'local': return 'Local checkout';
    case 'github': return 'GitHub · cloned when added';
    case 'folder': return 'Local folder, not in your repositories yet';
  }
  return 'Repository catalog';
}

/** The short tag the card's header shows for the source. */
export function sourceTag(candidate?: RepositoryCandidateView): string | undefined {
  if (!candidate) return undefined;
  return ({ registered: 'Local', local: 'Local', github: 'GitHub', folder: 'Folder' } as Record<string, string>)[candidate.source];
}

/** The repository name without its owner: "acme/module" → "module". */
export function shortName(name: string): string {
  const slash = name.lastIndexOf('/');
  return slash >= 0 ? name.slice(slash + 1) : name;
}

/**
 * The repository picker's search for "Choose another…", so the likely
 * repositories show together: the agent's own word when the best match
 * contains it ("tool" shows tools and tools-legacy), else the best match's
 * name ("module_directory" shows every module-directory), else the word.
 */
export function pickerQuery(request: RepositoryRequestView): string {
  const asked = shortName(request.query.trim().toLowerCase().replace(/\.git$/, '').replace(/\/+$/, ''));
  const top = request.candidates[0] ? shortName(request.candidates[0].name).toLowerCase() : '';
  if (!top) return asked;
  return asked.length >= 2 && top.includes(asked) ? asked : top;
}

/** A stable request key per request and repository: a retried Add is the same addition. */
export function additionKey(request: string, identity: string): string {
  let hash = 0x811c9dc5;
  for (let i = 0; i < identity.length; i++) { hash ^= identity.charCodeAt(i); hash = Math.imul(hash, 0x01000193) >>> 0; }
  return `repository-request:${request}:${hash.toString(16).padStart(8, '0')}`;
}

/** One line for the inbox and status: what the agent asks for. */
export function repositoryRequestSummary(request: AgentEvent): string {
  const view = repositoryRequestOf(request);
  const name = view.candidates[0]?.name || view.query;
  return name ? `Add ${name} to this workspace` : request.text || 'Add a repository to this workspace';
}
