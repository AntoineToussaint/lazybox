// The production npm packages a workspace needs, found the way Node finds
// them: from each package.json's dependencies, through node_modules on disk.
//
// build.mjs copies the worker's into the app with this, and notices.mjs
// lists every bundled package's licence. It used to read `npm ls --parseable`,
// whose text npm redacts: a path segment that looks like a UUID (a worktree
// or a CI checkout under one) comes out as `***`, and the copy found nothing.
import { existsSync, readFileSync, readdirSync, realpathSync } from 'node:fs';
import { dirname, join, relative, sep } from 'node:path';

const readJSON = path => JSON.parse(readFileSync(path, 'utf8'));
const inside = (parent, child) => { const rel = relative(parent, child); return rel !== '' && !rel.startsWith('..') && !rel.startsWith(sep); };

/** Where Node would load `name` from, required from `from`: the nearest node_modules/name up to root. */
export function resolvePackage(name, from, root) {
  for (let dir = from; ; dir = dirname(dir)) {
    if (!dir.endsWith(`${sep}node_modules`)) {
      const candidate = join(dir, 'node_modules', name);
      if (existsSync(join(candidate, 'package.json'))) return candidate;
    }
    if (dir === root || !inside(root, dir)) return undefined;
  }
}

/**
 * Every package `workspace` (a folder under root) needs at run time, each
 * once: its dependencies and optional dependencies, theirs, and so on, plus
 * peer dependencies that are installed. Optional ones not installed on this
 * platform are skipped; a missing required one is an error. Other workspaces
 * it depends on (@lazybox/*) are followed but not listed: they are ours, and
 * bundled from source.
 *
 * Each entry: name, version, license, its directory on disk (`dir`, real),
 * and `path`: where it sits relative to the node_modules it was found in, so
 * a copy under another node_modules resolves the same way.
 */
/**
 * The SDK's per-platform packages hold Anthropic's Claude Code program
 * (`claude`). A release built --without-claude-code leaves them out
 * (CLAUDE_COMPLIANCE.md): the Runner then runs the claude the person installed.
 */
export const isClaudeCodePackage = name => /^@anthropic-ai\/claude-agent-sdk-[a-z0-9]+-[a-z0-9]+(-[a-z0-9]+)?$/.test(name);

export function productionPackages(root, workspace) {
  root = realpathSync(root);
  const start = realpathSync(join(root, workspace));
  const queue = [start];
  const visited = new Set([start]);
  const found = new Map();
  while (queue.length) {
    const from = queue.shift();
    const manifest = readJSON(join(from, 'package.json'));
    const wanted = [
      ...Object.keys(manifest.dependencies || {}).map(name => [name, false]),
      ...Object.keys(manifest.optionalDependencies || {}).map(name => [name, true]),
      ...Object.keys(manifest.peerDependencies || {}).map(name => [name, true]),
    ];
    for (const [name, optional] of wanted) {
      const location = resolvePackage(name, from, root);
      if (!location) {
        if (optional) continue;
        throw new Error(`${name}, a dependency of ${manifest.name || relative(root, from)}, is not installed: run npm ci`);
      }
      const real = realpathSync(location);
      if (visited.has(real)) continue;
      visited.add(real);
      queue.push(real);
      if (!real.split(sep).includes('node_modules')) continue; // another workspace of this repository
      const own = readJSON(join(real, 'package.json'));
      const modules = location.slice(0, location.indexOf(`${sep}node_modules${sep}`) + `${sep}node_modules`.length);
      found.set(real, { name: own.name || name, version: own.version || '', license: licenseOf(own), dir: real, path: relative(modules, location), from: modules });
    }
  }
  return [...found.values()].sort((a, b) => a.path.localeCompare(b.path));
}

/** A package.json's licence, as SPDX-ish text. */
export function licenseOf(manifest) {
  const { license, licenses } = manifest;
  if (typeof license === 'string') return license;
  if (license?.type) return license.type;
  if (Array.isArray(licenses)) return licenses.map(l => l.type || l).join(' OR ');
  return '';
}

/** The licence and notice files a package ships at its top level. */
export function licenseFiles(dir) {
  let names = [];
  try { names = readdirSync(dir, { withFileTypes: true }).filter(e => e.isFile()).map(e => e.name); } catch { return []; }
  return names.filter(n => /^(licen[cs]e|copying|notice|copyright|unlicense|ofl)([-._].*)?$/i.test(n)).sort().map(n => join(dir, n));
}
