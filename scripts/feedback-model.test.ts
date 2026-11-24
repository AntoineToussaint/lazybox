import { test } from 'node:test';
import assert from 'node:assert/strict';
import { feedbackReport, issueTitle, issueUrl, ISSUE_URL_LIMIT, redact, tailLines, type FeedbackInput } from '../packages/surfaces/src/feedback-model.ts';
// Fake credentials for the redaction tests, assembled so no source line looks like a real one (the secret scan in scripts/check-private.mjs).
const FAKE_AWS_KEY = 'AKIA' + 'IOSFODNN7EXAMPLE';
const FAKE_GITHUB_TOKEN = ['ghp', '0123456789abcdefghijABCDEFGHIJ012345'].join('_');
const FAKE_SLACK_TOKEN = ['xoxb', '123456789012-abcdefghijkl'].join('-');

const HOME = '/Users/ada';

test('credentials never survive redaction', () => {
  const cases: [string, string][] = [
    ['Authorization: Bearer abcdefghijklmnop123', 'abcdefghijklmnop123'],
    [`using ${FAKE_GITHUB_TOKEN}`, 'ghp_0123456789abcdefghij'],
    ['token github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz', 'github_pat_11ABCDEFG'],
    ['ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnopqrstuvwxyz', 'sk-ant-api03'],
    ['openai sk-proj-abcdefghijklmnopqrstuvwx', 'sk-proj-abcdef'],
    [`slack ${FAKE_SLACK_TOKEN}`, 'xoxb-1234'],
    [`aws ${FAKE_AWS_KEY}`, FAKE_AWS_KEY],
    ['jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N', 'dozjgNryP4J3jVmNHl0w5N'],
    ['{"token":"d3adb33f-runner-bearer"}', 'd3adb33f-runner-bearer'],
    ['LAZYBOX_TOKEN=s3cr3tvalue', 's3cr3tvalue'],
    ['password: hunter22', 'hunter22'],
    ['client_secret=topsecretvalue&x=1', 'topsecretvalue'],
    ['GET /cb?code=oauthcode123&state=xyz789', 'oauthcode123'],
    ['https://user:pa55word@github.com/org/repo.git', 'pa55word'],
  ];
  for (const [line, secret] of cases) {
    const out = redact(line);
    assert.ok(!out.includes(secret), `${line} -> ${out}`);
    assert.match(out, /\[redacted\]/, line);
  }
  // A count is not a credential: usage stays readable.
  assert.equal(redact('input_tokens=1200 output_tokens: 34'), 'input_tokens=1200 output_tokens: 34');
  // Ordinary log text is left alone.
  assert.equal(redact('runner: session abc started on port 43210'), 'runner: session abc started on port 43210');
});

test('the home directory reads ~ and nobody is named in paths or addresses', () => {
  assert.equal(redact(`opened ${HOME}/code/api/main.go`, { home: HOME }), 'opened ~/code/api/main.go');
  assert.equal(redact(`cwd=${HOME}`, { home: HOME }), 'cwd=~');
  assert.equal(redact(`${HOME}/`, { home: `${HOME}/` }), '~/', 'a trailing slash on home is fine');
  // Another home that merely starts the same is not this person's.
  assert.equal(redact('/Users/adalovelace/x', { home: HOME }), '/Users/[user]/x');
  // Without knowing home (a browser), any user's home is hidden; shared folders are not homes.
  assert.equal(redact('/Users/bob/Library/Logs/x.log'), '/Users/[user]/Library/Logs/x.log');
  assert.equal(redact('/Users/Shared/lazybox'), '/Users/Shared/lazybox');
  assert.equal(redact('/home/bob/.config'), '/home/[user]/.config');
  assert.equal(redact('signed in as ada@example.com'), 'signed in as [email]');
});

test('the log tail keeps the last lines only', () => {
  const log = Array.from({ length: 300 }, (_, i) => `line ${i + 1}`).join('\n') + '\n\n';
  const tail = tailLines(log);
  assert.equal(tail.split('\n').length, 200);
  assert.ok(tail.startsWith('line 101\n'));
  assert.ok(tail.endsWith('line 300'));
  assert.equal(tailLines('a\nb\nc', 2), 'b\nc');
});

