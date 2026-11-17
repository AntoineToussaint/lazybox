import { scenario, caption, say, press, click, expect, highlight, wait, button, group, css, transcript } from './lib/dsl.ts';

export default scenario({
  title: 'From failing checks to a merged pull request',
  story: { as: 'a developer whose agent opened a pull request', want: 'failing checks to reach the agent in one click', soThat: 'I merge a green pull request without copying CI logs around' },
  tags: ['github', 'pull-requests', 'inbox', 'ci'],
  viewport: 'desktop',
  theme: 'dark',
  build(w) {
    w.connectGitHub();
    const refunds = w.workspace('Refund rounding', { repos: ['acme/api'], branch: '812-fix-refund-rounding' });
    const fix = refunds.conversation('Fix refund rounding', { createdMinutesAgo: 25 });
    fix.history(h => h
      .prompt('Refunds of 0.005 round down instead of to even (acme/api#812). Fix it, with a test.')
      .read('api/ledger/round.go', 'api/ledger/round_test.go')
      .edit('api/ledger/round.go', 'math.Floor(v*100) / 100', 'math.RoundToEven(v*100) / 100')
      .bash('go test ./ledger/...', 'ok  \tacme/api/ledger\t0.214s\n', { description: 'Run the ledger tests' })
      .reply('Refunds now round half to even; `TestRefundRounding` covers 10.005 → 10.01. **The ledger tests pass.**')
      .end(), { endedMinutesAgo: 4 });
    const pr = refunds.pullRequest({ number: 815, title: 'Round refunds half to even', conversation: fix, checks: 14 });
    w.open(fix);
    const row = css('.pr-inbox-item', 'the pull request’s Inbox row').withText('acme/api#815').first();
    const panelItem = css('.pull-requests-panel .pr-item', 'its row in Pull requests').withText('acme/api#815');
    return [
      caption('The fix is ready: ask Claude for a pull request'),
      say('Open a pull request for this.'),
      press('Enter'),
      fix.receives('message', 'Open a pull request'),
      fix.thinks(1),
      fix.opensPR(pr),
      fix.writes('Opened **#815** “Round refunds half to even”. CI is running.'),
      fix.done(),
      caption('Lazybox tracks it: a pill in the header, live from GitHub'),
      expect.exactText(css('.pr-pill').first(), '#815'),
      highlight(css('.pr-pill').first(), { label: 'Checks running' }),
      caption('CI fails on two checks', { hold: 1.6 }),
      pr.checksFail(['lint', 'unit-api']),
      expect.attribute(css('.pr-pill').first(), 'class', /is-failure/),
      highlight(css('.pr-pill').first(), { label: '2 of 14 checks failed' }),
      caption('The Inbox says so, with the reaction one click away'),
      click(css('.inbox-action', 'the Inbox')),
      expect.text(row, 'lint, unit-api (2 of 14)'),
      click(button('Send the failures to Claude').in(row)),
      expect.call('SendPullRequestFailures', body => body.ref?.key === 'acme/api#815'),
      expect.text(row, 'Sent 2 failing checks'),
      press('Escape'),
      caption('Claude gets the failing checks and their logs, not a wall of CI output'),
      expect.visible(css('.system-event', 'the checks chip').withText('Checks failed on acme/api#815')),
      fix.thinks(1.2),
      fix.writes('`lint` flags an unused import; `unit-api` expects the old rounding in `refund_api_test.go`. Fixing both.'),
      fix.edits('api/ledger/round.go'),
      fix.edits('api/http/refund_api_test.go', { from: '10.00', to: '10.01' }),
      fix.runs('make lint test', { output: 'ok  \tacme/api/http\t0.512s\nok  \tacme/api/ledger\t0.198s\n', seconds: 2, description: 'Run lint and the tests' }),
      fix.pushes(pr),
      fix.writes('Pushed the fix: lint is clean and **all API tests pass**.'),
      fix.done(),
      pr.checksRunning(),
      wait(1.5),
      caption('Green, and approved by a reviewer'),
      pr.checksPass({ approved: true }),
      expect.attribute(css('.pr-pill').first(), 'class', /is-success/),
      caption('Merge from the Pull requests panel'),
      press('Mod+K'),
      say('Show pull requests'),
      press('Enter'),
      expect.visible(panelItem),
      click(button('Merge').in(panelItem)),
      expect.text(group('Confirm merge'), 'Merge #815 into main?'),
      click(button('Merge').in(group('Confirm merge'))),
      expect.call('MergePullRequest', body => body.item?.key === 'acme/api#815'),
      expect.text(css('.pull-requests-panel .group-merged'), 'acme/api#815'),
      expect.text(transcript(), 'all API tests pass'),
      caption('Failing checks to merged, without leaving Lazybox', { hold: 3 }),
    ];
  },
});
