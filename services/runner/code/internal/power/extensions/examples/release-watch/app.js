workflow.openInbox();
let checks = [], releases = [], checksError = '', releaseError = '';

function render() {
  workflow.list([...releases, ...checks], 'No open pull requests or deployments to look at.');
  if (checksError || releaseError) workflow.say([checksError, releaseError].filter(Boolean).join(' · '));
}

workflow.poll(20000, async () => {
  try {
    const { pulls, error } = await lazybox.pulls.list();
    checksError = error;
    const seen = new Set();
    checks = [];
    for (const pull of pulls) {
      const identity = `${pull.repository.toLowerCase()}#${pull.number}`;
      if (seen.has(identity) || ['merged', 'closed'].includes(pull.status)) continue;
      seen.add(identity);
      const actions = [];
      if (pull.checkUrl) actions.push(workflow.button('Open failing check', () => lazybox.github.open(pull.checkUrl)));
      if (pull.url) actions.push(workflow.button('Open pull request', () => lazybox.github.open(pull.url)));
      if (pull.sessionId) actions.push(workflow.button('Open conversation', () => lazybox.conversations.show(pull.sessionId)));
      checks.push(workflow.row(identity, pull.title || identity, [identity, pull.detail].filter(Boolean).join(' · '), ...actions));
    }
  } catch (error) { checksError = error.message; }
  render();
});

workflow.poll(300000, async () => {
  try {
    const { items, truncated } = await lazybox.releases.list();
    releaseError = truncated ? 'Some deployment activity could not be loaded.' : '';
    releases = items.map(item => workflow.row(item.id, item.environment || 'Deployment',
      `${item.repository} · ${item.reason === 'deployment_approval' ? 'Needs your approval' : 'Deployment failed'}`,
      workflow.button('Open deployment', () => lazybox.github.open(item.url))));
  } catch (error) { releaseError = error.message; }
  render();
});
