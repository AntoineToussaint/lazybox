// The docs screenshots, one entry per image: what each shows and how the fake
// Runner (scripts/fixtures/runner-ui.mjs) is set up for it. The driver,
// scripts/docs-screenshots.mjs, takes them; scripts/docs-images.test.ts checks
// that every image the docs reference is one of these and is on disk.
//
// Every shot is taken in the dark and the light theme (`name.webp`,
// `name-light.webp`); some also in a small window (`name-narrow.webp`,
// `name-narrow-light.webp`). The docs reference the dark one; Help →
// Documentation shows the light one in the light theme.
//
// The data is one story: a team ("acme") adding a response cache to its API
// and preparing a release, on a fixed day and time (CLOCK), so the pictures
// are the same on every run.
import { devices, expect } from '@playwright/test';
import { defaultModelPrices } from './fixtures/runner-ui.mjs';
import { seedDesignDoc } from './fixtures/artifact-sample.mjs';

/** The moment every screenshot is taken: Tuesday 17 November 2026, 15:42 UTC. */
export const CLOCK = Date.UTC(2026, 10, 17, 15, 42, 0);
export const TIMEZONE = 'UTC';
export const DESKTOP = { width: 1440, height: 900 };
/** A small laptop window: where the dock starts folding panels into tabs. */
export const NARROW = { width: 1024, height: 680 };
export const PHONE = { width: 390, height: 844 };

export const shots = {};
/**
 * shot(name, run, options): `run(context)` sets up the fake Runner and opens
 * the app; it returns the page to photograph, or `{ png }` for a picture it
 * composed itself. The context: `f` (the fake Runner), `open(path, { runners })`,
 * `clock.ago(ms)` (what the fake Runner records next happened that long before
 * the picture; `open` brings the clock back), `addRunner(id)`, `rpc(method, body)`,
 * `scheme`, `viewport`, `browser`, `origin`, `prepare(page)`, `compose(pngs, scheme)`.
 * Options: `narrow` (also in a small window).
 */
const shot = (name, run, options = {}) => { shots[name] = { run, ...options }; };

/** Every file a shot writes: `hero.webp`, `hero-light.webp`, `hero-narrow.webp`… */
export function shotOutputs(name) {
  const sizes = shots[name].narrow ? ['', '-narrow'] : [''];
  return sizes.flatMap(size => ['', '-light'].map(theme => `${name}${size}${theme}.webp`));
}
export const allOutputs = () => Object.keys(shots).flatMap(shotOutputs);

// ---- The story -------------------------------------------------------------

const b64 = value => Buffer.from(JSON.stringify(value)).toString('base64');
const ms = value => String(value);
const MIN = 60_000;
const HOME = '/Users/you/.config/lazybox';

/** A conversation's journal, written the way the Runner records it. */
function journal(f, sessionId, start = Date.now() - 12 * MIN, fields = {}) {
  const channel = f.openEvents(sessionId);
  let clock = start;
  const push = (event, seconds = 1) => channel.push({ ...fields, ...event }, new Date(clock += seconds * 1000).toISOString());
  const tool = (id, fields, seconds = 1) => push({ type: 'tool_activity', providerDataJson: b64({ type: 'tool_call', id, ...fields }) }, seconds);
  const done = (id, output = 'ok', seconds = 1) => tool(id, { status: 'completed', output }, seconds);
  return { channel, push, tool, done,
    prompt: (text, operationId) => push({ type: 'operation_accepted', text, operationId: operationId || `op-${channel.events.length + 1}` }),
    say: text => push({ type: 'message_delta', text }),
    end: () => push({ type: 'turn_ended' }),
    plan: (id, todos) => tool(id, { name: 'TodoWrite', status: 'completed', input: { todos }, output: 'ok' }) };
}

/**
 * The two workspaces as a team would have them: acme/api and acme/web on
 * feature branches, a pull request open on the cache work with its checks
 * green, a dev server per workspace that runs one at a time, and real names
 * for the conversations.
 */
function realistic(f) {
  f.collections[0].name = 'Response cache'; f.collections[1].name = 'Release 2.4';
  for (const [collection, folder] of [[f.collections[0], 'response-cache'], [f.collections[1], 'release-2-4']]) {
    collection.root = `/Users/you/Library/Application Support/Lazybox/workspaces/${folder}`;
    for (const member of collection.members) member.path = `${collection.root}/${member.spec.name}`;
    for (const session of f.sessions) if (session.collectionId === collection.id) session.workingDirectory = collection.root;
  }
  Object.assign(f.library[0], { name: 'api', gitIdentity: 'github:acme/api', sourceUrl: 'https://github.com/acme/api' });
  Object.assign(f.library[1], { name: 'web', gitIdentity: 'github:acme/web', sourceUrl: 'https://github.com/acme/web' });
  for (const [collection, branch, base] of [[f.collections[0], 'feature/response-cache', '7c41e09b'], [f.collections[1], 'release/2.4', '2d8f6a13']]) for (const member of collection.members) Object.assign(member, { baseCommit: base.padEnd(40, '0') }), member.spec.branch = branch;
  const api = f.shipState('alpha', 'api');
  Object.assign(api, { head: 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678', base: '7c41e09b'.padEnd(40, '0'), githubRepository: 'acme/api', upstream: 'origin/feature/response-cache',
    files: [{ path: 'src/cache.ts', indexStatus: ' ', worktreeStatus: 'M' }, { path: 'src/cache.test.ts', indexStatus: '?', worktreeStatus: '?' }],
    pr: { number: '42', title: 'Cache API responses for five minutes', body: '', url: 'https://github.com/acme/api/pull/42', state: 'open', draft: false, base: 'main', headSha: 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678', mergeableState: 'clean', reviewDecision: '', checksState: 'success', author: 'you',
      checks: ['test', 'lint', 'build'].map((name, i) => ({ name, status: 'completed', conclusion: 'success', url: `https://github.com/acme/api/actions/runs/41/job/${i + 1}` })) } });
  Object.assign(f.shipState('alpha', 'web'), { head: 'b2c3d4e5f60718293a4b5c6d7e8f901234567890', base: '7c41e09b'.padEnd(40, '0'), githubRepository: 'acme/web', upstream: 'origin/feature/response-cache' });
  Object.assign(f.shipState('beta', 'api'), { head: 'c3d4e5f60718293a4b5c6d7e8f9012345678901a', base: '2d8f6a13'.padEnd(40, '0'), githubRepository: 'acme/api', upstream: 'origin/release/2.4', ahead: 2, behindDefault: 3 });
  Object.assign(f.shipState('beta', 'web'), { head: 'd4e5f60718293a4b5c6d7e8f9012345678901a2b', base: '2d8f6a13'.padEnd(40, '0'), githubRepository: 'acme/web', upstream: 'origin/release/2.4' });
  // Each workspace's scripts approved, nothing running yet: no warning on the Run button.
  for (const id of ['alpha', 'beta']) {
    const runtime = f.runtimeFor(id), base = id === 'alpha' ? 20000 : 20010;
    runtime.ports = { base, count: 10 };
    for (const source of runtime.sources) Object.assign(source, { approval: 'approved', changed: [] });
    for (const setup of runtime.setups) if (setup.pendingApproval || setup.runState?.state !== 'succeeded') Object.assign(setup, { pendingApproval: false, missingFiles: [], runState: runtime.setups[0].runState });
    for (const action of runtime.actions) Object.assign(action, { blocked: '', runState: { state: 'idle' }, port: base + (action.name === 'storybook' ? 1 : 0), url: action.url ? `http://127.0.0.1:${base + (action.name === 'storybook' ? 1 : 0)}` : '' });
  }
  f.themeFolder.directory = `${HOME}/themes`;
  for (const [name, doc] of f.config.docs) Object.assign(doc, { path: `${HOME}/${name}.yaml`, schemaPath: `${HOME}/${doc.kind}.schema.json` });
  Object.assign(f.sessions[0], { title: 'Add a response cache', provider: 'claude', model: 'claude-opus-5', modelReported: true, instructionsPending: false, status: 'ready', busy: false, agents: mainAgent('done', Date.now() - 6 * MIN, { turnEndedAtUnixMillis: ms(Date.now() - 6 * MIN), lastTurnStatus: 'completed' }) });
  Object.assign(f.sessions[1], { title: 'Prepare the 2.4 release notes', provider: 'codex', profileId: 'codex-default', model: 'gpt-5.4', modelReported: true, instructionsPending: false });
}
const CACHE_DIFF = `diff --git a/src/cache.ts b/src/cache.ts
index 1111111..2222222 100644
--- a/src/cache.ts
+++ b/src/cache.ts
@@ -1,12 +1,18 @@
 import { createClient } from './db';

-const ttl = 60;
+const ttl = 300;
+const max = 1000;

 export function createCache() {
   const entries = new Map<string, Entry>();
   return {
     get(key: string) {
-      return entries.get(key)?.value;
+      const entry = entries.get(key);
+      if (!entry || entry.expires < Date.now()) return undefined;
+      return entry.value;
     },
     set(key: string, value: unknown) {
+      if (entries.size >= max) entries.delete(entries.keys().next().value);
       entries.set(key, { value, expires: Date.now() + ttl * 1000 });
     },
   };
`;
/** The cache work: a finished turn with reads, edits, a plan and passing tests. */
function cacheTranscript(f, { sessionId = 'alpha-session', finished = true } = {}) {
  const root = f.sessions[0].workingDirectory;
  const j = journal(f, sessionId);
  j.prompt('Add a cache in front of the API responses: entries expire after five minutes, at most 1,000 of them. Add tests.', 'op-1');
  j.say('I’ll read how responses are served today, then add the cache with an expiry and a size limit, and test both.');
  j.plan('plan-1', [{ content: 'Read the response path', status: 'in_progress' }, { content: 'Add expiry and a size limit', status: 'pending' }, { content: 'Test expiry and eviction', status: 'pending' }]);
  for (const [i, file] of ['src/cache.ts', 'src/db.ts', 'src/routes.ts', 'README.md'].entries()) j.tool(`read-${i}`, { name: 'Read', status: 'running', input: { file_path: `${root}/api/${file}` } });
  j.tool('grep-1', { name: 'Grep', status: 'running', input: { pattern: 'createCache', path: `${root}/api` } });
  for (const id of ['read-0', 'read-1', 'read-2', 'read-3', 'grep-1']) j.done(id, 'line one\nline two\n');
  j.plan('plan-2', [{ content: 'Read the response path', status: 'completed' }, { content: 'Add expiry and a size limit', status: 'in_progress' }, { content: 'Test expiry and eviction', status: 'pending' }]);
  j.tool('edit-1', { name: 'Edit', status: 'running', input: { file_path: `${root}/api/src/cache.ts`, old_string: 'const ttl = 60;', new_string: 'const ttl = 300;\nconst max = 1000;' } });
  j.done('edit-1');
  j.tool('edit-2', { name: 'Write', status: 'running', input: { file_path: `${root}/api/src/cache.test.ts`, content: 'test("entries expire")' } });
  j.done('edit-2');
  j.plan('plan-3', [{ content: 'Read the response path', status: 'completed' }, { content: 'Add expiry and a size limit', status: 'completed' }, { content: 'Test expiry and eviction', status: finished ? 'completed' : 'in_progress' }]);
  j.tool('test', { name: 'Bash', status: 'running', input: { command: 'npm test -- cache', description: 'Run the cache tests' } });
  if (finished) {
    j.done('test', '\u001b[32m✓ 6 tests passed\u001b[0m\n', 4);
    j.say('Done. `createCache` now drops entries after 300 s and evicts the oldest once it holds 1,000. Six tests cover expiry, eviction and a cache miss; they pass.');
    j.end();
  }
  return j;
}
function cacheChanges(f) {
  const api = f.shipState('alpha', 'api');
  api.files = [{ path: 'src/cache.ts', indexStatus: ' ', worktreeStatus: 'M' }, { path: 'src/cache.test.ts', indexStatus: '?', worktreeStatus: '?' }];
  f.diffs.set('alpha/api/src/cache.ts', CACHE_DIFF);
  return api;
}
const checkpointsOf = f => f.setCheckpoints('alpha-session', { turns: [{ operationId: 'op-1', prompt: 'Add a cache', files: [{ member: 'api', path: 'src/cache.ts', status: 'M', additions: 6, deletions: 1 }, { member: 'api', path: 'src/cache.test.ts', status: 'A', additions: 58 }] }] });

