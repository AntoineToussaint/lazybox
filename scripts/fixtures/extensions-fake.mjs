// An explicitly fake set of extensions (EXTENSIONS.md) for the browser
// fixture: the extension folders are real (a temporary directory, the
// bundled examples copied into it), and so is the content origin — a loopback
// server of its own that serves Lazybox's runtime files and each extension's
// own files under /x/<token>/ with the Runner's two policies, read from the
// same policy.json. What the fake leaves out is the Runner's parser (a small
// one reads the manifests' subset here; the Go tests cover the real one) and
// its persistence: approvals live in memory.
import { createServer } from 'node:http';
import { cp, mkdtemp, mkdir, readdir, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, extname, normalize } from 'node:path';
import { createHash, randomBytes } from 'node:crypto';
import { fileURLToPath } from 'node:url';

const power = fileURLToPath(new URL('../../services/runner/code/internal/power/extensions/', import.meta.url));
const runtimeDir = join(power, 'runtime'), examplesDir = join(power, 'examples');
const policy = JSON.parse(await readFile(join(runtimeDir, 'policy.json'), 'utf8'));
const types = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.mjs': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8', '.json': 'application/json', '.svg': 'image/svg+xml', '.png': 'image/png' };
const runtimeFiles = ['shell.html', 'shell.js', 'shell.css', 'lazybox.js', 'lazybox.css', 'workflow.js'];

