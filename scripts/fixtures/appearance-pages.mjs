// The pages appearance checks look at, with content that exercises every
// kind of colour: chat prose, a table, highlighted code, a diff, a diagram,
// an approval card, notices, the sidebar, Settings and the inbox.
// Explicitly fake: the fake Runner's events, no provider.
import { expect } from '@playwright/test';
import { seedDesignDoc } from './artifact-sample.mjs';

export const richMessage = [
  '# Parser fix',
  '',
  'Fixed **nested lists**: `parseBlock` stops at a blank line now. See [the issue](https://example.invalid/1).',
  '',
  '| Check | Result |',
  '|---|---|',
  '| unit | 40 passed |',
  '| lint | clean |',
  '',
  '```json',
  '{ "ready": true, "count": 2, "name": "parser" }',
  '```',
  '',
  '```diff',
  '- const list = parse(lines);',
  '+ const list = parseBlock(lines, { nested: true });',
  '```',
  '',
  '> Quoted note from the reviewer.',
  '',
  '```mermaid',
  'flowchart LR',
  '  Request --> Parser --> Result',
  '```',
].join('\n');

/** Seeds the first conversation with rich content and a pending approval. */
export function seedConversation(fixture) {
  const session = fixture.sessions[0];
  const channel = fixture.openEvents(session.id);
  channel.push({ type: 'operation_accepted', text: 'Fix the parser and show me what you checked.' });
  channel.push({ type: 'message_delta', text: richMessage });
  channel.push({ type: 'turn_ended', status: 'completed' });
  const request = { type: 'approval', requestId: 'approval-appearance', text: 'Bash', providerDataJson: Buffer.from(JSON.stringify({ tool: 'Bash', input: { command: 'go test ./...', description: 'Run the tests' }, toolUseId: 'tool-appearance' })).toString('base64') };
  session.pending = [request]; session.status = 'waiting_approval';
  channel.push(request);
  // A long design doc for the artifact reader page.
  fixture.designDoc = seedDesignDoc(fixture, { sessionId: session.id });
  return session;
}

/** Visits each key page; `shot(name)` is called once it has settled. */
export async function visitKeyPages(page, base, token, shot, { settingsTabs = ['Subscriptions'], artifactId = '' } = {}) {
  if (artifactId) {
    // The artifact reader, and its rendered comparison of two versions.
    await page.goto(`${base}/?view=artifact&collection=alpha&artifact=${artifactId}#token=${token}`);
    const article = page.getByRole('article', { name: 'Datasource migration' });
    await expect(article.getByRole('heading', { level: 1 })).toBeVisible();
    await expect.poll(() => article.getByRole('img', { name: 'Mermaid diagram' }).evaluate(img => img.complete && img.naturalWidth > 0).catch(() => false)).toBe(true);
    await page.waitForTimeout(600);
    await shot('artifact');
    await article.locator('#rollout-plan').evaluate(h => h.scrollIntoView({ block: 'start' }));
    await page.waitForTimeout(300);
    await shot('artifact-tables');
    await article.getByRole('tab', { name: 'Changes' }).click();
    await expect(article.getByRole('region', { name: 'Rendered changes' })).toBeVisible();
    await page.waitForTimeout(600);
    await shot('artifact-changes');
  }
  await page.goto(`${base}/?view=workspace&collection=alpha#token=${token}`);
  await expect(page.getByRole('navigation', { name: 'Workspaces', exact: true })).toBeVisible();
  await page.waitForTimeout(400);
  await shot('home');
  await page.goto(`${base}/?view=workspace&collection=alpha&session=alpha-session#token=${token}`);
  await expect(page.getByRole('heading', { name: 'Parser fix' })).toBeVisible();
  await expect.poll(() => page.getByRole('img', { name: 'Mermaid diagram' }).evaluate(img => img.complete && img.naturalWidth > 0).catch(() => false)).toBe(true);
  // The diagram settles (fitted to its viewport) a moment after it loads.
  await page.waitForTimeout(1000);
  await shot('conversation');
  const inbox = page.locator('.inbox-action');
  if (await inbox.isVisible()) { await inbox.click(); await page.waitForTimeout(300); await shot('inbox'); await page.keyboard.press('Escape'); }
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  for (const tab of settingsTabs) {
    const button = page.getByRole('tab', { name: tab, exact: true });
    if (!await button.count()) continue;
    await button.click(); await page.waitForTimeout(300);
    await shot(`settings-${tab.toLowerCase().replace(/[^a-z]+/g, '-')}`);
  }
}