const mainAgent = (status, updated, extra = {}) => [{ id: 'root', name: 'Main agent', status, updatedAtUnixMillis: ms(updated), ...extra }];
const question = (id, text) => ({ type: 'question', requestId: id, providerDataJson: b64({ questions: [{ question: text }] }) });
const approval = (id, command, description) => ({ type: 'approval', requestId: id, text: 'Bash', providerDataJson: b64({ tool: 'Bash', toolUseId: `use-${id}`, input: { command, description }, always: [] }) });
/** Several agents across both workspaces, each in a different state. */
function fleet(f) {
  realistic(f);
  const now = Date.now(), base = { profileId: 'claude-default', provider: 'claude', capabilities: { files: true }, pending: [], instructionsPending: false, permission: 'ask', gatewayId: 'direct' };
  Object.assign(f.sessions[1], { status: 'waiting_input', agents: mainAgent('waiting_input', now - 5 * MIN), pending: [question('q-release', 'Which version should the notes start from?')] });
  f.sessions.push(
    { ...base, id: 'deps-session', collectionId: 'beta', workId: 'beta', workingDirectory: f.collections[1].root, title: 'Bump dependencies', model: 'claude-sonnet-5', modelReported: true, status: 'waiting_approval', createdAtUnixMillis: ms(now - 83 * MIN), agents: mainAgent('waiting_approval', now - 2 * MIN), pending: [approval('a-deps', 'npm update', 'Update the dependencies')] },
    { ...base, id: 'parser-session', collectionId: 'alpha', workId: 'alpha', workingDirectory: f.collections[0].root, title: 'Split the parser', model: 'claude-opus-5', modelReported: true, status: 'done', createdAtUnixMillis: ms(now - 60 * MIN), agents: mainAgent('done', now - 21 * MIN, { turnEndedAtUnixMillis: ms(now - 21 * MIN), lastTurnStatus: 'completed' }), lastMessage: 'I split the parser into three files.\n\n**All 42 tests pass.**' },
    { ...base, id: 'preview-session', collectionId: 'beta', workId: 'beta', workingDirectory: f.collections[1].root, title: 'Deploy the preview', model: 'claude-sonnet-5', modelReported: true, status: 'failed', fault: 'The preview deploy exited with code 1: the staging token expired.', createdAtUnixMillis: ms(now - 120 * MIN), agents: mainAgent('failed', now - 8 * MIN) },
  );
}
/** "Bump dependencies" asked to run npm update: its transcript up to the approval. */
function depsTranscript(f) {
  const deps = f.sessions.find(s => s.id === 'deps-session');
  const j = journal(f, deps.id, Date.now() - 4 * MIN);
  j.prompt('Bump the dependencies and run the tests.');
  j.say('Two packages have new minor versions: `undici` 7.4 → 7.6 and `zod` 4.1 → 4.3. I’ll update them, then run the tests.');
  j.push(deps.pending[0]);
  return deps;
}
const PLAN = `# Response cache

Cache API responses in memory, in front of the database.

## Decisions
- **Expiry:** 300 s, the longest any endpoint tolerates stale data.
- **Size:** at most 1,000 entries; the oldest is evicted first.
- **Scope:** \`GET\` routes only; anything with a session cookie bypasses it.

## Steps
1. Add expiry and a size limit to \`createCache\`.
2. Wire it into \`routes.ts\` for the read endpoints.
3. Test expiry, eviction and a miss.
4. Measure: p95 of \`/products\` before and after.
`;
const WORKSPACE = '/?view=workspace&collection=alpha&session=alpha-session';

// ---- The shots -------------------------------------------------------------

// README and Getting started: the whole window, Workspaces above Agents, a
// finished turn, and the plan the agent wrote beside it.
shot('hero', async ({ f, open, clock }) => {
  fleet(f); cacheTranscript(f); checkpointsOf(f);
  clock.ago(9 * MIN);
  f.artifacts.agentPut({ collectionId: 'alpha', sessionId: 'alpha-session', title: 'Response cache', kind: 'plan', content: PLAN });
  const page = await open(WORKSPACE);
  await expect(page.locator('.messages')).toContainText('Six tests cover');
  await showPanel(page, 'Artifacts');
  await page.getByRole('listbox', { name: 'Artifacts' }).getByRole('option').first().click();
  await expect(page.locator('.panel-artifacts')).toContainText('Decisions');
  await expect(page.locator('.panel-agents .agent-row').first()).toBeVisible();
  return page;
}, { narrow: true });

