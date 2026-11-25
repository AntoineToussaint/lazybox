const attentionOnly = document.getElementById('attention');
const count = document.getElementById('attention-count');
let entries = [];

function render() {
  const needsAttention = entries.filter(entry => entry.tone === 'warn' || entry.tone === 'danger');
  count.textContent = String(needsAttention.length);
  const visible = attentionOnly.checked ? needsAttention : entries;
  workflow.list(visible.map(entry => entry.row), attentionOnly.checked
    ? 'No setup issues need attention.' : 'No setup or service activity in this workspace.');
}
attentionOnly.onchange = render;

function entry(key, title, detail, state, tone, actions) {
  const row = workflow.row(key, title, detail, ...actions);
  const badge = document.createElement('span');
  badge.className = 'bootstrap-state';
  badge.dataset.tone = tone;
  badge.textContent = state;
  row.querySelector('strong').append(' ', badge);
  return { row, tone };
}

workflow.poll(5000, async () => {
  const workspace = await workflow.workspace();
  if (!workspace) { entries = []; count.textContent = '0'; attentionOnly.disabled = true; return; }
  const { setups, services } = await lazybox.runtime.read(workspace.id);
  const show = (label, kind, target) => workflow.button(label, () => lazybox.runtime.show(workspace.id, kind, target));
  entries = setups.map(setup => {
    const detail = ['Repository setup'];
    if (setup.missing) detail.push(setup.missing + (setup.missing === 1 ? ' missing file' : ' missing files'));
    if (setup.copied) detail.push(setup.copied + (setup.copied === 1 ? ' file copied' : ' files copied'));
    const failed = setup.state === 'failed';
    const state = setup.pendingApproval ? 'Needs approval' : failed ? 'Failed' :
      setup.missing ? 'Missing files' : setup.state === 'succeeded' ? 'Complete' : setup.state;
    const tone = failed ? 'danger' : setup.pendingApproval || setup.missing ? 'warn' : setup.state === 'succeeded' ? 'ok' : 'muted';
    const label = setup.pendingApproval ? 'Review setup' : failed ? 'Inspect failed setup' : setup.missing ? 'Inspect missing files' : 'Open setup';
    return entry('setup:' + setup.member, setup.member, detail.join(' · '), state, tone, [show(label, 'setup', setup.member)]);
  });
  for (const service of services) {
    if (service.state === 'idle') continue;
    const actions = [];
    if (service.hasLog) actions.push(show('Open logs', 'logs', service.id));
    if (service.preview) actions.push(show('Open preview', 'preview', service.id));
    const tone = service.state === 'failed' ? 'danger' : service.state === 'running' ? 'ok' : 'muted';
    entries.push(entry('service:' + service.id, service.name, service.member + ' · Service', service.state, tone, actions));
  }
  const order = { danger: 0, warn: 1, muted: 2, ok: 3 };
  entries.sort((a, b) => order[a.tone] - order[b.tone]);
  attentionOnly.disabled = false;
  render();
});
