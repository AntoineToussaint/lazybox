#!/usr/bin/env node
// Checks that what the front-door documents tell a person to run, and link
// to, exists in the repository they cloned (the standalone repository that
// scripts/export-repo.mjs writes):
//
//   node scripts/export-check.mjs [repository root]     default: this repository
//
// In README.md, CONTRIBUTING.md, SUPPORT.md, SECURITY.md and the install and
// testers' pages:
//   - `make <target>` names a target of the Makefile, and nothing says
//     `make -C v2` (the monorepo's spelling);
//   - `npm run <script>` names a script of package.json;
//   - `node <file>` names a file;
//   - a repository path in a code span (`scripts/release-gates.json`) exists;
//   - a relative link or image exists, and its #fragment names a heading;
//   - a link to a file of this repository on GitHub names a file that exists.
//
// export-repo.mjs runs it on every export; the standalone repository's CI
// runs it too. It reads files only: nothing is built or run.
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, posix, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const FRONT_DOOR = ['README.md', 'CONTRIBUTING.md', 'SUPPORT.md', 'SECURITY.md'];
/** Pages whose commands are checked too (their links are the docs site's: npm run docs:check). */
export const COMMAND_PAGES = ['docs/install.md', 'docs/getting-started.md', 'docs/testers.md'];
const REPOSITORY = /^https:\/\/github\.com\/AntoineToussaint\/lazybox\/(?:blob|tree)\/main\/([^#?]+)/;

/** The Makefile's targets: every name before a rule's colon, and every .PHONY. */
export function makeTargets(makefile) {
  const targets = new Set();
  for (const line of makefile.split('\n')) {
    if (/^\s/.test(line) || /^[A-Z_]+\s*[:?+]?=/.test(line)) continue;
    const rule = /^([^:#=]+?)\s*:(?![=])/.exec(line);
    if (rule) for (const name of rule[1].trim().split(/\s+/)) targets.add(name);
  }
  for (const phony of makefile.matchAll(/^\.PHONY:\s*(.+)$/gm)) for (const name of phony[1].trim().split(/\s+/)) targets.add(name);
  return targets;
}

/** Code: fenced blocks and inline spans, with their line numbers. */
export function codeOf(markdown) {
  const out = [];
  const lines = markdown.split('\n');
  let fence;
  lines.forEach((line, index) => {
    const marker = /^\s*(```|~~~)/.exec(line)?.[1];
    if (marker && !fence) { fence = marker; return; }
    if (marker && fence === marker) { fence = undefined; return; }
    if (fence) { out.push({ text: line.replace(/\s#\s.*$/, ''), line: index + 1, block: true }); return; }
    for (const span of line.matchAll(/`([^`\n]+)`/g)) out.push({ text: span[1], line: index + 1, block: false });
  });
  return out;
}

/** Prose links and images (not inside code), with their line numbers. */
export function linksOf(markdown) {
  const out = [];
  let fence = false;
  markdown.split('\n').forEach((line, index) => {
    if (/^\s*(```|~~~)/.test(line)) { fence = !fence; return; }
    if (fence) return;
    const text = line.replace(/`[^`\n]*`/g, '');
    for (const m of text.matchAll(/\]\(([^)\s]+)\)/g)) out.push({ target: m[1], line: index + 1 });
    for (const m of text.matchAll(/\s(?:src|srcset|href)="([^"]+)"/g)) out.push({ target: m[1], line: index + 1 });
  });
  return out;
}

/** GitHub's heading anchors (scripts/docs-links.test.ts `anchors`). */
export function anchors(markdown) {
  const seen = new Map(), out = new Set();
  const text = markdown.replace(/^(```|~~~)[^\n]*\n[\s\S]*?^\1[^\n]*$/gm, '');
  for (const match of text.matchAll(/^#{1,6}[ \t]+(.+?)[ \t#]*$/gm)) {
    const plain = match[1].replace(/!\[([^\]]*)\]\([^)]*\)/g, '$1').replace(/\[([^\]]*)\]\([^)]*\)/g, '$1').replace(/<[^>]+>/g, '').replace(/[*`]/g, '');
    const base = plain.trim().toLowerCase().replace(/[^\p{L}\p{M}\p{N}\p{Pc}\- ]/gu, '').replace(/ /g, '-');
    const count = seen.get(base) ?? 0;
    seen.set(base, count + 1);
    out.add(count ? `${base}-${count}` : base);
  }
  return out;
}

/** Every problem with one document of the repository at `root`. */
export function checkDocument(root, path, { links = true, targets, scripts } = {}) {
  const problems = [];
  const text = readFileSync(join(root, path), 'utf8');
  const where = line => `${path}:${line}`;
  const topLevel = new Set(readdirSync(root));
  const exists = file => existsSync(join(root, file));
  for (const { text: code, line } of codeOf(text)) {
    if (/make -C v2\b/.test(code)) problems.push(`${where(line)}: \`make -C v2\` is the monorepo's spelling; here it is \`make\``);
    for (const m of code.matchAll(/(?:^|[\s;&|(])make\s+((?:-[A-Za-z]\s*\S+\s+)*)([a-z][\w.-]*)/g)) {
      if (m[1].trim()) continue;
      if (!targets.has(m[2])) problems.push(`${where(line)}: make ${m[2]}: no such target in the Makefile`);
    }
    for (const m of code.matchAll(/npm run\s+([\w:.-]+)/g)) if (!(m[1] in scripts)) problems.push(`${where(line)}: npm run ${m[1]}: no such script in package.json`);
    for (const m of code.matchAll(/(?:^|[\s;&|(])node\s+(?:--[\w-]+(?:\s+[\w-]+)?\s+)*([\w./-]+\.m?[jt]s)\b/g)) if (!exists(m[1])) problems.push(`${where(line)}: node ${m[1]}: no such file`);
    // A path in this repository: its first segment is a top-level entry, and
    // it is spelled out (no placeholder, glob or ellipsis).
    const path = code.trim();
    if (/^[\w.-]+(?:\/[\w.-]+)+\/?$/.test(path) && topLevel.has(path.split('/')[0]) && !exists(path)) problems.push(`${where(line)}: ${path}: no such file or folder`);
  }
  if (!links) return problems;
  const directory = posix.dirname(path);
  for (const { target, line } of linksOf(text)) {
    const remote = REPOSITORY.exec(target);
    if (remote) {
      if (!exists(decodeURIComponent(remote[1]))) problems.push(`${where(line)}: ${target}: no such file in this repository`);
      continue;
    }
    if (/^[a-z][a-z0-9+.-]*:/i.test(target) || target.startsWith('/')) continue;
    const [file, fragment] = target.split('#');
    const resolved = file ? posix.normalize(posix.join(directory, decodeURIComponent(file))) : path;
    if (resolved.startsWith('../')) { problems.push(`${where(line)}: ${target}: leaves the repository`); continue; }
    if (!exists(resolved)) { problems.push(`${where(line)}: ${target}: no such file`); continue; }
    if (fragment && resolved.endsWith('.md') && statSync(join(root, resolved)).isFile() && !anchors(readFileSync(join(root, resolved), 'utf8')).has(decodeURIComponent(fragment).toLowerCase())) {
      problems.push(`${where(line)}: ${target}: no heading #${fragment} in ${resolved}`);
    }
  }
  return problems;
}

/** Every problem in the repository at `root`. */
export function check(root) {
  const targets = makeTargets(readFileSync(join(root, 'Makefile'), 'utf8'));
  const scripts = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')).scripts || {};
  const problems = [];
  for (const path of FRONT_DOOR) {
    if (!existsSync(join(root, path))) { problems.push(`${path}: missing`); continue; }
    problems.push(...checkDocument(root, path, { targets, scripts }));
  }
  for (const path of COMMAND_PAGES) {
    if (!existsSync(join(root, path))) { problems.push(`${path}: missing`); continue; }
    problems.push(...checkDocument(root, path, { links: false, targets, scripts }));
  }
  return problems;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const root = resolve(process.argv[2] || join(dirname(fileURLToPath(import.meta.url)), '..'));
  const problems = check(root);
  if (problems.length) {
    console.error(`What the README and CONTRIBUTING tell people to run or open is missing in ${relative(process.cwd(), root) || '.'}:\n  ${problems.join('\n  ')}`);
    process.exitCode = 1;
  } else console.log(`✓ Every command and link in ${[...FRONT_DOOR, ...COMMAND_PAGES].join(', ')} exists.`);
}