shot('appearance', async ({ f, open }) => {
  realistic(f); cacheTranscript(f);
  const page = await open(WORKSPACE);
  await openSettings(page, 'Appearance');
  await expect(page.getByRole('tabpanel', { name: 'Appearance' })).toBeVisible();
  return page;
});

shot('changes-review', async ({ f, open }) => {
  realistic(f); cacheTranscript(f); cacheChanges(f);
  const page = await open(WORKSPACE);
  await expect(page.locator('.messages')).toContainText('Six tests cover');
  const changes = await openChanges(page);
  await changes.getByRole('group', { name: 'Diff layout' }).getByRole('button', { name: 'Split' }).click();
  await comment(page, changes, 'Comment on line 4', 'Make the limit configurable, with 1000 as the default.');
  await comment(page, changes, 'Comment on line 10', 'An expired entry is still in the map: delete it here too.');
  await expect(changes.getByRole('complementary', { name: 'Review' })).toContainText('2 pending');
  return page;
});

// Another agent reviews: Codex's findings in the Review tray and in the diff.
shot('agent-review', async ({ f, open, rpc }) => {
  realistic(f); cacheTranscript(f); cacheChanges(f); checkpointsOf(f);
  await rpc('StartReview', { collectionId: 'alpha', sourceSessionId: 'alpha-session', scope: 'changes', provider: 'codex' });
  const run = f.reviewOf('alpha').runs[0];
  f.fileFinding(run.id, { path: 'src/cache.ts', startLine: 10, endLine: 12, snippet: ['      const entry = entries.get(key);', '      if (!entry || entry.expires < Date.now()) return undefined;', '      return entry.value;'], severity: 'major', title: 'An expired entry is never removed', body: 'get() returns undefined for an expired entry but leaves it in the map, so it still counts against the 1,000 limit and pushes out live entries. Delete it before returning.' });
  f.fileFinding(run.id, { path: 'src/cache.ts', startLine: 4, snippet: ['const max = 1000;'], severity: 'minor', title: 'Make the limit configurable', body: 'Read CACHE_MAX_ENTRIES, with 1000 as the default, as the handbook documents.' });
  f.fileFinding(run.id, { path: 'src/cache.ts', startLine: 15, snippet: ['      if (entries.size >= max) entries.delete(entries.keys().next().value);'], severity: 'nit', title: 'Name the eviction', body: 'A small evictOldest() reads better than the iterator chain.' });
  f.finishReview(run.id);
  Object.assign(f.sessions.find(s => s.id === run.reviewerSessionId), { status: 'ready', busy: false, model: 'gpt-5.4', modelReported: true });
  const page = await open(WORKSPACE);
  await expect(page.locator('.messages')).toContainText('Six tests cover');
  const changes = await openChanges(page);
  const tray = changes.getByRole('complementary', { name: 'Review' });
  await expect(tray.getByRole('button', { name: /^To triage/ })).toContainText('3');
  await expect(changes.getByRole('article', { name: /^Major finding/ })).toBeVisible();
  return page;
});

shot('chat-decisions', async ({ f, open }) => {
  realistic(f);
  const session = f.sessions[0];
  Object.assign(session, { busy: true, status: 'waiting_input', agents: [{ id: 'root', status: 'running' }] });
  const ask = { type: 'question', requestId: 'question-1', text: 'AskUserQuestion', providerDataJson: b64({ tool: 'AskUserQuestion', toolUseId: 'ask-1', input: { questions: [
    { question: 'Where should the cache keep its entries?', header: 'Storage', multiSelect: false, options: [{ label: 'In memory', description: 'Fastest; lost on restart' }, { label: 'SQLite', description: 'One file beside the API' }, { label: 'Redis', description: 'Shared by every API instance' }] },
  ] } }) };
  session.pending = [ask];
  const j = journal(f, 'alpha-session', Date.now() - 2 * MIN);
  j.prompt('Add a cache in front of the API responses, with tests.');
  j.say('Before I start, one choice only you can make.');
  j.push(ask);
  const page = await open(WORKSPACE);
  const card = page.locator('.request.question-card');
  await expect(card.getByRole('radiogroup', { name: 'Where should the cache keep its entries?' })).toBeVisible();
  await page.keyboard.press('1');
  await blur(page);
  return page;
});

shot('checkpoints', async ({ f, open }) => {
  realistic(f);
  const session = f.sessions[0];
  session.title = 'Parse the cache settings';
  const j = journal(f, 'alpha-session');
  const turn = (id, prompt, reply) => { j.push({ type: 'operation_accepted', operationId: id, text: prompt }); j.say(reply); j.end(); };
  turn('op-1', 'Add a parser for the cache settings file.', 'Added `parseSettings` in `settings.go`, with the defaults documented in the README.');
  turn('op-2', 'Add tests for it.', 'Added `settings.test.ts`: twelve cases, including a missing file and a bad TTL. They pass.');
  turn('op-3', 'Now document the settings.', '`docs/settings.md` now has one section per setting, with its default.');
  const state = f.setCheckpoints(session.id, { turns: [
    { operationId: 'op-1', prompt: 'Add a parser', files: [{ member: 'api', path: 'settings.go', status: 'A', additions: 40 }, { member: 'api', path: 'README.md', status: 'M', additions: 2, deletions: 1 }] },
    { operationId: 'op-2', prompt: 'Add tests for it', files: [{ member: 'web', path: 'settings.test.ts', status: 'A', additions: 12 }] },
    { operationId: 'op-3', prompt: 'Now document the settings', files: [{ member: 'api', path: 'docs/settings.md', status: 'A', additions: 18 }] },
  ] });
  state.commitsSince = { api: 1 };
  const page = await open(`/?view=workspace&collection=alpha&session=${session.id}`);
  const marker = page.locator('.messages').getByRole('group', { name: 'Turn 1 checkpoint' });
  await expect(marker).toContainText('Turn 1 · 2 files changed');
  await marker.getByLabel('Turn 1 actions', { exact: true }).click();
  await marker.getByRole('button', { name: 'Revert to before this turn…' }).click();
  await expect(page.getByRole('dialog', { name: 'Revert files to before turn 1' })).toContainText('files will change');
  return page;
});

shot('pr-loop', async ({ f, open }) => {
  realistic(f); cacheTranscript(f);
  const api = f.shipState('alpha', 'api');
  Object.assign(api, { ahead: 0, files: [],
    commits: [{ id: api.head, subject: 'Cache API responses for five minutes', committedAtUnix: String(Math.floor(Date.now() / 1000) - 600) }],
    committed: [{ path: 'src/cache.ts', indexStatus: 'M' }, { path: 'src/cache.test.ts', indexStatus: 'A' }] });
  Object.assign(api.pr, { mergeableState: 'unstable', reviewDecision: 'approved', checksState: 'failure',
    checks: [{ name: 'test', status: 'completed', conclusion: 'failure', url: 'https://github.com/acme/api/actions/runs/41/job/1', summary: '1 test failed: cache evicts the oldest entry' }, { name: 'lint', status: 'completed', conclusion: 'success', url: 'https://github.com/acme/api/actions/runs/41/job/2' }, { name: 'build', status: 'completed', conclusion: 'success', url: 'https://github.com/acme/api/actions/runs/40' }, { name: 'e2e', status: 'in_progress', conclusion: '', url: '' }] });
  f.diffs.set('alpha/api/src/cache.ts', CACHE_DIFF);
  const page = await open(WORKSPACE);
  await page.getByRole('button', { name: 'Review changes', exact: true }).click();
  const card = page.getByRole('region', { name: 'Workspace changes' }).getByRole('region', { name: 'Pull request #42' });
  await expect(card).toContainText('1 failing');
  return page;
});

