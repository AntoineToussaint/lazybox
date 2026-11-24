// CHANGELOG.md is the one source of release notes (its header says how to
// write it). This reads it and produces the two things built from it:
//
//   node scripts/release/changelog.mjs --write        regenerate the app's What's new
//   node scripts/release/changelog.mjs --check        fail when the two disagree
//   node scripts/release/changelog.mjs notes 0.1.0    the GitHub release notes, Markdown
import { readFile, writeFile } from 'node:fs/promises';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
export const changelogPath = resolve(root, 'CHANGELOG.md');
export const releaseNotesPath = resolve(root, 'packages/surfaces/src/release-notes.ts');

// The install guide the GitHub release links to, on the repository's main branch.
export const installGuidePath = 'docs/install.md';
export const installGuide = `https://github.com/AntoineToussaint/lazybox/blob/main/${installGuidePath}`;

const heading = /^## \[([A-Za-z0-9][A-Za-z0-9.+-]*)\] - (\d{4}-\d{2}-\d{2}) · (.+)$/;
const groupHeading = /^### (.+)$/;
const item = /^- \*\*(.+?)\*\* (.+)$/;
export const versionPattern = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;

/** The releases in a changelog, newest first, in the app's Release shape. */
export function parseChangelog(text) {
  const releases = [];
  let current, last, group;
  const lines = text.split('\n');
  lines.forEach((line, index) => {
    const where = `CHANGELOG.md:${index + 1}`;
    if (line.startsWith('## ')) {
      const m = heading.exec(line);
      if (!m) throw new Error(`${where}: a section heading is "## [id] - YYYY-MM-DD · Title", got: ${line}`);
      if (releases.some(r => r.id === m[1])) throw new Error(`${where}: release ${m[1]} appears twice`);
      current = { id: m[1], date: m[2], title: m[3].trim(), items: [] };
      releases.push(current);
      last = group = undefined;
      return;
    }
    if (!current) return; // the header
    if (line.startsWith('### ')) {
      // A group of items inside a big section ("Workspaces and tabs"): the
      // items under it carry its name until the next group.
      group = groupHeading.exec(line)[1].trim();
      if (current.items.some(i => i.group === group)) throw new Error(`${where}: group "${group}" appears twice in ${current.id}`);
      last = undefined;
      return;
    }
    if (line.startsWith('- ')) {
      const m = item.exec(line);
      if (!m) throw new Error(`${where}: an item is "- **What** body", got: ${line}`);
      last = { title: m[1].trim(), body: m[2].trim(), ...(group ? { group } : {}) };
      current.items.push(last);
    } else if (line.startsWith('  ') && last && line.trim()) {
      last.body += ' ' + line.trim();
    } else if (line.trim()) {
      throw new Error(`${where}: only items belong in a section, got: ${line}`);
    }
  });
  for (const r of releases) {
    if (!r.items.length) throw new Error(`CHANGELOG.md: release ${r.id} has no items`);
    if (r.items.some(i => i.group) && !r.items.every(i => i.group)) throw new Error(`CHANGELOG.md: in ${r.id}, items before the first ### group belong in a group too`);
  }
  return releases;
}

const quote = s => `'${s.replaceAll('\\', '\\\\').replaceAll("'", "\\'")}'`;

/** packages/surfaces/src/release-notes.ts for these releases. */
export function renderReleaseNotes(releases) {
  const body = releases.map(r => [
    '  {',
    `    id: ${quote(r.id)},`,
    `    date: ${quote(r.date)},`,
    `    title: ${quote(r.title)},`,
    '    items: [',
    ...r.items.map(i => `      { ${i.group ? `group: ${quote(i.group)}, ` : ''}title: ${quote(i.title)}, body: ${quote(i.body)} },`),
    '    ],',
    '  },',
  ].join('\n')).join('\n');
  return `import type { Release } from './onboarding-model';

/**
 * The release notes bundled with the app, newest first. "What's new" shows
 * the releases newer than the last one this person saw (onboarding-model.ts).
 *
 * Generated from CHANGELOG.md by \`node scripts/release/changelog.mjs --write\`:
 * edit the changelog, not this file.
 */
export const releaseNotes: readonly Release[] = [
${body}
];
`;
}

/**
 * The GitHub release notes of a version: its section, then the dated
 * sections below it up to the previous version (notes Dev and Working got
 * between releases, which this release brings to everyone else).
 */
