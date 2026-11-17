import { scenario, caption, say, press, click, expect, highlight, wait, css, role, textbox, transcript } from './lib/dsl.ts';

const packages = ['auth', 'billing', 'catalog', 'http', 'invoices', 'ledger', 'mail', 'search', 'tenants', 'users'];
/** What `go test ./...` prints for a mid-sized service: hundreds of passing lines, two failures. */
function testLog(failing: boolean) {
  const lines: string[] = [];
  for (const pkg of packages) {
    for (let i = 1; i <= 34; i++) lines.push(`=== RUN   Test${pkg[0].toUpperCase()}${pkg.slice(1)}Case${i}`, `--- PASS: Test${pkg[0].toUpperCase()}${pkg.slice(1)}Case${i} (0.0${i % 9}s)`);
    if (failing && pkg === 'ledger') lines.push('=== RUN   TestRefundRounding', '    refund_test.go:42: refund(10.005) = 10.00, want 10.01', '--- FAIL: TestRefundRounding (0.00s)', '=== RUN   TestRefundPartial', '    refund_test.go:77: refund(3.335, 0.5) = 1.66, want 1.67', '--- FAIL: TestRefundPartial (0.00s)', 'FAIL', `FAIL\tacme/api/${pkg}\t0.212s`);
    else lines.push(`ok  \tacme/api/${pkg}\t0.${100 + packages.indexOf(pkg) * 37}s`);
  }
  return lines.join('\n') + '\n';
}
const FAILURES = `go test ./... → exit 1 (go-test filter: 708 passing lines dropped)

--- FAIL: TestRefundRounding (0.00s)
    refund_test.go:42: refund(10.005) = 10.00, want 10.01
--- FAIL: TestRefundPartial (0.00s)
    refund_test.go:77: refund(3.335, 0.5) = 1.66, want 1.67
FAIL	acme/api/ledger	0.212s

Full output: ~/Library/Application Support/Lazybox/output/go-test-1.log`;

export default scenario({
  title: 'The agent runs the tests: you see everything, it gets the failures',
  story: { as: 'a developer watching an agent run a big test suite', want: 'the full output live in a console while the agent reads only what failed', soThat: 'it fixes the right thing without spending its context on passing tests' },
  tags: ['command-output', 'console', 'tests'],
  viewport: 'desktop',
  theme: 'dark',
  build(w) {
    const api = w.workspace('Refund API', { repos: ['acme/api'], branch: 'fix/refunds' });
    const conv = api.conversation('Make the refund tests pass', { createdMinutesAgo: 20 });
    conv.history(h => h.prompt('Refunds round wrongly since the ledger change. Look into it.').read('api/ledger/round.go').reply('The ledger rounds with `math.Floor`. Tell me when to run the tests.').end(), { endedMinutesAgo: 2 });
    w.open(conv);
    const report = css('[data-testid="command-report"]', 'what the agent got');
    return [
      caption('Ask the agent to run the whole suite'),
      say('Run all the API tests and fix what fails.'),
      press('Enter'),
      conv.receives('message', 'Run all the API tests'),
      conv.thinks(0.8),
      caption('A long run takes over the idle console: every line, live'),
      conv.runs('go test ./...', { output: testLog(true), exit: 1, seconds: 5, description: 'Run all the API tests', console: { agentView: FAILURES, filter: 'go-test', outputBytes: 1_468_006 } }),
      expect.visible(css('[data-testid="console-output"]', 'the console output')),
      expect.exactText(css('.command-panel .console-state [role=status]'), 'Failed · exit 1'),
      expect.visible(report),
      expect.text(report, 'go-test filter'),
      caption('…but the agent read **a few hundred bytes of 1.4 MiB**: just the failures'),
      highlight(report, { label: 'What the agent read', hold: 2.4 }),
      caption('You can still find anything in the full output'),
      say('FAIL', { into: textbox('Search output') }),
      wait(1.4),
      caption('The agent’s view: two failing tests, their lines, the full log’s path'),
      click(role('button', /^Run all the API tests .*Failed$/).in(transcript()).first()),
      expect.text(transcript(), 'refund(10.005) = 10.00, want 10.01'),
      wait(1.4),
      conv.writes('Two failures, both in `ledger`: `math.Floor` drops the half cent. I’ll round half to even.'),
      conv.edits('api/ledger/round.go', { from: 'math.Floor(v*100) / 100', to: 'math.RoundToEven(v*100) / 100' }),
      caption('It runs the suite again; the console stays on what you are reading'),
      conv.runs('go test ./...', { output: testLog(false), exit: 0, seconds: 3.5, description: 'Run all the API tests again', console: { agentView: 'go test ./... → exit 0 (go-test filter: 680 passing lines dropped)\nok — 10 packages', filter: 'go-test', outputBytes: 1_402_113 } }),
      expect.text(report, 'Exit 1'),
      caption('The new run is offered: **Show** it when you are ready'),
      click(role('button', /^1 newer run, the newest go test/)),
      expect.visible(report),
      expect.text(report, 'Exit 0'),
      conv.writes('Fixed: refunds round half to even. **All 10 packages pass.**'),
      conv.done(),
      caption('Full output for you, a short answer for the agent', { hold: 3 }),
    ];
  },
});
