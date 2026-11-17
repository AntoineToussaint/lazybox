// THIRD_PARTY_NOTICES.txt for a release (build.mjs): every component the app
// bundles that Lazybox did not write, with its licence, and the full text of
// each distinct licence once, naming the components it covers.
//
//   npm      the production packages of the desktop app and the phone app
//            (bundled into their JavaScript) and of the Claude worker (copied
//            into the app as they are): packages.mjs
//   Go       the modules linked into the lazybox CLI and the Runner
//            (go list -deps), and Go's own standard library and runtime
//   Rust     the crates the desktop app links (cargo metadata, normal
//            dependencies for aarch64-apple-darwin)
//   Node.js  the bundled Node, with its LICENSE (it lists Node's own deps)
//
// The fonts (Inter, JetBrains Mono, Newsreader) are npm packages of the
// interface; they lead the file with their SIL Open Font License text.
//
//   node scripts/release/notices.mjs [--out <file>] [--node-license <file>]
import { spawnSync } from 'node:child_process';
import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { resolve, dirname, join, basename } from 'node:path';
import { fileURLToPath } from 'node:url';
import { productionPackages, licenseFiles } from './packages.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
export const NOTICES_FILE = 'THIRD_PARTY_NOTICES.txt';
/** The workspaces whose production packages ship in the app. */
export const NPM_WORKSPACES = { 'apps/desktop': 'the desktop app', 'apps/web': 'the phone app', 'workers/claude-sdk': 'the Claude worker' };
const GO_MAIN = 'github.com/AntoineToussaint/lazybox/v2';
const FONTS = { '@fontsource-variable/inter': 'Inter', '@fontsource-variable/jetbrains-mono': 'JetBrains Mono', '@fontsource-variable/newsreader': 'Newsreader' };

/**
 * A licence with no open-source grant: the component may still ship (its
 * licence says on what terms), but a person should read it, so the build
 * lists these and the notices say so at the top.
 */
export function needsAttention(license) {
  return !license || /^(UNLICENSED|SEE LICENSE|proprietary|commercial)/i.test(license.trim());
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: 'utf8', maxBuffer: 256 << 20, ...options });
  if (result.status !== 0) throw new Error(`${command} ${args.join(' ')} failed: ${result.stderr || result.error?.message || result.status}`);
  return result.stdout;
}

const readText = path => readFileSync(path, 'utf8').replace(/\r\n/g, '\n').trim();

/** npm packages of every shipped workspace, each once (name@version), with where it ships. */
export function npmComponents(base = root, { omit = () => false } = {}) {
  const byKey = new Map();
  for (const [workspace, where] of Object.entries(NPM_WORKSPACES)) {
    for (const p of productionPackages(base, workspace)) {
      if (omit(p.name)) continue;
      const key = `${p.name}@${p.version}`;
      const files = licenseFiles(p.dir);
      const entry = byKey.get(key) || { kind: 'npm', name: p.name, version: p.version, license: p.license || guessLicense(files), files, in: [] };
      if (!entry.in.includes(where)) entry.in.push(where);
      byKey.set(key, entry);
    }
  }
  return [...byKey.values()].sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));
}

/** Go modules linked into the CLI and the Runner, for the release's platform. */
export function goComponents({ goos = 'darwin', goarch = 'arm64' } = {}) {
  const code = join(root, 'services/runner/code');
  const env = { ...process.env, GOOS: goos, GOARCH: goarch, CGO_ENABLED: '0' };
  const format = '{{with .Module}}{{if not .Main}}{{.Path}}\t{{.Version}}\t{{if .Replace}}{{.Replace.Dir}}{{else}}{{.Dir}}{{end}}{{end}}{{end}}';
  const lines = new Set(run('go', ['list', '-deps', '-f', format, '.', './cmd/lazybox'], { cwd: code, env }).split('\n').filter(Boolean));
  const modules = [...lines].map(line => { const [path, version, dir] = line.split('\t'); return { kind: 'go', name: path, version, dir }; })
    .filter(m => m.name !== GO_MAIN);
  const goroot = run('go', ['env', 'GOROOT'], { cwd: code }).trim();
  const goVersion = run('go', ['env', 'GOVERSION'], { cwd: code }).trim();
  const out = [{ kind: 'go', name: 'Go (standard library and runtime)', version: goVersion, license: 'BSD-3-Clause', files: licenseFiles(goroot) }];
  for (const m of modules.sort((a, b) => a.name.localeCompare(b.name))) {
    const files = m.dir ? licenseFiles(m.dir) : [];
    out.push({ kind: 'go', name: m.name, version: m.version, license: guessLicense(files), files });
  }
  return out;
}

/** Rust crates the desktop app links: normal dependencies, for the release target. */
export function rustComponents({ target = 'aarch64-apple-darwin' } = {}) {
  const manifest = join(root, 'apps/desktop/src-tauri/Cargo.toml');
  const meta = JSON.parse(run('cargo', ['metadata', '--format-version', '1', '--locked', '--filter-platform', target, '--manifest-path', manifest]));
  const nodes = new Map(meta.resolve.nodes.map(n => [n.id, n]));
  const packages = new Map(meta.packages.map(p => [p.id, p]));
  const linked = new Set();
  const queue = [meta.resolve.root];
  while (queue.length) {
    const id = queue.shift();
    for (const dep of nodes.get(id)?.deps || []) {
      if (!dep.dep_kinds.some(k => k.kind === null)) continue; // dev and build dependencies do not ship
      if (linked.has(dep.pkg)) continue;
      linked.add(dep.pkg);
      queue.push(dep.pkg);
    }
  }
  return [...linked].map(id => packages.get(id)).filter(p => p.source) // a path crate is ours
    .map(p => {
      const dir = dirname(p.manifest_path);
      const files = licenseFiles(dir);
      if (p.license_file && existsSync(join(dir, p.license_file)) && !files.includes(join(dir, p.license_file))) files.push(join(dir, p.license_file));
      return { kind: 'rust', name: p.name, version: p.version, license: p.license || (p.license_file ? `see ${p.license_file}` : ''), files };
    })
    .sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));
}