// Run your app: the dev server on the workspace's own ports, and its actions.
shot('workspace-runtime', async ({ f, open, rpc }) => {
  realistic(f); cacheTranscript(f);
  await rpc('StartWorkspaceAction', { collectionId: 'alpha', action: 'api/dev' });
  f.runtimeFor('alpha').actions.find(a => a.key === 'api/dev').runState.startedAtUnixMillis = ms(Date.now() - 14 * MIN);
  const page = await open(WORKSPACE);
  const run = page.getByRole('group', { name: 'Run', exact: true });
  await expect(run.getByRole('button', { name: 'Stop dev', exact: true })).toBeEnabled();
  await run.getByLabel('More actions', { exact: true }).click();
  await page.getByRole('group', { name: 'More actions menu', exact: true }).getByRole('button', { name: 'Show actions and logs', exact: true }).click();
  const actions = page.getByRole('region', { name: 'Workspace actions', exact: true });
  await expect(actions.getByRole('listitem', { name: 'dev, Running' })).toBeVisible();
  await closePanel(page, 'Command output');
  await blur(page);
  return page;
});

// The console reads output as a log: a time gutter, sources with chips, levels, milestones.
const E = '\x1b';
const SERVICES_LOG = [
  '15:38:02 [codefly] starting 3 services in acme',
  '15:38:03 acme/api      | (INFO) Building go binary',
  `15:38:04 acme/web      > ${E}[1m${E}[35m▲ Next.js 15.0.3${E}[0m`,
  '15:38:04 acme/web      > - Experiments (use with caution):',
  '15:38:04 acme/web      >   · turbo',
  'db-1  | 2026-11-17T15:38:05Z LOG:  database system is ready to accept connections',
  '15:38:06 acme/api      | (INFO) cache: ttl=300s max=1000 entries',
  '15:38:06 acme/api      | (INFO) GET /healthz 200 in 2ms',
  '15:38:07 acme/api      | (INFO) GET /healthz 200 in 2ms',
  '15:38:08 acme/api      | (INFO) GET /healthz 200 in 2ms',
  '[codefly] still starting acme/api… 5s',
  '15:38:09 acme/api      | (WARN) GET /products 200 in 412ms (cache miss)',
  '15:38:09 acme/api      | (ERROR) cache warmup failed: src/cache.ts:31: connection refused',
  '    at warmup (src/cache.ts:31)',
  '15:38:10 acme/api      | (INFO) GET /products 200 in 3ms (cache hit)',
  '15:38:11 acme/web      > ✓ Ready in 2.1s',
  '15:38:11 acme/web      > - Local:        http://localhost:20003',
  '>> acme/web: Running on :20003',
  '>> acme/api: Running in 9.4s',
].join('\n') + '\n';
shot('console-log', async ({ f, open }) => {
  realistic(f); cacheTranscript(f);
  const run = f.streamCommand('alpha-session', 'codefly run', ['codefly', 'run']);
  run.write(SERVICES_LOG);
  const page = await open(WORKSPACE);
  await page.getByRole('tab', { name: 'Command output', exact: true }).click();
  const panel = page.locator('.command-panel');
  await expect(panel.locator('select')).toHaveValue(run.id);
  await expect(panel.getByTestId('console-output')).toContainText('Running in 9.4s');
  await expect(panel.getByRole('group', { name: 'Sources' })).toBeVisible();
  await resizeGroup(page, null, 'ArrowUp', 3); // the console's group is the active one: its tab was just clicked
  await blur(page);
  return page;
});

// The Web panel: a link from the chat, opened beside it (⌥-click).
const HANDBOOK = `<!doctype html><title>Response cache · API handbook</title>
<style>body{font:15px/1.6 -apple-system,system-ui,sans-serif;margin:0;color:#1f2328;background:#fff}header{padding:14px 28px;border-bottom:1px solid #d0d7de;font-weight:600}main{max-width:640px;padding:8px 28px}code{background:#f6f8fa;padding:1px 5px;border-radius:4px;font-size:13px}table{border-collapse:collapse;width:100%}td,th{border:1px solid #d0d7de;padding:6px 10px;text-align:left;font-size:14px}</style>
<header>Acme API handbook</header><main><h1>Response cache</h1><p>The API keeps <code>GET</code> responses in memory for a short time, so repeated reads do not reach the database.</p>
<h2>Settings</h2><table><tr><th>Variable</th><th>Default</th><th>Meaning</th></tr><tr><td><code>CACHE_TTL_SECONDS</code></td><td>300</td><td>How long an entry stays fresh</td></tr><tr><td><code>CACHE_MAX_ENTRIES</code></td><td>1000</td><td>Oldest entries are evicted beyond this</td></tr></table>
<h2>Bypass</h2><p>Requests with a session cookie are never cached.</p></main>`;
shot('web-panel', async ({ f, open, context }) => {
  realistic(f);
  await context.route('https://handbook.acme.dev/**', route => route.fulfill({ contentType: 'text/html; charset=utf-8', body: HANDBOOK }));
  const j = journal(f, 'alpha-session', Date.now() - MIN);
  j.prompt('Where are the cache settings documented?');
  j.say('In [the API handbook](https://handbook.acme.dev/api/cache): `CACHE_TTL_SECONDS` and `CACHE_MAX_ENTRIES`, with their defaults.');
  j.end();
  const page = await open(WORKSPACE);
  const link = page.locator('.panel-chat').getByRole('link', { name: 'the API handbook' });
  await link.click({ modifiers: ['Alt'] });
  const frame = page.locator('.panel-web').first().frameLocator('iframe');
  await expect(frame.getByRole('heading', { name: 'Response cache' })).toBeVisible();
  // The hint for sites that refuse frames, put away as a person does once they have read it.
  await page.getByRole('button', { name: 'Dismiss' }).click();
  return page;
});

/**
 * The orchestrated mission, over the last 40 minutes: "Cache lead" holds a
 * grant on "Ship the response cache" and started three agents. "Add the cache"
 * reported with a passing run and green checks; "Document the cache settings"
 * waits for an approval; "Benchmark /products" runs. Two touched the same file.
 */
function orchestrated(f, clock, { accept = false } = {}) {
  realistic(f);
  const lead = f.sessions[0];
  Object.assign(lead, { title: 'Cache lead', status: 'running', busy: true, agents: mainAgent('running', Date.now() - 20_000) });
  const { missions } = f;
  clock.ago(41 * MIN);
  const j = journal(f, lead.id, Date.now());
  j.prompt('Ship the response cache: entries expire after five minutes, at most 1,000. Split it between agents; I accept each task.');
  j.say('Three agents: one adds the cache, one benchmarks /products once it lands, one documents the settings.');
  const m = missions.create('Ship the response cache', 'Cache GET responses for five minutes; measure the effect on /products.');
  clock.ago(40 * MIN);
  missions.handle('UpdateMission', { missionId: m.id, grant: { sessionId: lead.id, ceiling: { maxChildren: 3, reviewLimit: 3, budgetUsd: 20 } } });
  clock.ago(39 * MIN);
  const add = missions.spawnChild(lead.id, { title: 'Add the cache', brief: 'Objective: expiry and a size limit in createCache\nDone when: npm test passes', role: 'worker' });
  clock.ago(38 * MIN);
  const bench = missions.spawnChild(lead.id, { title: 'Benchmark /products', brief: 'Objective: p95 before and after\nDone when: numbers in the PR', role: 'researcher', permission: 'read-only' });
  clock.ago(37 * MIN);
  const docs = missions.spawnChild(lead.id, { title: 'Document the cache settings', brief: 'Objective: one section per setting', role: 'writer' });
  clock.ago(35 * MIN);
  missions.agentMessage(lead.id, add.id, 'Keep the eviction in createCache; routes.ts only wires it.');
  clock.ago(33 * MIN);
  missions.agentMessage(lead.id, bench.id, 'Measure once the cache lands: 200 requests each way.');
  const tasks = missions.missions.find(v => v.id === m.id).tasks, task = title => tasks.find(t => t.title === title);
  clock.ago(31 * MIN);
  missions.addLease(m.id, { taskId: task('Add the cache').id, path: 'api/src/cache.ts', mode: 'exclusive', reason: 'rewriting the cache' });
  clock.ago(22 * MIN);
  missions.recordRun(add.id, { title: 'npm test', exitCode: 0 });
  clock.ago(19 * MIN);
  missions.addOverlap(m.id, { taskIds: [task('Add the cache').id, task('Document the cache settings').id], paths: ['api/README.md'] });
  clock.ago(14 * MIN);
  missions.reportChild(add.id, 'Expiry and eviction in createCache; six tests pass.');
  missions.setCost(task('Add the cache').id, 2.1);
  missions.setCost(task('Benchmark /products').id, 0.8);
  missions.setCost(task('Document the cache settings').id, 0.4);
  if (accept) { clock.ago(6 * MIN); missions.handle('UpdateMission', { missionId: m.id, accept: { taskId: task('Add the cache').id } }); }
  const grant = missions.missions.find(v => v.id === m.id).grants[0];
  missions.setBudget(grant.id, { spentUsd: 3.3, reservedUsd: 4 });
  Object.assign(add, { status: 'ready', busy: false });
  Object.assign(docs, { status: 'waiting_approval', busy: false, pending: [approval('a-docs', 'npm run docs:check', 'Check the docs build')] });
  Object.assign(bench, { status: 'running', busy: true });
  return { m, lead, add, bench, docs };
}

