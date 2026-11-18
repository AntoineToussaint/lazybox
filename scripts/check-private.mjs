#!/usr/bin/env node
// Every tracked file is checked for names that must stay private (an
// employer and its internal products) and for anything shaped like a secret.
// CI runs it on every push; run it locally with `node scripts/check-private.mjs`.
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';

const PRIVATE_NAME_PATTERNS = [/(?<![A-Za-z])(?:[Oo]bin|[O]BIN)(?![a-z])/, /(?<![A-Za-z])R[o]bin(?![a-z])/];
const SECRET_PATTERNS = [
  /gh[pousr]_[A-Za-z0-9]{36,}/, /github_pat_[A-Za-z0-9_]{60,}/, /sk-ant-[A-Za-z0-9_-]{40,}/, /sk-proj-[A-Za-z0-9_-]{40,}/,
  /AKIA[0-9A-Z]{16}/, /-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----/, /xox[baprs]-[A-Za-z0-9-]{20,}/,
];
const SKIP = new Set(['scripts/check-private.mjs']);

const files = execFileSync('git', ['ls-files', '-z'], { encoding: 'utf8' }).split('\0').filter(Boolean);
const problems = [];
for (const path of files) {
  if (SKIP.has(path)) continue;
  let data;
  try { data = readFileSync(path); } catch { continue; }
  if (data.includes(0)) continue; // binary
  const text = data.toString('utf8');
  text.split('\n').forEach((line, index) => {
    for (const pattern of PRIVATE_NAME_PATTERNS) if (pattern.test(line)) problems.push(`${path}:${index + 1}: names something private`);
  });
  for (const pattern of SECRET_PATTERNS) if (pattern.test(text)) problems.push(`${path}: looks like a secret (${pattern})`);
}
if (problems.length) {
  console.error(`Private names or secrets in ${problems.length} place(s):\n  ` + problems.join('\n  '));
  process.exit(1);
}
console.log(`✓ No private names or secrets in ${files.length} tracked files.`);