export function releaseBody(releases, version, { unsigned = false } = {}) {
  const start = releases.findIndex(r => r.id === version);
  if (start < 0) throw new Error(`CHANGELOG.md has no section for ${version}; add "## [${version}] - YYYY-MM-DD · Title" at the top`);
  const included = [releases[start]];
  for (const r of releases.slice(start + 1)) {
    if (versionPattern.test(r.id)) break;
    included.push(r);
  }
  const out = [];
  if (unsigned) {
    out.push(`> **Unsigned build.** This release is not signed with an Apple Developer ID or notarized, so macOS refuses to open it the first time. Open Lazybox once and choose **Done**, then **System Settings → Privacy & Security → Open Anyway**, and open it again (on macOS 15 and later, Control-click → Open no longer does this). Or run \`xattr -dr com.apple.quarantine /Applications/Lazybox.app\` in a terminal. See [Unsigned builds](${installGuide}#unsigned-builds).`, '');
  }
  included.forEach((r, i) => {
    const level = i === 0 ? '##' : '###';
    out.push(`${level} ${r.title}`, '');
    for (const [group, items] of groupsOf(r.items)) {
      if (group) out.push(`${level}# ${group}`, '');
      for (const it of items) out.push(`- **${it.title}.** ${it.body}`);
      out.push('');
    }
  });
  // No Homebrew line: the workflow publishes the cask only for a release
  // without a prerelease suffix, so the notes do not promise it.
  out.push(`Install: download the disk image below, open it and drag Lazybox to Applications. Already installed? Lazybox offers the update and installs it only when you choose to (Settings → About & updates). Full instructions: [the install guide](${installGuide}).`);
  return out.join('\n') + '\n';
}

/**
 * The dated sections above a version's own: notes that reached Dev and
 * Working after its section was written. Its release notes start at its
 * section, so they would leave these out; the build refuses until they are
 * folded in (or released as a later version, whose notes collect them).
 */
export function unfoldedAbove(releases, version) {
  const at = releases.findIndex(r => r.id === version);
  return at > 0 ? releases.slice(0, at).filter(r => !versionPattern.test(r.id)).map(r => r.id) : [];
}

/** A section's items in order, as [group, items] runs ('' when ungrouped). */
export function groupsOf(items) {
  const runs = [];
  for (const it of items) {
    const group = it.group || '';
    if (runs.at(-1)?.[0] !== group) runs.push([group, []]);
    runs.at(-1)[1].push(it);
  }
  return runs;
}

/** The plain-text notes the in-app updater shows (latest.json "notes"). */
export function updaterNotes(releases, version) {
  const r = releases.find(r => r.id === version);
  if (!r) return '';
  return [r.title, ...groupsOf(r.items).flatMap(([group, items]) => [...(group ? [group] : []), ...items.map(i => `• ${i.title}: ${i.body}`)])].join('\n');
}

/**
 * The sections an update feed carries: this version's and the ones before
 * it, newest first, at most `limit`. An installed app shows those newer than
 * its own version; one older than all of them is told the list is partial.
 */
export function updaterReleases(releases, version, limit = 25) {
  const start = releases.findIndex(r => r.id === version);
  if (start < 0) return [];
  return releases.slice(start, start + limit).map(({ id, date, title, items }) => ({ id, date, title, items: items.map(({ group, title, body }) => ({ ...(group ? { group } : {}), title, body })) }));
}

export async function readChangelog() {
  return parseChangelog(await readFile(changelogPath, 'utf8'));
}

async function main() {
  const [command, version] = process.argv.slice(2);
  const releases = await readChangelog();
  if (command === '--write') {
    await writeFile(releaseNotesPath, renderReleaseNotes(releases));
    console.log(`Wrote ${releaseNotesPath} (${releases.length} releases)`);
  } else if (command === '--check') {
    const current = await readFile(releaseNotesPath, 'utf8');
    if (current !== renderReleaseNotes(releases)) {
      console.error('release-notes.ts does not match CHANGELOG.md; run: node scripts/release/changelog.mjs --write');
      process.exitCode = 1;
    } else console.log('release-notes.ts matches CHANGELOG.md');
  } else if (command === 'notes' && version) {
    process.stdout.write(releaseBody(releases, version, { unsigned: process.argv.includes('--unsigned') }));
  } else {
    console.error('Use changelog.mjs --write | --check | notes <version> [--unsigned]');
    process.exitCode = 2;
  }
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main().catch(error => { console.error(error.message); process.exitCode = 1; });