// Let an agent run agents: the orchestrator's conversation, and its tree in Agents.
shot('orchestrator', async ({ f, open, clock }) => {
  orchestrated(f, clock, { accept: true });
  const page = await open(WORKSPACE);
  const chat = page.locator('.panel-chat');
  await expect(chat.locator('.orchestrator-chip')).toBeVisible();
  await expect(page.locator('.panel-agents li[data-orchestrator]')).toBeVisible();
  await blur(page);
  return page;
});

shot('orchestrator-plan', async ({ f, open }) => {
  fleet(f);
  const lead = f.sessions[0];
  lead.title = 'Cache lead';
  const j = journal(f, lead.id, Date.now() - 3 * MIN);
  j.prompt('Ship the response cache. Propose how you would split it first.');
  j.say('Here is how I would split it: the cache first, then the benchmark; the docs alongside.');
  const m = f.missions.create('Ship the response cache');
  f.missions.handle('UpdateMission', { missionId: m.id, grant: { sessionId: lead.id, ceiling: { maxChildren: 3, planRequired: true } } });
  f.missions.proposePlan(lead.id, { summary: 'the cache first, then the benchmark; docs alongside', tasks: [
    { key: 'cache', title: 'Add the cache', brief: 'Objective: expiry and a size limit in createCache\nDone when: npm test passes', role: 'worker', workspace: 'new', paths: ['api/src/cache.ts', 'api/README.md'], estimateUsd: 4 },
    { key: 'bench', title: 'Benchmark /products', role: 'researcher', permission: 'read-only', paths: ['api/bench/'], estimateUsd: 2.5, after: ['cache'] },
    { key: 'docs', title: 'Document the cache settings', role: 'writer', paths: ['api/README.md', 'docs/cache.md'], estimateUsd: 1.5 },
  ] });
  const page = await open(WORKSPACE);
  const line = page.locator('.panel-chat .lineage-line.kind-plan_proposed');
  await line.getByRole('button', { name: 'Review plan…' }).click();
  await expect(page.getByRole('dialog', { name: 'Plan for Ship the response cache' })).toBeVisible();
  return page;
});

/** ⌘K → Open the missions board: the board as a page, on a tab. */
async function missionsBoard(page, tab) {
  await page.locator('.panel-chat').getByRole('textbox', { name: 'Message', exact: true }).focus();
  await page.keyboard.press('ControlOrMeta+k');
  await page.keyboard.type('missions board');
  await page.getByRole('option', { name: /Open the missions board/ }).click();
  const panel = page.getByRole('region', { name: 'Missions and tasks', exact: true });
  await expect(panel).toBeVisible();
  if (tab) await panel.getByRole('tablist', { name: 'Mission views' }).getByRole('tab', { name: tab }).click();
  await blur(page);
  return panel;
}

shot('missions', async ({ f, open, clock }) => {
  const { m } = orchestrated(f, clock);
  // Work of the person's own beside the orchestrator's: one accepted on evidence, one not started.
  clock.ago(90 * MIN);
  const read = f.missions.addTask(m, 'Read the response path', 'Objective: know every read endpoint\nDone when: listed in the plan');
  const reader = { ...f.sessions[0], id: 'reader-session', title: 'Read the response path', status: 'done', busy: false, pending: [], lineage: undefined, agents: mainAgent('done', Date.now(), { lastTurnStatus: 'completed' }) };
  f.sessions.push(reader);
  f.missions.assign(m, read, reader, false);
  clock.ago(70 * MIN);
  f.missions.recordRun(reader.id, { title: 'npm test', exitCode: 0 });
  f.missions.handle('UpdateMission', { missionId: m.id, report: { taskId: read.id, summary: 'Six read endpoints, listed in the plan.' } });
  clock.ago(65 * MIN);
  f.missions.handle('UpdateMission', { missionId: m.id, accept: { taskId: read.id } });
  f.missions.addTask(m, 'Remove the old memo layer', 'Objective: one cache, not two\nDone when: memo.ts is gone and tests pass');
  const page = await open(WORKSPACE);
  const panel = await missionsBoard(page);
  await expect(panel.locator('.mission-orchestrator')).toContainText('Cache lead');
  await expect(panel.locator('.flag-overlap').first()).toBeVisible();
  return page;
});

shot('missions-timeline', async ({ f, open, clock }) => {
  orchestrated(f, clock, { accept: true });
  const page = await open(WORKSPACE);
  const panel = await missionsBoard(page, /Timeline/);
  await expect(panel.getByRole('table', { name: 'Mission timeline' }).locator('.timeline-row').first()).toBeVisible();
  return page;
});

// Concepts → The inbox: the Inbox button goes to the Agents list, on what needs
// you first; the approval is answered right there in the conversation.
shot('inbox', async ({ f, open }) => {
  fleet(f); cacheTranscript(f); depsTranscript(f);
  f.sessions.find(s => s.id === 'parser-session').pinnedAtUnixMillis = ms(Date.now() - 50 * MIN);
  const page = await open('/?view=workspace&collection=beta&session=deps-session');
  await expect(page.locator('.panel-chat .request')).toContainText('npm update');
  await expect(page.locator('.inbox-action')).toHaveAccessibleName(/need/);
  await page.locator('.inbox-action').click();
  await expect(page.locator('.panel-agents :focus')).toHaveCount(1);
  return page;
});

shot('command-palette', async ({ f, open }) => {
  fleet(f); cacheTranscript(f);
  const page = await open(WORKSPACE);
  await expect(page.locator('.messages')).toContainText('Six tests cover');
  await page.locator('[data-agent-composer]').focus();
  await page.keyboard.press('ControlOrMeta+k');
  const palette = page.getByRole('dialog', { name: 'Command palette' });
  await palette.getByRole('combobox', { name: 'Search commands' }).fill('review');
  await expect(palette.getByRole('option').first()).toBeVisible();
  return page;
});

// ⌘N: a task in one step — repositories, agent, mode, the first message.
shot('new-task', async ({ f, open }) => {
  realistic(f); cacheTranscript(f);
  f.completeGitHub();
  Object.assign(f.github, { login: 'you' });
  f.githubRepositories.splice(0, f.githubRepositories.length,
    ...[['api', true], ['web', false], ['infra', true], ['docs', false]].map(([name, isPrivate], i) => ({ id: String(500 + i), name, fullName: `acme/${name}`, private: isPrivate, defaultBranch: 'main', htmlUrl: `https://github.com/acme/${name}` })));
  const page = await open(WORKSPACE);
  await expect(page.locator('.messages')).toContainText('Six tests cover');
  await page.locator('[data-agent-composer]').focus();
  await page.keyboard.press('ControlOrMeta+n');
  const sheet = page.getByRole('dialog', { name: 'New task' });
  await expect(sheet).toBeVisible();
  const message = sheet.getByRole('textbox', { name: 'First message' });
  await message.fill('Load-test the cache. Run 200 requests against /products with it on and off, and put p50 and p95 in PR #42.');
  const search = sheet.getByRole('combobox', { name: 'Add a repository' });
  await search.focus();
  await search.fill('acme/api');
  await expect(sheet.getByRole('listbox', { name: 'Repositories' }).getByRole('option').first()).toContainText('acme/api');
  await page.keyboard.press('Enter');
  await search.fill('');
  await expect(sheet.locator('.new-task-chip')).toHaveCount(1);
  await message.focus();
  return page;
});

