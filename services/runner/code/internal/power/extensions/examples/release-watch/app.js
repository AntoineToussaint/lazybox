workflow.openInbox();
const allPulls = document.getElementById('all-pulls');
let checks = [], releases = [], checksError = '', releaseError = '';
allPulls.addEventListener('change', render);

function render() {
  const rows = [...releases, ...checks]
    .filter(entry => allPulls.checked || entry.priority < 2)
    .sort((a, b) => a.priority - b.priority);
  workflow.list(rows.map(entry => entry.row), allPulls.checked
    ? 'No open pull requests or deployments.'
    : 'No deployment approvals or failures to address.');
  if (checksError || releaseError) workflow.say([checksError, releaseError].filter(Boolean).join(' · '));
}

workflow.poll(20000, async () => {
  try {
    const { pulls, error, truncated } = await lazybox.pulls.list();
    checksError = [error, truncated && 'Showing the first 100 pull requests.'].filter(Boolean).join(' · ');
    const seen = new Set();
    checks = [];
    for (const pull of pulls) {
      const identity = `${pull.repository.toLowerCase()}#${pull.number}`;
      if (seen.has(identity) || ['merged', 'closed'].includes(pull.status)) continue;
      seen.add(identity);
      const actions = [];
      const failed = pull.checks === 'failure' || pull.status === 'checks-failed';
      if (failed) {
        const failures = pull.failingChecks || [{ name: '', url: pull.checkUrl }];
        for (const check of failures) {
          if (check.url) actions.push(workflow.button(check.name ? 'Inspect ' + check.name : 'Open failing check', () => lazybox.github.open(check.url)));
        }
      }
      if (pull.url) actions.push(workflow.button('Open pull request', () => lazybox.github.open(pull.url)));
      if (pull.sessionId) actions.push(workflow.button('Open conversation', () => lazybox.conversations.show(pull.sessionId)));
      const row = workflow.row(identity, pull.title || identity, [identity, pull.detail].filter(Boolean).join(' · '), ...actions);
      if (failed) row.classList.add('failed');
      checks.push({ row, priority: failed ? 1 : 2 });
    }
  } catch (error) { checksError = error.message; }
  render();
});

workflow.poll(300000, async () => {
  try {
    const { items, truncated } = await lazybox.releases.list();
    releaseError = truncated ? 'Some deployment activity could not be loaded.' : '';
    releases = items.map(item => {
      const approval = item.reason === 'deployment_approval';
      const row = workflow.row(item.id, item.environment || 'Deployment',
        item.repository + ' · ' + (approval ? 'Needs your approval' : 'Deployment failed'),
        workflow.button(approval ? 'Review deployment' : 'Inspect failed deployment', () => lazybox.github.open(item.url)));
      row.classList.add(approval ? 'approval' : 'failed');
      return { row, priority: approval ? 0 : 1 };
    });
  } catch (error) { releaseError = error.message; }
  render();
});