/** A licence name from a component's licence files, for Go modules (which declare none) and packages that do not say. */
function guessLicense(files) {
  const text = files.map(f => { try { return readFileSync(f, 'utf8').slice(0, 4000); } catch { return ''; } }).join('\n');
  if (/Apache License,?\s+Version 2\.0/i.test(text)) return 'Apache-2.0';
  if (/Mozilla Public License,? (version|v\.?) ?2\.0/i.test(text)) return 'MPL-2.0';
  if (/Permission is hereby granted, free of charge/i.test(text)) return 'MIT';
  if (/Redistribution and use in source and binary forms/i.test(text)) return /Neither the name|names of its/i.test(text) ? 'BSD-3-Clause' : 'BSD-2-Clause';
  if (/Permission to use, copy, modify, and\/or distribute/i.test(text)) return 'ISC';
  // Only now: BSD and MIT texts often say "All rights reserved" too.
  if (/All rights reserved|proprietary/i.test(text)) return 'proprietary (see its licence text)';
  return files.length ? 'see its licence text' : '';
}

/**
 * The notices text. Components are listed by kind; each distinct licence
 * text follows once, with the components it covers.
 */
export function renderNotices({ version, components, node }) {
  const texts = new Map(); // hash → { text, file, covers: [] }
  const refs = new Map(); // component label → [hash]
  const label = c => `${c.name} ${c.version}`.trim();
  const add = (component, text, file) => {
    const hash = createHash('sha256').update(text).digest('hex').slice(0, 12);
    if (!texts.has(hash)) texts.set(hash, { text, file, covers: [] });
    if (!texts.get(hash).covers.includes(label(component))) texts.get(hash).covers.push(label(component));
    refs.set(label(component), [...new Set([...(refs.get(label(component)) || []), hash])]);
  };
  const all = [...components, ...(node ? [node] : [])];
  for (const c of all) for (const file of c.files) { try { add(c, readText(file), basename(file)); } catch { /* unreadable: listed without text */ } }
  // A package that ships no licence file still names an SPDX licence: point at its standard text.
  const spdx = license => /^[A-Za-z0-9.+-]+$/.test(license || '') ? ` https://spdx.org/licenses/${license}.html` : '';
  const ref = c => (refs.get(label(c)) || []).map(h => `[${h}]`).join(' ') || `(no licence file in the package; standard text:${spdx(c.license) || ' see the licences named'})`;
  const line = (c, shown = label(c)) => `  ${shown}  —  ${c.license || 'licence not stated'}  ${ref(c)}${c.in ? `  (${c.in.join(', ')})` : ''}`;
  const attention = all.filter(c => needsAttention(c.license));
  const fonts = components.filter(c => FONTS[c.name]);
  const section = (title, list, show = c => line(c)) => list.length ? [title, '', ...list.map(show), ''] : [];
  const out = [
    `Third-party notices for Lazybox ${version}`,
    '',
    'Lazybox includes software written by others. Each component below is',
    'listed with its version, its licence, and references to the licence and',
    'notice texts that follow, each printed once with the components it covers.',
    '',
  ];
  if (attention.length) {
    out.push('Components whose licence is not an open-source licence (read their', 'licence text below for the terms they ship under):', '', ...attention.map(c => line(c)), '');
  }
  out.push(
    ...section('Fonts (in the app\'s interface)', fonts, c => line(c, `${FONTS[c.name]}: ${label(c)}`)),
    ...section('npm packages', components.filter(c => c.kind === 'npm')),
    ...section('Go modules (the lazybox CLI and the Runner)', components.filter(c => c.kind === 'go')),
    ...section('Rust crates (the desktop app)', components.filter(c => c.kind === 'rust')),
    ...section('Node.js (runs the Claude worker)', node ? [node] : []),
    'Licence and notice texts',
    '',
  );
  for (const [hash, { text, file, covers }] of texts) {
    out.push('='.repeat(78), `[${hash}] ${file}`, `Covers: ${covers.join(', ')}`, '-'.repeat(78), text, '');
  }
  return out.join('\n') + '\n';
}

/** Collects every component and writes the notices; returns what needs attention. */
export function writeNotices({ out, version, nodeLicense, nodeVersion = '', omit }) {
  const components = [...npmComponents(root, { omit }), ...goComponents(), ...rustComponents()];
  const node = nodeLicense ? { kind: 'node', name: 'Node.js', version: nodeVersion, license: 'MIT and others (see its LICENSE)', files: [nodeLicense] } : undefined;
  writeFileSync(out, renderNotices({ version, components, node }));
  return {
    components: components.length + (node ? 1 : 0),
    attention: components.filter(c => needsAttention(c.license)).map(c => ({ name: c.name, version: c.version, license: c.license || '' })),
  };
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  const option = name => { const i = args.indexOf(name); return i >= 0 ? args[i + 1] : undefined; };
  const out = resolve(option('--out') || NOTICES_FILE);
  const report = writeNotices({ out, version: option('--version') || 'dev', nodeLicense: option('--node-license') });
  console.log(`Wrote ${out}: ${report.components} components`);
  for (const a of report.attention) console.log(`  ! ${a.name} ${a.version}: ${a.license || 'no licence stated'}`);
}