shot('shortcuts', async ({ f, open }) => {
  fleet(f); cacheTranscript(f);
  const page = await open(WORKSPACE);
  await page.locator('.workspace-brand').click();
  await page.keyboard.press('?');
  await expect(page.getByRole('dialog', { name: 'Keyboard shortcuts' })).toBeVisible();
  return page;
});

shot('subagents', async ({ f, open }) => {
  realistic(f);
  const now = Date.now(), at = offset => new Date(now + offset).toISOString();
  const A = 'claude-tool:toolu_explore', B = 'claude-tool:toolu_audit', D = 'claude-tool:toolu_bench';
  Object.assign(f.sessions[0], { title: 'Find what slows /products', busy: true, status: 'running', capabilities: { ...f.sessions[0].capabilities, stopAgent: true },
    agents: [{ id: 'root', status: 'running', phase: 'thinking', phaseSinceUnixMillis: String(now - 4000), turnStartedAtUnixMillis: String(now - 300000) },
      ...[[A, 'running'], [B, 'running'], [D, 'done']].map(([id, status]) => ({ id, parentId: 'root', status, name: 'Sub-agent', kind: 'local_agent', nativeId: 'task-' + id }))] });
  const ch = f.openEvents('alpha-session');
  const push = (offset, event) => ch.push(event.data === undefined ? event : { ...Object.fromEntries(Object.entries(event).filter(([k]) => k !== 'data')), providerDataJson: b64(event.data) }, at(offset));
  const call = (id, name, status, extra = {}) => ({ type: 'tool_call', id, name, status, ...extra });
  const spawn = (offset, id, toolUse, description, type, prompt) => {
    push(offset, { type: 'tool_activity', data: call(toolUse, 'Task', 'running', { input: { description, prompt, subagent_type: type } }) });
    push(offset + 200, { type: 'agent_status', agentId: id, parentAgentId: 'root', nativeId: 'task-' + toolUse, status: 'running', name: description, agentKind: 'local_agent', data: { toolUseId: toolUse, subagentType: type, prompt } });
  };
  push(-300000, { type: 'operation_accepted', operationId: 'op-1', text: '/products got slow this week. Find out why, in parallel where you can.' });
  push(-299000, { type: 'turn_started' });
  push(-298000, { type: 'message_delta', text: 'I’ll split this three ways: one agent reads the query path, one audits this week’s commits, and one benchmarks the endpoint.' });
  spawn(-290000, D, 'toolu_bench', 'Benchmark /products', 'general-purpose', 'Measure p50 and p95 of GET /products, 200 requests.');
  for (let i = 0; i < 12; i++) { push(-289000 + i * 500, { type: 'tool_activity', agentId: D, data: call('d' + i, 'Bash', 'running', { input: { command: `curl -s -o /dev/null -w '%{time_total}' localhost:20000/products` } }) }); push(-288800 + i * 500, { type: 'tool_activity', agentId: D, data: call('d' + i, undefined, 'completed', { output: '0.41' }) }); }
  push(-200000, { type: 'tool_activity', data: { type: 'tool_call', id: 'toolu_bench', status: 'completed', output: [{ type: 'text', text: 'p50 410 ms, p95 1.9 s (last week: p95 220 ms).' }] } });
  push(-199990, { type: 'agent_status', agentId: D, status: 'done', data: { toolUseId: 'toolu_bench', summary: 'p95 went from 220 ms to 1.9 s', usage: { totalTokens: 21000, toolUses: 12, durationMs: 90000 } } });
  spawn(-95000, A, 'toolu_explore', 'Trace the query path', 'Explore', 'Trace GET /products from the route to the database.');
  push(-90000, { type: 'tool_activity', agentId: A, data: call('a1', 'Grep', 'running', { input: { pattern: 'listProducts', path: 'api/src' } }) });
  push(-89000, { type: 'tool_activity', agentId: A, data: call('a1', undefined, 'completed', { output: 'routes.ts:31: router.get("/products", listProducts)' }) });
  push(-88000, { type: 'message_delta', agentId: A, parentAgentId: 'root', text: 'listProducts now loads every variant per product: one query each.' });
  push(-8000, { type: 'tool_activity', agentId: A, data: call('a2', 'Read', 'running', { input: { file_path: 'api/src/products.ts' } }) });
  push(-5000, { type: 'agent_status', agentId: A, status: 'running', data: { toolUseId: 'toolu_explore', usage: { totalTokens: 48200, toolUses: 14, durationMs: 90000 }, summary: 'Reading products.ts' } });
  spawn(-60000, B, 'toolu_audit', 'Audit this week’s commits', 'general-purpose', 'List this week’s commits that touch the products query.');
  push(-50000, { type: 'tool_activity', agentId: B, data: call('b1', 'Bash', 'running', { input: { command: 'git log --since=7.days --oneline -- api/src/products.ts', description: 'List recent commits' } }) });
  push(-4000, { type: 'activity', status: 'thinking' });
  const page = await open(WORKSPACE);
  await expect(page.locator('.messages')).toContainText('Trace the query path');
  return page;
});

shot('activity-detail', async ({ f, open }) => {
  realistic(f);
  const session = f.sessions[0];
  Object.assign(session, { title: 'Why can’t the owner read the runtime?', busy: true, status: 'running', agents: [{ id: 'root', status: 'running' }] });
  const root = session.workingDirectory;
  const j = journal(f, 'alpha-session', Date.now() - 45_000, { turnId: 't1' });
  const tool = (id, fields, seconds = 0) => j.push({ type: 'tool_activity', turnId: 't1', providerDataJson: b64({ type: 'tool_call', id, ...fields }) }, seconds);
  j.prompt('Why can’t the owner read their own runtime?');
  j.say('I’ll trace how the runtime resolves access.');
  ['runtime_access.go', 'policy.go', 'owner.go', 'session.go', 'grants.go', 'store.go', 'handler.go', 'errors.go'].forEach((file, i) => {
    j.push({ type: 'activity', status: 'thinking' }, 2);
    tool(`sh-${i}`, { name: 'Bash', status: 'running', input: { command: `sed -n '1,200p' internal/runtime/${file}`, ...(i === 0 ? { description: 'Read the access check' } : {}) } }, 1.4);
    tool(`sh-${i}`, { status: 'completed', output: `package runtime\n// ${file}\n` }, 0.1);
  });
  for (const [i, file] of ['internal/runtime/policy_test.go', 'README.md'].entries()) { tool(`read-${i}`, { name: 'Read', status: 'running', input: { file_path: `${root}/${file}` } }, 1); tool(`read-${i}`, { status: 'completed', output: 'a\nb\n' }, 0.1); }
  tool('fail', { name: 'Bash', status: 'running', input: { command: 'npm test', description: 'Run the unit tests' } }, 1);
  tool('fail', { status: 'failed', output: [{ type: 'text', text: 'Exit code 1\nFAIL src/access.test.ts › owner can read' }] }, 3);
  tool('edit', { name: 'Edit', status: 'running', input: { file_path: `${root}/internal/runtime/runtime_access.go`, old_string: 'return false', new_string: 'return owner == caller' } }, 1);
  tool('edit', { status: 'completed', output: 'ok' }, 0.1);
  j.say('Found it: the owner check compared the wrong ids. Running the tests now.');
  tool('diff', { name: 'Bash', status: 'running', input: { command: 'git diff --stat' } }, 1); tool('diff', { status: 'completed', output: ' 1 file changed' }, 0.1);
  tool('gotest', { name: 'Bash', status: 'running', input: { command: 'go test ./internal/runtime/... -count 1', description: 'Run the runtime tests' } }, 1);
  const page = await open(WORKSPACE);
  const first = page.locator('.messages').first().locator('.tool-lines').first();
  await expect(first).toContainText('Explored the codebase');
  return page;
});

