// Artifacts for the fake Runner (runner-ui.mjs): the RPCs of ARTIFACTS.md with
// the Runner's rules — append-only versions, an unchanged put is no version,
// a stale expected version is refused, tracking a file twice is one artifact,
// a retired artifact leaves the default list and is restorable —
// and `fileChanged`, which stands in for the Runner's file watcher.
import { createHash, randomUUID } from 'node:crypto';

const KINDS = new Set(['plan', 'report', 'spec', 'notes', 'todo-list', 'other']);
const digest = text => createHash('sha256').update(text).digest('hex');

export function artifactFixture({ workspaceFiles, mutations }) {
  /** id → { meta, versions: [{ header, content }] } */
  const artifacts = new Map();
  let clock = Date.now() - 3_600_000;
  const now = () => (clock = Math.max(clock + 1, Date.now()));
  const wire = (entry, version) => {
    const shown = version ? entry.versions.find(v => v.header.number === version) : entry.versions.at(-1);
    return { ...entry.meta, versions: entry.versions.map(v => v.header), ...(shown === undefined ? {} : { content: shown.content, contentVersion: shown.header.number }) };
  };
  const summary = entry => { const { content, contentVersion, ...rest } = wire(entry); return rest; };
  const append = (entry, content, author, source) => {
    const number = entry.versions.length + 1, at = now();
    entry.versions.push({ header: { number, createdAtUnixMillis: String(at), author, source, digest: digest(content), size: String(Buffer.byteLength(content)) }, content });
    entry.meta.updatedAtUnixMillis = String(at);
  };
  const create = ({ collectionId, title, kind, content, author, source, trackedPath = '', format = 'markdown', links = [] }) => {
    const id = randomUUID().replace(/-/g, '').slice(0, 20).toUpperCase(), at = now();
    const entry = { meta: { id, collectionId, links, title, kind: KINDS.has(kind) ? kind : 'other', format, createdAtUnixMillis: String(at), updatedAtUnixMillis: String(at), originSessionId: source?.sessionId || '', originConversationId: source?.conversationId || '', trackedPath, trackingProblem: '' }, versions: [] };
    append(entry, content, author, source);
    artifacts.set(id, entry);
    return entry;
  };
  const find = (collectionId, id) => { const entry = artifacts.get(id); return entry && entry.meta.collectionId === collectionId ? entry : undefined; };
  const retire = (entry, by, supersededBy = '', reason = '') => { entry.meta.retired = { atUnixMillis: String(now()), by, supersededBy, reason }; };
  const firstHeading = text => /^\s{0,3}#\s+(.+)$/m.exec(text)?.[1].trim();

  const handle = (method, body, json, error) => {
    switch (method) {
      case 'ListArtifacts': return json({ artifacts: [...artifacts.values()].filter(e => e.meta.collectionId === body.collectionId && (body.includeRetired || !e.meta.retired)).sort((a, b) => Number(b.meta.updatedAtUnixMillis) - Number(a.meta.updatedAtUnixMillis)).map(summary) });
      case 'GetArtifact': {
        const entry = find(body.collectionId, body.artifactId);
        if (!entry) return error('not_found', 'artifact not found');
        if (body.version && !entry.versions.some(v => v.header.number === body.version)) return error('not_found', `artifact has no version ${body.version}`);
        return json(wire(entry, body.version || 0));
      }
      case 'PutArtifact': {
        mutations.push({ method, body });
        const content = body.content || '';
        if (!content.trim()) return error('invalid_argument', 'artifact content is empty');
        const author = { kind: 'person' }, source = body.source || { kind: 'edit' };
        if (!body.artifactId) {
          if (!body.title?.trim()) return error('invalid_argument', 'a title is required');
          return json(wire(create({ collectionId: body.collectionId, title: body.title.trim(), kind: body.kind, content, author, source, format: body.format, links: body.links })));
        }
        const entry = find(body.collectionId, body.artifactId);
        if (!entry) return error('not_found', 'artifact not found');
        const latest = entry.versions.at(-1);
        if (body.expectedVersion && body.expectedVersion !== latest.header.number) return error('aborted', `a newer version (v${latest.header.number}) exists; review it before saving`);
        if (body.title) entry.meta.title = body.title;
        if (body.kind && KINDS.has(body.kind)) entry.meta.kind = body.kind;
        if (latest.content !== content) append(entry, content, author, source);
        return json(wire(entry));
      }
      case 'TrackArtifactFile': {
        mutations.push({ method, body });
        // The fixture workspace has no root on disk: an absolute path is somewhere else, as the Runner says.
        if (String(body.path).startsWith('/')) return error('invalid_argument', 'file is outside this workspace');
        const existing = [...artifacts.values()].find(e => e.meta.collectionId === body.collectionId && e.meta.trackedPath === body.path);
        if (existing) return json(wire(existing));
        const file = workspaceFiles.get(body.collectionId + '/' + body.path) || { text: '# File preview\nA retained file.\n' };
        const title = body.title || firstHeading(file.text) || body.path.split('/').at(-1).replace(/\.[^.]+$/, '');
        return json(wire(create({ collectionId: body.collectionId, title, kind: body.kind, content: file.text, author: { kind: 'person' }, source: { kind: 'file', path: body.path }, trackedPath: body.path })));
      }
      case 'UpdateArtifact': {
        mutations.push({ method, body });
        const entry = find(body.collectionId, body.artifactId);
        if (!entry) return error('not_found', 'artifact not found');
        if (body.title) entry.meta.title = body.title.trim();
        if (body.kind && KINDS.has(body.kind)) entry.meta.kind = body.kind;
        if (body.stopTracking) { entry.meta.trackedPath = ''; entry.meta.trackingProblem = ''; }
        entry.meta.updatedAtUnixMillis = String(now());
        return json(wire(entry));
      }
      case 'RetireArtifact': {
        mutations.push({ method, body });
        const entry = find(body.collectionId, body.artifactId);
        if (!entry) return error('not_found', 'artifact not found');
        if (body.restore) delete entry.meta.retired;
        else {
          if (body.supersededBy && !find(body.collectionId, body.supersededBy)) return error('invalid_argument', 'the superseding artifact is not in this workspace');
          retire(entry, { kind: 'person' }, body.supersededBy || '', body.reason || '');
        }
        return json(wire(entry));
      }
      case 'DeleteArtifact': mutations.push({ method, body }); artifacts.delete(find(body.collectionId, body.artifactId)?.meta.id); return json({});
      case 'SaveArtifactToRepository': {
        mutations.push({ method, body });
        const entry = find(body.collectionId, body.artifactId);
        if (!entry) return error('not_found', 'artifact not found');
        const key = body.collectionId + '/' + body.path, existing = workspaceFiles.get(key);
        if (existing && !body.expectedFileVersion) return error('already_exists', `a file already exists at ${body.path}`);
        if (existing && existing.version !== body.expectedFileVersion) return error('aborted', 'the file changed');
        workspaceFiles.set(key, { path: body.path, text: entry.versions.at(-1).content, version: randomUUID(), binary: false, truncated: false });
        if (body.track) entry.meta.trackedPath = body.path;
        return json({ path: body.path, artifact: wire(entry) });
      }
      case 'CreateArtifactIssue': mutations.push({ method, body }); return json({ number: '42', url: `https://github.com/fixture-team/${body.member}/issues/42` });
      default: return false;
    }
  };
  /** The Runner's watcher saw a tracked file change on disk. */
  const fileChanged = (collectionId, path, text) => {
    const key = collectionId + '/' + path;
    workspaceFiles.set(key, { ...(workspaceFiles.get(key) || { path, binary: false, truncated: false }), text, version: randomUUID() });
    for (const entry of artifacts.values()) if (entry.meta.collectionId === collectionId && entry.meta.trackedPath === path && entry.versions.at(-1).content !== text) append(entry, text, { kind: 'file' }, { kind: 'file', path });
  };
  /** An agent's `lazybox artifact put`; with `supersedes`, the artifacts it retires as replaced by this one. */
  const agentPut = ({ collectionId, sessionId, conversationId = '', title, kind, content, artifactId, format, supersedes = [] }) => {
    const source = { kind: 'agent', sessionId, conversationId }, author = { kind: 'agent', sessionId };
    const entry = artifactId ? find(collectionId, artifactId) : create({ collectionId, title, kind, content, author, source, format });
    if (artifactId) append(entry, content, author, source);
    for (const id of supersedes) retire(find(collectionId, id), author, entry.meta.id);
    return wire(entry);
  };
  /** An agent's `lazybox artifact retire`. */
  const agentRetire = ({ collectionId, sessionId, artifactId, supersededBy = '', reason = '' }) => { const entry = find(collectionId, artifactId); retire(entry, { kind: 'agent', sessionId }, supersededBy, reason); return wire(entry); };
  return { artifacts, handle, fileChanged, agentPut, agentRetire };
}
