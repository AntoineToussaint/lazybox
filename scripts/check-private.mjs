#!/usr/bin/env node
// Keeps what must stay private out of the repository: every tracked file is
// checked for private names (an organization and its internal products) and
// for anything shaped like a secret, and must not be a compiled executable
// (a stray `go build`). CI runs it on every push; run it locally with
// `node scripts/check-private.mjs`. The names are stored base64-encoded so
// this file does not spell out what it hides.
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

/** Problems in the tracked files of the repository at `root`. */
export function checkTree(root, { patterns = PRIVATE_NAME_PATTERNS } = {}) {
  const files = execFileSync('git', ['ls-files', '-z'], { cwd: root, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 }).split('\0').filter(Boolean);
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

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const { files, problems } = checkTree(process.cwd());
  if (problems.length) {
    console.error(`Private names, secrets or executables in ${problems.length} place(s):\n  ` + problems.join('\n  '));
    process.exit(1);
  }
  console.log(`✓ No private names, secrets or executables in ${files} tracked files.`);
}