// The artifact reader: a design doc an agent wrote and revised, read like a document.
shot('artifacts', async ({ f, open, clock }) => {
  realistic(f);
  f.collections[0].name = 'Billing datasource';
  f.sessions[0].title = 'Plan the datasource migration';
  clock.ago(58 * MIN);
  f.artifacts.agentPut({ collectionId: 'alpha', sessionId: 'alpha-session', title: 'Billing read paths', kind: 'report', content: '# Billing read paths\n\nEvery query billing makes against `accounts`, by endpoint.\n' });
  clock.ago(41 * MIN);
  seedDesignDoc(f, { sessionId: 'alpha-session' });
  clock.ago(25 * MIN);
  f.artifacts.agentPut({ collectionId: 'alpha', sessionId: 'alpha-session', title: 'Review of the migration plan', kind: 'report', content: '# Review of the migration plan\n\nNo blockers. Two notes on the rollback of phase 3.\n' });
  const page = await open(WORKSPACE);
  await showPanel(page, 'Artifacts');
  await page.getByRole('listbox', { name: 'Artifacts' }).getByRole('option', { name: /Datasource migration/ }).click();
  await expect(page.getByText('Move reads to the new datasource first').first()).toBeVisible();
  await page.getByRole('button', { name: 'Maximize Artifacts' }).click();
  return page;
});

shot('devices-pairing', async ({ f, open, context }) => {
  realistic(f); cacheTranscript(f);
  f.remote.tailscale = { ...f.remote.tailscale, enabled: true, state: 'serving' };
  f.remote.phoneUrl = 'https://laptop.example.ts.net:8443';
  // The same pairing code on every run, so the QR code is the same picture.
  await context.route('**/BeginDevicePairing', async route => {
    const response = await route.fetch(), body = await response.json();
    const offer = f.remote.offers.get(body.id), [id, secret] = ['5f0c2a9e41d84b7f9c3e6a1d2b7f8e40', '9d3e7a1c5b2f4e8a0c6d1b9f3a7e5c2d8b4f0a6e2c9d1b7f5a3e8c0d4b6f2a1e'];
    f.remote.offers.delete(offer.id); Object.assign(offer, { id, secret }); f.remote.offers.set(id, offer);
    await route.fulfill({ response, json: { ...body, id, url: `${f.remote.phoneUrl}/?view=remote#${new URLSearchParams({ pair: `${id}.${secret}`, grants: offer.grants.join(',') })}` } });
  });
  const page = await open(WORKSPACE);
  await openSettings(page, 'Devices');
  await page.getByRole('tabpanel', { name: 'Devices' }).getByRole('button', { name: 'Pair a phone' }).click();
  await expect(page.getByRole('dialog', { name: 'Pair a phone' }).getByRole('img', { name: /Pairing code/ })).toBeVisible();
  await blur(page);
  return page;
});

// The phone: its Inbox and a conversation waiting for an approval, side by side.
shot('phone-inbox', async ({ f, browser, scheme, origin, prepare, compose, rpc }) => {
  fleet(f); cacheTranscript(f); depsTranscript(f);
  f.remote.tailscale = { ...f.remote.tailscale, enabled: true, state: 'serving' };
  f.remote.phoneUrl = origin;
  const pairing = await rpc('BeginDevicePairing', { grants: ['respond'] });
  const context = await browser.newContext({ ...devices['iPhone 13'], viewport: PHONE, isMobile: false, deviceScaleFactor: 2, reducedMotion: 'reduce', colorScheme: scheme, timezoneId: TIMEZONE, locale: 'en-US' });
  try {
    const page = await context.newPage(); page.setDefaultTimeout(15000);
    await prepare(page);
    await page.goto(pairing.url);
    await page.getByRole('textbox', { name: 'Name this phone' }).fill('iPhone');
    await page.getByRole('button', { name: 'Pair this phone' }).click();
    await page.getByRole('button', { name: 'Open inbox' }).click();
    await expect(page.getByRole('heading', { name: 'Inbox' })).toBeVisible();
    await expect(page.locator('.remote-row').first()).toBeVisible();
    // The phone asks once whether to turn on notifications: answered "Not now" for the picture.
    await page.getByRole('complementary', { name: 'Notifications' }).getByRole('button', { name: /Not now|Dismiss|Later/ }).click().catch(() => {});
    await settle(page);
    const inbox = await page.screenshot({ type: 'png' });
    await page.locator('.remote-row').filter({ hasText: 'Bump dependencies' }).locator('.remote-row-open').click();
    await expect(page.locator('.request').filter({ hasText: 'npm update' })).toBeVisible();
    await settle(page);
    const conversation = await page.screenshot({ type: 'png' });
    return { png: await compose([inbox, conversation], scheme) };
  } finally { await context.close(); }
});

shot('subscriptions', async ({ f, open }) => {
  realistic(f); cacheTranscript(f);
  f.runnerSettings.prices = defaultModelPrices();
  Object.assign(f.profiles.find(p => p.id === 'codex-saved'), { label: 'Team Codex' });
  f.profiles.find(p => p.id === 'codex-saved').availability.email = 'team@example.com';
  const now = Math.floor(Date.now() / 1000);
  f.profiles.push({ id: 'claude-work', provider: 'claude', label: 'Work Claude', managed: true, isolation: 'environment', availability: { state: 'exhausted', plan: 'max', email: 'work@example.com', resetAt: String(now + 7800), observedAtUnixMillis: String(Date.now()), windows: [{ name: 'five_hour', durationMinutes: '300', usedPercent: 100, resetAt: String(now + 7800) }, { name: 'seven_day', durationMinutes: '10080', usedPercent: 64, resetAt: String(now + 300000) }] } });
  f.profiles.push({ id: 'claude-personal', provider: 'claude', label: 'Personal Claude', managed: true, isolation: 'environment', availability: { state: 'available', plan: 'pro', email: 'me@example.com', observedAtUnixMillis: String(Date.now()), windows: [{ name: 'five_hour', durationMinutes: '300', usedPercent: 22, resetAt: String(now + 9000) }, { name: 'seven_day', durationMinutes: '10080', usedPercent: 41, resetAt: String(now + 400000) }] } });
  const page = await open(WORKSPACE);
  await openSettings(page, 'Subscriptions');
  await expect(page.getByRole('tabpanel', { name: 'Subscriptions' })).toContainText('Work Claude');
  return page;
});

shot('processes', async ({ f, open }) => {
  realistic(f); cacheTranscript(f);
  Object.assign(f.processes[0], { label: 'Add a response cache', conversationTitle: 'Add a response cache', workspaceName: 'Response cache' });
  Object.assign(f.processes[1], { conversationTitle: 'Prepare the 2.4 release notes', workspaceName: 'Release 2.4' });
  Object.assign(f.processes[2], { label: 'codex · an old checkout' });
  const page = await open(WORKSPACE);
  await showPanel(page, 'Processes');
  await expect(page.getByRole('region', { name: 'Hosted processes', exact: true })).toContainText('go test');
  await blur(page);
  return page;
});

shot('welcome', async ({ f, open }) => {
  f.collections.splice(0); f.sessions.splice(0);
  const page = await open('/?view=workspace');
  await expect(page.getByRole('dialog', { name: 'Welcome to Lazybox' })).toBeVisible();
  await blur(page);
  return page;
});

shot('dock-panels', async ({ f, open }) => {
  realistic(f); cacheTranscript(f);
  const page = await open(WORKSPACE);
  await expect(page.locator('.messages')).toContainText('Six tests cover');
  await showPanel(page, 'Files');
  await page.getByLabel('Files panel actions', { exact: true }).click();
  await expect(page.getByRole('button', { name: 'Dock left', exact: true })).toBeVisible();
  await blur(page);
  return page;
});

shot('new-workspace', async ({ f, open }) => {
  realistic(f); cacheTranscript(f);
  Object.assign(f.github, { state: 'connected', authMethod: 'personal_token', login: 'you' });
  f.githubRepositories.splice(0, f.githubRepositories.length,
    ...[['api', true], ['web', false], ['infra', true], ['mobile', true], ['design-system', false], ['docs', false]].map(([name, isPrivate], i) => ({ id: String(500 + i), name, fullName: `acme/${name}`, private: isPrivate, defaultBranch: 'main', htmlUrl: `https://github.com/acme/${name}` })));
  const page = await open(WORKSPACE);
  await page.getByRole('button', { name: 'New workspace', exact: true }).click();
  const dialog = page.getByRole('dialog', { name: 'New workspace', exact: true });
  const search = dialog.getByRole('searchbox', { name: 'Find repositories' });
  await search.fill('acme/api');
  await dialog.getByRole('checkbox', { name: 'acme/api', exact: true }).check();
  await search.fill('acme/web');
  await dialog.getByRole('checkbox', { name: 'acme/web', exact: true }).check();
  await search.fill('acme');
  await expect(dialog.getByRole('checkbox', { name: 'acme/infra', exact: true })).toBeVisible();
  return page;
});

