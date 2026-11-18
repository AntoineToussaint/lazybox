import { scenario, caption, press, paste, say, click, expect, highlight, agent, wait, button, group, textbox, dialog, css, transcript } from './lib/dsl.ts';

export default scenario({
  title: 'Start a task from a GitHub issue',
  story: { as: 'a developer with an issue to fix', want: 'to start an agent from the issue’s link', soThat: 'it gets the issue, a workspace and a branch without me retyping anything' },
  tags: ['github', 'new-task', 'issues'],
  viewport: 'desktop',
  theme: 'dark',
  build(w) {
    w.connectGitHub();
    const payments = w.workspace('Payments', { repos: ['acme/api', 'acme/web'], branch: 'feature/invoices' });
    payments.conversation('Invoice PDFs', { createdMinutesAgo: 90 }).history(h => h
      .prompt('Render invoices as PDFs, with the company logo.')
      .read('api/src/invoices.ts', 'api/src/pdf.ts')
      .reply('Invoices render as PDFs now, logo included. **All 31 tests pass.**')
      .end(), { endedMinutesAgo: 40 });
    const issue = w.issue({
      repo: 'acme/api', number: 812, title: 'Fix refund rounding', labels: ['bug', 'payments'], author: 'alice',
      body: 'Refunds of 0.005 round down instead of to even.\n\nSee `ledger/round.go`.',
      comments: [{ author: 'bob', body: 'Repro: refund 10.005 → 10.00 (expected 10.01)', daysAgo: 2 }],
    });
    w.open(payments);
    const sheet = dialog('New task');
    const source = group('From acme/api#812').in(sheet);
    return [
      caption('An issue to fix: start the task **from its link**'),
      press('Mod+N'),
      expect.focused(textbox('First message').in(sheet)),
      paste(issue, { into: textbox('First message').in(sheet) }),
      expect.visible(source),
      expect.exactText(css('.item-source-title'), 'Fix refund rounding'),
      caption('Lazybox reads the issue: its **repository**, a **branch**, a **name**'),
      highlight(source, { label: 'The issue, quoted for the agent' }),
      expect.exactText(css('.new-task-chip-name'), ['acme/api']),
      expect.exactText(css('[data-testid="new-task-branch"]'), '812-fix-refund-rounding'),
      highlight(css('[data-testid="new-task-branch"]'), { label: 'Its own branch', hold: 1.4 }),
      caption('See exactly what the agent will get'),
      click(button('What the agent gets').in(source)),
      expect.text(css('pre[aria-label="What the agent gets"], [aria-label="What the agent gets"]').first(), 'Repro: refund 10.005'),
      wait(1.6),
      click(button('Hide').in(source)),
      caption('Add a word of your own, and go'),
      say('Start with the ledger: round half to even.', { into: textbox('First message').in(sheet) }),
      press('Enter'),
      expect.hidden(sheet),
      agent.receives('message', 'External content: GitHub issue acme/api#812'),
      caption('A workspace on its branch, and the agent starts with the issue in hand'),
      expect.visible(css('.chat-heading .item-origin-chip')),
      agent.thinks(1.4),
      agent.reads('api/ledger/round.go', 'api/ledger/round_test.go'),
      agent.writes('The ledger rounds with `math.Floor`, so 10.005 becomes 10.00. I’ll switch to banker’s rounding (half to even) and add the repro from the issue as a test.'),
      agent.edits('api/ledger/round.go', { from: 'math.Floor(v*100) / 100', to: 'math.RoundToEven(v*100) / 100' }),
      agent.runs('go test ./ledger/...', { output: 'ok  \tacme/api/ledger\t0.214s\n', seconds: 1.6, description: 'Run the ledger tests' }),
      agent.writes('Fixed: refunds round half to even, and `TestRefundRounding` covers 10.005 → 10.01. **The ledger tests pass.**'),
      agent.done(),
      expect.text(transcript(), 'The ledger tests pass'),
      caption('From an issue link to a tested fix, without retyping a thing', { hold: 3 }),
    ];
  },
});
