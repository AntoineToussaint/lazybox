import { scenario, caption, say, press, click, rightClick, expect, wait, menuitem, tab, role, css, text, label } from './lib/dsl.ts';

const PLAN_V1 = `# Billing datasource rollout

Move billing's reads to the regional datasource, then its writes.

## Phases
1. **Dual read** behind a per-tenant flag; compare results.
2. **Dual write** once two weeks of reads match.
3. **Cut over** tenant by tenant.

## Rollback
Turning the flag off sends a tenant back to the legacy database at once.
`;
const PLAN_V2 = PLAN_V1
  .replace('2. **Dual write** once two weeks of reads match.', '2. **Dual write** once two weeks of reads match, **behind its own flag**.')
  .replace('3. **Cut over** tenant by tenant.', '3. **Cut over** tenant by tenant, largest last.\n4. **Remove** the legacy read path.');

export default scenario({
  title: 'Track a plan the agent wrote, and see what changed',
  story: { as: 'a tech lead reviewing an agent’s rollout plan', want: 'the Markdown file it wrote to become a document I can follow', soThat: 'every revision is kept and I read only what changed' },
  tags: ['artifacts', 'markdown', 'review'],
  viewport: 'desktop',
  theme: 'dark',
  build(w) {
    const billing = w.workspace('Billing datasource', { repos: ['acme/billing'], branch: 'plan/datasource' });
    billing.file('docs/ROLLOUT.md', PLAN_V1);
    const plan = billing.conversation('Plan the datasource move', { createdMinutesAgo: 35 });
    plan.history(h => h
      .prompt('Write a rollout plan for moving billing to the regional datasource. Keep it short.')
      .read('billing/src/store.ts', 'billing/src/tenants.ts')
      .tool('Write', { file_path: `${billing.root}/billing/docs/ROLLOUT.md`, content: PLAN_V1 })
      .reply('The plan is in `docs/ROLLOUT.md`: three phases, each behind a flag, with a rollback for every step.')
      .end(), { endedMinutesAgo: 5 });
    w.open(plan);
    const fileLink = role('button', 'docs/ROLLOUT.md').in(css('.messages')).last();
    const page = role('article', 'Billing datasource rollout');
    return [
      caption('The agent wrote its plan to a Markdown file'),
      expect.visible(fileLink),
      caption('Right-click the file it named: **Track as artifact**'),
      rightClick(fileLink),
      click(menuitem('Track as artifact')),
      expect.call('TrackArtifactFile', body => body.path === 'docs/ROLLOUT.md'),
      expect.visible(page),
      expect.text(page, 'Dual read'),
      caption('A document now, kept in sync with the file'),
      expect.text(css('.doc-title-block'), 'docs/ROLLOUT.md'),
      wait(1.2),
      caption('Ask for a change in the conversation…'),
      say('Put the dual write behind its own flag, and add a step to remove the old path.'),
      press('Enter'),
      plan.receives('message', 'dual write'),
      plan.thinks(1),
      plan.edits('docs/ROLLOUT.md', { from: 'once two weeks of reads match.', to: 'once two weeks of reads match, behind its own flag.' }),
      billing.changesFile('docs/ROLLOUT.md', PLAN_V2),
      plan.writes('Updated `docs/ROLLOUT.md`: dual writes have their own flag, and phase 4 removes the legacy read path.'),
      plan.done(),
      caption('…and the artifact follows: version 2, from the file on disk'),
      expect.text(css('.artifact-meta'), 'File changed on disk'),
      expect.value(label('Version').in(page), '2'),
      expect.text(page, 'Remove'),
      caption('**Changes** shows the two versions side by side, rendered'),
      click(tab('Changes').in(page)),
      expect.visible(role('region', 'Rendered changes').in(page)),
      wait(2.2),
      click(role('radio', 'Text').in(page)),
      expect.visible(css('.artifact-diff-line.is-add').first()),
      wait(1.6),
      click(tab('Document').in(page)),
      caption('Every revision kept, and you read only what changed', { hold: 3 }),
      expect.visible(text('Remove').in(page).first()),
    ];
  },
});