shot('actions-hooks', async ({ f, open }) => {
  realistic(f); cacheTranscript(f);
  const step = s => ({ when: 'always', ...s });
  const hook = h => ({ description: '', workspaces: [], conversations: [], repositories: [], run: [], shell: '', webhook: '', headers: {}, grants: [], timeoutSeconds: 60, enabled: true, ...h });
  f.power.setFile({
    actions: [
      { id: 'test-then-fix', title: 'Run the tests, ask the agent to fix failures', icon: 'play', key: 't', where: ['palette', 'menu', 'conversation'], confirm: false, steps: [step({ kind: 'run', action: 'api/test' }), step({ kind: 'send', text: 'The tests failed. Read the output in the console and fix them.', to: 'current', when: 'failed' }), step({ kind: 'open', panel: 'changes', when: 'succeeded' })] },
      { id: 'nudge', title: 'Tell the agent to continue', icon: 'send', key: 'g', where: ['palette', 'menu', 'conversation'], confirm: false, steps: [step({ kind: 'send', text: 'Continue.', to: 'current' })] },
      { id: 'unattended', title: 'Run unattended', icon: 'shield', key: '', where: ['palette'], confirm: true, description: 'No approvals until you change it back.', steps: [step({ kind: 'permission', permission: 'yolo' })] },
    ],
    hooks: [hook({ id: 'notify', on: ['agent.needs-input'], run: ['terminal-notifier', '-title', 'Lazybox', '-message', 'needs you'] }), hook({ id: 'ci-failed', on: ['pr.checks-failed'], webhook: 'https://hooks.example.com/lazybox', description: 'Post failing checks to the team channel' })],
    issues: [], path: '~/.config/lazybox/actions.yaml', schemaPath: '~/.config/lazybox/actions.schema.json',
  });
  const page = await open(WORKSPACE);
  await openSettings(page, 'Actions & hooks');
  await expect(page.getByRole('tabpanel', { name: 'Actions & hooks' })).toContainText('Run the tests');
  return page;
});

// Settings are files: keys.yaml with a mistake, said by line in Settings → Shortcuts.
shot('settings-files', async ({ f, open }) => {
  realistic(f); cacheTranscript(f);
  f.config.setFile('keys', { path: `${HOME}/keys.yaml`, schemaPath: `${HOME}/keys.schema.json`,
    value: { preset: 'vim', bindings: [{ action: 'review.withAgent', keys: ['Mod+Shift+r'], section: 'global', line: 5 }], menu: [], vim: { messageBox: false, fileEditor: false } },
    issues: [{ path: 'keys.global.palete.open', line: 7, message: 'there is no action "palete.open" (did you mean "palette.open"?) (lazybox do --list lists every action)', severity: 'error' }] });
  const page = await open(WORKSPACE);
  await openSettings(page, 'Shortcuts');
  await expect(page.getByRole('list', { name: 'Problems in keys.yaml' })).toContainText('line 7');
  // The toast said it as the file was saved; the picture is Settings.
  await page.locator('.toast').getByRole('button', { name: /Dismiss|Close/ }).click().catch(() => {});
  return page;
});

// Several Runners: Dev (this window's) and Working side by side, the release one stopped.
shot('several-runners', async ({ f, open, addRunner }) => {
  realistic(f); cacheTranscript(f);
  f.versionInfo.version = '0.1.22-dev';
  Object.assign(f.sessions[1], { status: 'waiting_input', agents: mainAgent('waiting_input', Date.now() - 5 * MIN), pending: [question('q-release', 'Which version should the notes start from?')] });
  const working = await addRunner('working');
  working.versionInfo.version = '0.1.21';
  const now = Date.now();
  Object.assign(working.collections[0], { name: 'Billing service' });
  Object.assign(working.collections[1], { name: 'Docs site' });
  Object.assign(working.sessions[0], { title: 'Migrate the invoices table', instructionsPending: false, status: 'waiting_approval', busy: false, agents: mainAgent('waiting_approval', now - MIN), pending: [approval('a-migrate', 'npm run migrate', 'Run the migration')] });
  Object.assign(working.sessions[1], { title: 'Rewrite the docs navigation', instructionsPending: false, status: 'done', busy: false, pending: [], agents: mainAgent('done', now - 15 * MIN, { turnEndedAtUnixMillis: ms(now - 15 * MIN), lastTurnStatus: 'completed' }), lastMessage: 'The navigation has three levels now.' });
  const page = await open(WORKSPACE, { runners: [
    { id: 'dev', name: 'dev', channel: 'dev', product: 'Lazybox Dev', source: 'registered', locator: `${HOME}/runners/dev.json`, current: true, state: 'running' },
    { id: 'working', name: 'working', channel: 'working', product: 'Lazybox Working', source: 'registered', locator: `${HOME}/runners/working.json`, current: false, state: 'running', url: '/__lazybox/runner/working', token: working.token },
    { id: 'release', name: 'lazybox', channel: 'release', product: 'Lazybox', source: 'registered', locator: `${HOME}/runners/lazybox.json`, current: false, state: 'stopped', problem: 'Open Lazybox to start it.' },
  ] });
  await expect(page.locator('.runner-group')).toHaveCount(3);
  await expect(page.locator('.panel-agents li').filter({ hasText: 'Migrate the invoices table' }).locator('.runner-badge')).toHaveText('Working');
  await resizeGroup(page, 'Workspaces', 'ArrowDown', 4);
  await blur(page);
  return page;
});

// ---- Helpers ---------------------------------------------------------------

/** Shows a panel from the dock's Panels menu ("Show X", or "Focus X" when it is already open). */
async function showPanel(page, title) {
  await page.locator('summary[aria-label="Panels"]').click();
  await page.getByRole('button', { name: new RegExp(`^(Show|Focus) ${title}$`) }).click();
}
/** Closes a panel by its tab's ×. */
async function closePanel(page, title) {
  await page.locator('.dv-tab').filter({ hasText: title }).locator('.dock-tab-close').click();
}
/**
 * Gives a panel's group more room from the keyboard, as a person would (⌃⌘ and
 * an arrow: Resize, move the active group's edge; 5 % a tap). The ring that
 * shows which group the keys move goes at the next pointer press. `title`:
 * the group to make active first (null: the active one).
 */
async function resizeGroup(page, title, arrow, taps) {
  if (title) await page.locator('.dv-tab').filter({ hasText: title }).first().click();
  for (let i = 0; i < taps; i++) { await page.keyboard.press(`Control+Meta+${arrow}`); await page.waitForTimeout(60); }
  await page.evaluate(() => window.dispatchEvent(new PointerEvent('pointerdown')));
}
async function openSettings(page, tab) {
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('tab', { name: tab, exact: true }).click();
}
async function openChanges(page) {
  await page.getByRole('button', { name: 'Review changes', exact: true }).click();
  const changes = page.getByRole('region', { name: 'Workspace changes' });
  await expect(changes.getByRole('grid', { name: 'File diff' })).toContainText('const max = 1000;');
  return changes;
}
async function comment(page, changes, line, text) {
  const diff = changes.getByRole('grid', { name: 'File diff' });
  await diff.getByRole('button', { name: line, exact: true }).focus();
  await page.keyboard.press('Enter');
  const field = changes.getByRole('textbox', { name: 'Comment' });
  await field.fill(text);
  await field.press('ControlOrMeta+Enter');
  await expect(changes.getByRole('textbox', { name: 'Comment' })).toHaveCount(0);
}
/** No focus ring in the picture. */
async function blur(page) { await page.evaluate(() => document.activeElement?.blur()); }

/**
 * Ready for the picture: fonts loaded, and the page has stopped asking the
 * Runner for things (no request started for 600 ms; streams that stay open do
 * not count), so a panel that loads late is in the picture every time.
 */
export async function settle(page) {
  await page.evaluate(() => document.fonts.ready);
  const started = Date.now();
  for (;;) {
    const quiet = await page.evaluate(() => {
      const last = Math.max(0, ...performance.getEntriesByType('resource').map(entry => entry.startTime));
      return performance.now() - last;
    });
    if (quiet >= 600 || Date.now() - started > 8000) break;
    await page.waitForTimeout(Math.max(100, 600 - quiet));
  }
  await page.waitForTimeout(200);
}
