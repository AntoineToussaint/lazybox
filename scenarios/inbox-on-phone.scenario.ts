import { scenario, caption, click, say, expect, highlight, wait, button, role, css, heading, textbox } from './lib/dsl.ts';

export default scenario({
  title: 'Triage the Inbox from your phone',
  story: { as: 'a developer away from my desk', want: 'to approve, answer and reply to my agents from my phone', soThat: 'they keep working while I am out' },
  tags: ['phone', 'remote', 'inbox'],
  viewport: 'phone',
  theme: 'dark',
  build(w) {
    const api = w.workspace('API upkeep', { repos: ['acme/api'], branch: 'chore/deps' });
    const release = w.workspace('Release 2.4', { repos: ['acme/web', 'acme/api'], branch: 'release/2.4' });
    const deps = api.conversation('Bump dependencies', { createdMinutesAgo: 70 });
    deps.history(h => h.prompt('Bump the dependencies and run the tests.').reply('`undici` 7.4 → 7.6 and `zod` 4.1 → 4.3 have new minor versions. I’ll update them.'), { endedMinutesAgo: 2 });
    const notes = release.conversation('Write the release notes', { agent: 'codex', createdMinutesAgo: 45 });
    notes.history(h => h.prompt('Write the 2.4 release notes from the merged pull requests.').read('web/CHANGELOG.md').reply('I have the merged pull requests since 2.3.'), { endedMinutesAgo: 5 });
    const parser = api.conversation('Split the parser', { createdMinutesAgo: 120 });
    parser.history(h => h
      .prompt('Split the query parser into lexer, parser and printer.')
      .read('api/src/query/parser.ts')
      .bash('npm test -- query', 'Tests  42 passed (42)\n', { description: 'Run the query tests' })
      .reply('Split into `lexer.ts`, `parser.ts` and `printer.ts`. **All 42 tests pass.**')
      .end(), { endedMinutesAgo: 12 });
    w.open('phone');
    const row = (title: string) => css('.remote-row', `the “${title}” row`).withText(title).first();
    return [
      caption('Away from your desk? Your agents wait on you here'),
      deps.requests('npm update', { description: 'Update the dependencies' }),
      notes.asks({ question: 'Which version should the notes start from?', options: [{ label: '2.3.0', description: 'The last release' }, { label: '2.3.2', description: 'The last patch' }] }),
      expect.visible(heading('Inbox')),
      expect.text(row('Bump dependencies'), 'npm update'),
      highlight(css('.group-needs-input', 'Needs input'), { label: 'Needs you', hold: 1.6 }),
      caption('An approval: **Allow** from the row'),
      click(button('Allow').in(row('Bump dependencies'))),
      expect.text(css('.remote-snackbar'), 'Allowed · Bump dependencies'),
      deps.receives('approval', 'DECISION_ACCEPT'),
      deps.runs('npm update', { output: 'changed 2 packages in 3s\n', seconds: 1.2 }),
      deps.runs('npm test', { output: 'Tests  128 passed (128)\n', seconds: 1.2 }),
      deps.writes('Updated `undici` and `zod`; **all 128 tests pass.**'),
      deps.done(),
      caption('A question: tap the answer'),
      expect.visible(role('radio', '2.3.2').in(row('Write the release notes'))),
      click(role('radio', '2.3.2').in(row('Write the release notes'))),
      notes.receives('answer', '2.3.2'),
      notes.thinks(1),
      notes.writes('Release notes from 2.3.2: **14 changes**, grouped by area, in `CHANGELOG.md`.'),
      notes.done(),
      caption('A finished turn: read it, and reply'),
      click(css('.remote-row-open', 'Split the parser').in(row('Split the parser'))),
      expect.visible(heading('Split the parser')),
      expect.text(css('.remote-transcript'), 'All 42 tests pass'),
      say('Looks good. Push it and open a pull request.', { into: textbox('Message') }),
      click(button('Send message')),
      parser.receives('message', 'Push it'),
      parser.thinks(0.8),
      parser.runs('git push -u origin chore/deps', { output: 'branch set up to track origin/chore/deps\n', seconds: 1 }),
      parser.writes('Pushed, and opened **#823** “Split the query parser”.'),
      parser.done(),
      expect.text(css('.remote-transcript'), 'Split the query parser'),
      wait(0.8),
      click(button('Back to inbox')),
      expect.visible(heading('Inbox')),
      expect.hidden(css('.group-needs-input .remote-row')),
      caption('Three agents unblocked from your pocket', { hold: 3 }),
    ];
  },
});