const base: FeedbackInput = {
  what: 'The diff panel went blank after I pressed ⌘J.',
  app: { version: '0.2.0', channel: 'release' }, runnerVersion: '0.2.0', platform: 'macOS 15.1 (aarch64)',
  screenshot: false,
  diagnostics: { os: '15.1', arch: 'aarch64', home: HOME, runnerLog: { path: `${HOME}/Library/Logs/Lazybox/runner.log`, tail: `boot ok\nwrote ${HOME}/x\nAuthorization: Bearer abcdefghijklmnopqrstu\n` } },
  includeDiagnostics: true,
  conversation: { title: 'Fix the login bug in /Users/ada/app', agent: 'claude', model: 'opus', state: 'failed', error: 'token=abcdefgh1234 expired' },
  includeConversation: false,
};

test('the report is exactly what is shared, redacted, and names only what was ticked', () => {
  const report = feedbackReport(base);
  assert.match(report, /^# Lazybox feedback\n\n## What happened\n\nThe diff panel went blank/);
  assert.match(report, /## Versions\n\n- Lazybox 0\.2\.0 \(release\)\n- Runner 0\.2\.0\n\n## Diagnostics\n\n- macOS 15\.1 \(aarch64\)\n/);
  assert.match(report, /Runner log: last 3 lines of ~\/Library\/Logs\/Lazybox\/runner\.log/);
  assert.match(report, /```text\nboot ok\nwrote ~\/x\nAuthorization: Bearer \[redacted\]\n```/);
  assert.ok(!report.includes(HOME), 'no home path');
  assert.ok(!report.includes('abcdefghijklmnopqrstu'), 'no token');
  // Conversation content only when ticked, and never its messages.
  assert.ok(!report.includes('Fix the login bug'), 'the conversation is not named unless ticked');
  const named = feedbackReport({ ...base, includeConversation: true });
  assert.match(named, /## Conversation\n\n- Title: Fix the login bug in ~\/app\n- Agent: claude · opus\n- State: failed\n- Last error: token=\[redacted\] expired/);
  assert.match(named, /Its messages are not included/);
  // Diagnostics unticked: no OS line, no log.
  const bare = feedbackReport({ ...base, includeDiagnostics: false });
  assert.ok(!bare.includes('## Diagnostics') && !bare.includes('boot ok') && !bare.includes('macOS'));
  assert.match(feedbackReport({ ...base, screenshot: true }), /Screenshot: `screenshot\.png`/);
  assert.match(feedbackReport({ ...base, what: '  ' }), /_\(not filled in yet\)_/);
  assert.match(feedbackReport({ ...base, diagnostics: { os: '15.1', arch: 'aarch64', home: HOME } }), /Runner log: none available in this build/);
});

test('the GitHub issue is pre-filled, never posted, and fits in a link', () => {
  assert.equal(issueTitle('  Blank diff\nmore detail'), 'Feedback: Blank diff');
  assert.equal(issueTitle(''), 'Feedback: Lazybox');
  // The title is redacted like the body: it is part of what is shared.
  assert.equal(issueTitle(`Crash with ${FAKE_GITHUB_TOKEN} in ${HOME}/x`, HOME), 'Feedback: Crash with [redacted] in ~/x');
  assert.ok(issueTitle('x'.repeat(200)).length <= 'Feedback: '.length + 72);
  const report = feedbackReport(base);
  const url = new URL(issueUrl(report, base.what, { savedAt: '~/Downloads/Lazybox feedback 2026-12-02 09.00.00', home: HOME }));
  assert.equal(url.origin + url.pathname, 'https://github.com/AntoineToussaint/lazybox/issues/new');
  assert.equal(url.searchParams.get('title'), 'Feedback: The diff panel went blank after I pressed ⌘J.');
  assert.ok(url.searchParams.get('body')!.startsWith(report));
  assert.match(url.searchParams.get('body')!, /saved at `~\/Downloads\/Lazybox feedback 2026-12-02 09\.00\.00`: drag it here/);
  // A long log is cut to fit, and the cut closes its code block.
  const long = feedbackReport({ ...base, diagnostics: { ...base.diagnostics!, runnerLog: { path: 'runner.log', tail: Array.from({ length: 200 }, (_, i) => `line ${i} ${'x'.repeat(60)}`).join('\n') } } });
  const cut = issueUrl(long, base.what);
  assert.ok(cut.length <= ISSUE_URL_LIMIT, `${cut.length}`);
  const body = new URL(cut).searchParams.get('body')!;
  assert.match(body, /cut to fit a link/);
  assert.equal((body.match(/```/g) || []).length % 2, 0, 'code fences stay balanced');
  assert.match(body, /Save to file/);
});