/** The manifests' YAML subset: scalars, `key: [a, b]`, lists of scalars and of `- id: …` mappings. */
export function parseManifest(text) {
  const out = {}; let list = null, item = null;
  const scalar = v => { v = v.trim(); if (/^".*"$/.test(v)) return JSON.parse(v); if (v.startsWith('[') && v.endsWith(']')) return v.slice(1, -1).split(',').map(s => s.trim()).filter(Boolean); return v; };
  for (const raw of text.split('\n')) {
    const line = raw.replace(/\s+#.*$/, '').replace(/^#.*$/, '');
    if (!line.trim()) continue;
    let m;
    if ((m = /^([\w-]+):\s*(.*)$/.exec(line))) { list = null; item = null; if (m[2]) out[m[1]] = scalar(m[2]); else { out[m[1]] = []; list = out[m[1]]; } continue; }
    if (list && (m = /^\s+-\s+([\w-]+):(?:\s+(.*))?$/.exec(line))) { item = { [m[1]]: scalar(m[2] || '') }; list.push(item); continue; }
    if (list && (m = /^\s+-\s+(.*)$/.exec(line))) { item = null; list.push(scalar(m[1])); continue; }
    if (item && (m = /^\s+([\w-]+):(?:\s+(.*))?$/.exec(line))) item[m[1]] = scalar(m[2] || '');
  }
  return out;
}

const subset = (a, b) => a.every(v => b.includes(v));

export async function createExtensionsFake({ mutations }) {
  const dir = await mkdtemp(join(tmpdir(), 'lazybox-extensions-'));
  const approvals = new Map(), sessions = new Map(), creds = new Map();
  let revision = 1, origin = '';
  const examples = [];
  for (const id of await readdir(examplesDir)) { const m = parseManifest(await readFile(join(examplesDir, id, 'extension.yaml'), 'utf8')); examples.push({ id, name: m.name, description: m.description || '' }); }

  async function scan(id) {
    const folder = join(dir, id);
    const files = await readdir(folder, { recursive: true });
    let digest = '';
    for (const f of files.sort()) { const s = await stat(join(folder, f)); if (s.isFile()) digest += `${f}:${s.size}:${s.mtimeMs};`; }
    const text = await readFile(join(folder, 'extension.yaml'), 'utf8').catch(() => '');
    const m = parseManifest(text);
    const panels = (m.panels || []).map(p => ({ id: p.id, title: p.title, icon: p.icon || 'puzzle', entry: p.entry || 'index.html', description: p.description || '' }));
    const a = approvals.get(id);
    const grants = m.grants || [], hosts = m.network || [];
    const issues = [];
    for (const [i, g] of grants.entries()) if (!/^(read:(workspaces|sessions|attention|commands|missions|usage|runtime|pulls|releases|artifacts|mailbox)|show:(conversations|runtime|github)|scan:disk|draft:artifact|send:messages|switch:(claude|codex)|do:[\w.-]+)$/.test(g)) issues.push({ path: `grants[${i}]`, line: 1, message: `there is no grant "${g}"`, severity: 'error' });
    // An account policy (EXTENSIONS.md): the subset parser keeps its key; its two fields are read from the text.
    const accountPolicy = m.accountPolicy !== undefined ? { providers: grants.filter(g => g.startsWith('switch:')).map(g => g.slice(7)), switchOnLimit: /^\s+switchOnLimit:\s*true\b/m.test(text), accounts: [] } : undefined;
    const state = issues.length || !m.name ? 'broken' : !a ? 'new' : !a.enabled ? 'disabled' : !subset(grants, a.grants) || !subset(hosts, a.hosts) ? 'review' : 'enabled';
    return { id, name: m.name || '', version: m.version || '', description: m.description || '', author: m.author || '', homepage: '', path: folder, state, panels, grants, hosts, ...(accountPolicy ? { accountPolicy } : {}), approvedGrants: a?.grants || [], approvedHosts: a?.hosts || [], approvedAtUnixMillis: a ? String(a.at) : '0', error: m.name ? '' : 'the manifest needs a name', issues, digest: createHash('sha256').update(digest).digest('hex').slice(0, 16) };
  }
  async function list() {
    const extensions = [];
    for (const entry of (await readdir(dir, { withFileTypes: true })).filter(e => e.isDirectory()).sort((a, b) => a.name.localeCompare(b.name))) extensions.push(await scan(entry.name));
    // Sessions whose grants no longer hold are revoked, as the Runner does.
    for (const [token, s] of sessions) {
      const ext = extensions.find(e => e.id === s.id);
      const effective = ext && (ext.state === 'enabled' || ext.state === 'review') ? [ext.grants.filter(g => ext.approvedGrants.includes(g)).join(), ext.hosts.filter(h => ext.approvedHosts.includes(h)).join()].join('|') : '';
      if (effective !== s.signature) { sessions.delete(token); creds.delete(s.credential); }
    }
    return { directory: dir, extensions, examples: examples.map(x => ({ ...x, installed: extensions.some(e => e.id === x.id) })), revision: String(revision++), protocol: 1 };
  }

  const server = createServer(async (req, res) => {
    const headers = { 'X-Content-Type-Options': 'nosniff', 'Cache-Control': 'no-store', 'Referrer-Policy': 'no-referrer' };
    const url = new URL(req.url, 'http://x');
    const [, x, token, ...rest] = url.pathname.split('/');
    const s = x === 'x' ? sessions.get(token) : undefined;
    if (!s) { res.writeHead(410, headers); res.end('closed'); return; }
    const file = rest.join('/');
    headers['Access-Control-Allow-Origin'] = '*';
    const base = `${origin}/x/${token}/`;
    const extensionPolicy = policy.extension.replaceAll('{base}', base).replaceAll('{hosts}', s.hosts.length ? s.hosts.join(' ') : "'none'");
    if (file.startsWith('_lazybox/')) {
      const name = file.slice('_lazybox/'.length);
      if (name === 'disk-report.html' && s.grants.includes('scan:disk')) {
        res.writeHead(200, { ...headers, 'Content-Type': types['.html'], 'Content-Security-Policy': extensionPolicy });
        res.end('Waiting for the first scan.');
        return;
      }
      if (!runtimeFiles.includes(name)) { res.writeHead(404, headers); res.end(); return; }
      const ancestors = /^https?:\/\//.test(s.origin) ? `; frame-ancestors ${s.origin}` : '';
      headers['Content-Security-Policy'] = name === 'shell.html' ? policy.shell.replaceAll('{base}', base).replaceAll('{frameAncestors}', ancestors) : extensionPolicy;
      res.writeHead(200, { ...headers, 'Content-Type': types[extname(name)] }); res.end(await readFile(join(runtimeDir, name))); return;
    }
    const clean = normalize(file);
    if (!file || clean !== file || clean.startsWith('..') || clean.split('/').some(p => p.startsWith('.')) || clean === 'extension.yaml' || !types[extname(clean)]) { res.writeHead(404, headers); res.end(); return; }
    try { const data = await readFile(join(dir, s.id, clean)); res.writeHead(200, { ...headers, 'Content-Type': types[extname(clean)], 'Content-Security-Policy': extensionPolicy }); res.end(data); }
    catch { res.writeHead(404, headers); res.end(); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  origin = `http://127.0.0.1:${server.address().port}`;

  const fake = {
    dir, origin, approvals, sessions, creds,
    /** RPCs extension credentials made, and those refused. */
    calls: [], denied: [],
    /** Writes an extension folder (a test's own, such as a malicious one). */
    async addFolder(id, files) { await mkdir(join(dir, id), { recursive: true }); for (const [name, text] of Object.entries(files)) await writeFile(join(dir, id, name), text); },
    /** A save in an editor: the panel reloads. */
    async writeFile(id, name, text) { await writeFile(join(dir, id, name), text); },
    /** The grants an extension credential holds, or undefined for a forged or revoked one. */
    scope(credential) { return creds.get(credential); },
    /** The providers an enabled extension's account policy switches (RunnerSettings.automaticSwitchingProviders). */
    async switching() {
      const on = (await list()).extensions.filter(e => (e.state === 'enabled' || e.state === 'review') && e.accountPolicy?.switchOnLimit);
      return ['claude', 'codex'].filter(p => on.some(e => e.grants.includes(`switch:${p}`) && e.approvedGrants.includes(`switch:${p}`)));
    },
    async handle(method, body) {
      switch (method) {
        case 'ListExtensions': return { value: await list() };
        case 'InstallExampleExtension': {
          mutations.push({ method, body });
          if (!examples.some(x => x.id === body.example)) return { error: { code: 'failed_precondition', message: `there is no example "${body.example}"` } };
          if (existsSync(join(dir, body.example))) return { error: { code: 'failed_precondition', message: 'already exists; an existing extension is never replaced' } };
          await cp(join(examplesDir, body.example), join(dir, body.example), { recursive: true });
          return { value: await list() };
        }
        case 'ApproveExtension': {
          mutations.push({ method, body });
          const ext = (await list()).extensions.find(e => e.id === body.id);
          if (!ext) return { error: { code: 'not_found', message: 'there is no such extension' } };
          const grants = body.grants || [], hosts = body.hosts || [];
          if (ext.state === 'broken' || !subset(grants, ext.grants) || !subset(ext.grants, grants) || !subset(hosts, ext.hosts) || !subset(ext.hosts, hosts)) return { error: { code: 'failed_precondition', message: `${ext.name} asks for something else now (its manifest changed); review it again` } };
          approvals.set(body.id, { grants: ext.grants, hosts: ext.hosts, enabled: true, at: Date.now() });
          return { value: await list() };
        }
        case 'SetExtensionEnabled': {
          mutations.push({ method, body });
          const a = approvals.get(body.id), ext = (await list()).extensions.find(e => e.id === body.id);
          if (!a || !ext) return { error: { code: 'failed_precondition', message: 'review what it asks for first' } };
          if (body.enabled && (!subset(ext.grants, a.grants) || !subset(ext.hosts, a.hosts))) return { error: { code: 'failed_precondition', message: `${ext.name} asks for more than you approved; review it first` } };
          a.enabled = !!body.enabled;
          return { value: await list() };
        }
        case 'RemoveExtension': {
          mutations.push({ method, body });
          await rm(join(dir, body.id), { recursive: true, force: true }); approvals.delete(body.id);
          return { value: await list() };
        }
        case 'OpenExtensionPanel': {
          const ext = (await list()).extensions.find(e => e.id === body.id);
          if (!ext) return { error: { code: 'not_found', message: 'there is no such extension' } };
          if (ext.state !== 'enabled' && ext.state !== 'review') return { error: { code: 'failed_precondition', message: `${ext.name} is not approved yet: review what it asks for in Settings → Extensions` } };
          const panel = ext.panels.find(p => p.id === body.panel) || ext.panels[0];
          const grants = ext.grants.filter(g => ext.approvedGrants.includes(g)), hosts = ext.hosts.filter(h => ext.approvedHosts.includes(h));
          const token = randomBytes(18).toString('base64url'), credential = `lbx-ext.${randomBytes(24).toString('base64url')}`;
          const s = { id: ext.id, token, credential, grants, hosts, origin: body.origin || '', signature: [grants.join(), hosts.join()].join('|') };
          sessions.set(token, s); creds.set(credential, s);
          return { value: { url: `${origin}/x/${token}/_lazybox/shell.html?entry=${encodeURIComponent(panel.entry)}`, contentOrigin: origin, credential, grants, hosts, digest: ext.digest, protocol: 1 } };
        }
      }
      return undefined;
    },
    async close() { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); await rm(dir, { recursive: true, force: true }); },
  };
  return fake;
}

/**
 * What an extension credential may call, by grant (transport/extension_auth.go,
 * mirrored): the fixture refuses everything else, as the Runner does.
 */
export function extensionAllows(grants, method, body) {
  const has = g => grants.includes(g);
  switch (method) {
    case 'ListHistory': return has('read:sessions') || has('read:attention');
    case 'ListCollections': return has('read:workspaces');
    case 'ListCommands': return has('read:commands') && !!body.sessionId;
    case 'GetWorkspaceRuntime': return has('read:runtime') && !!body.collectionId;
    case 'ListLinks': return has('read:pulls') && !body.sessionId && !body.focused && !!(body.all || body.collectionId);
    case 'ListArtifacts': return has('read:artifacts') && !!body.collectionId && !body.includeRetired;
    case 'ListReleaseWatch': return has('read:releases');
    case 'ListMailbox': return has('read:mailbox');
    case 'ListMissions': return has('read:missions');
    case 'GetTelemetry': return has('read:usage');
    case 'Apply': return has('send:messages') && !!body.message && Object.keys(body).every(k => ['sessionId', 'operationId', 'message'].includes(k));
    case 'PostUiRequest':
      if (body.kind === 'open') return has('show:conversations') && !!(body.sessionId || body.collectionId) && !body.file && !body.panel && !body.line;
      return body.kind === 'do' && has(`do:${body.actionId}`);
    case 'WaitUiRequest': return grants.some(g => g === 'show:conversations' || g.startsWith('do:'));
  }
  return false;
}
