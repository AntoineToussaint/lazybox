#!/usr/bin/env node
// Keeps what must stay private out of the repository:
//   - every tracked file is checked for private names (an organization and
//     its internal products) and for anything shaped like a
//     secret, and must not be a compiled executable (a stray `go build`);
//   - with `--commits <range>`, the author, committer and Co-authored-by
//     addresses of the commits in that range are checked for the same names,
//     so a work address cannot land through a commit either.
//
//   node scripts/check-private.mjs                              the tree
//   node scripts/check-private.mjs --commits origin/main..HEAD  and a branch's commits
//
// CI runs it on every pull request with the commits the pull request adds,
// and on every push to main with the commits the push adds. The names are
// stored base64-encoded so this file does not spell out what it hides.
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const decode = source => new RegExp(Buffer.from(source, 'base64').toString('utf8'));
export const PRIVATE_NAME_PATTERNS = [
  'KD88IVtBLVphLXpdKSg/OltPb11iaW58T0JJTikoPyFbYS16XSk=',
  'KD88IVtBLVphLXpdKVJvYmluKD8hW2Etel0p',
].map(decode);
const SECRET_PATTERNS = [
  /gh[pousr]_[A-Za-z0-9]{36,}/, /github_pat_[A-Za-z0-9_]{60,}/, /sk-ant-[A-Za-z0-9_-]{40,}/, /sk-proj-[A-Za-z0-9_-]{40,}/,
  /AKIA[0-9A-Z]{16}/, /-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----/, /xox[baprs]-[A-Za-z0-9-]{20,}/,
];
/** The first bytes of a Mach-O (32/64-bit, either byte order) or ELF executable. */
const EXECUTABLE_MAGIC = ['feedface', 'feedfacf', 'cefaedfe', 'cffaedfe', '7f454c46'];

const git = (args, cwd) => execFileSync('git', args, { cwd, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 });

/** Problems in the tracked files of the repository at `root`. */
export function checkTree(root, { patterns = PRIVATE_NAME_PATTERNS } = {}) {
  const files = git(['ls-files', '-z'], root).split('\0').filter(Boolean);
  const problems = [];
  for (const path of files) {
    let data;
    try { data = readFileSync(resolve(root, path)); } catch { continue; }
    if (EXECUTABLE_MAGIC.includes(data.subarray(0, 4).toString('hex'))) { problems.push(`${path}: a compiled executable; build into bin/ (make build-go) and remove it with git rm --cached`); continue; }
    if (data.includes(0)) continue; // binary
    const text = data.toString('utf8');
    text.split('\n').forEach((line, index) => {
      for (const pattern of patterns) if (pattern.test(line)) problems.push(`${path}:${index + 1}: names something private`);
    });
    for (const pattern of SECRET_PATTERNS) if (pattern.test(text)) problems.push(`${path}: looks like a secret (${pattern})`);
  }
  return { files: files.length, problems };
}

/** Problems in the identities of the commits in `range` (`base..head`). */
export function checkCommits(root, range, { patterns = PRIVATE_NAME_PATTERNS } = {}) {
  const format = '%H%x1f%ae%x1f%ce%x1f%(trailers:key=Co-authored-by,valueonly,separator=%x1f)%x1e';
  const records = git(['log', '--no-color', `--format=${format}`, range, '--'], root).split('\x1e').map(record => record.trim()).filter(Boolean);
  const problems = [];
  for (const record of records) {
    const [sha, author, committer, ...coauthors] = record.split('\x1f');
    const named = [['author', author], ['committer', committer], ...coauthors.filter(Boolean).map(value => ['Co-authored-by', value])];
    for (const [role, value] of named) {
      if (patterns.some(pattern => pattern.test(value))) problems.push(`commit ${sha.slice(0, 12)}: its ${role} address names something private; set user.email (and amend the commit's author and committer) to an address you can publish`);
    }
  }
  return { commits: records.length, problems };
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const root = process.cwd();
  const at = process.argv.indexOf('--commits');
  const range = at > 0 ? process.argv[at + 1] : undefined;
  if (at > 0 && !range) { console.error('usage: check-private.mjs [--commits <base>..<head>]'); process.exit(2); }
  const tree = checkTree(root);
  const commits = range ? checkCommits(root, range) : undefined;
  const problems = [...tree.problems, ...(commits?.problems ?? [])];
  if (problems.length) {
    console.error(`Private names, secrets or executables in ${problems.length} place(s):\n  ` + problems.join('\n  '));
    process.exit(1);
  }
  console.log(`✓ No private names, secrets or executables in ${tree.files} tracked files${commits ? `, nor private addresses in ${commits.commits} commit(s) of ${range}` : ''}.`);
}
