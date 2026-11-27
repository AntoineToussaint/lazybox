const readyOnly = document.getElementById('ready-only');
const count = document.getElementById('ready-count');
const labels = { running: 'Running', stopping: 'Stopping', succeeded: 'Finished', failed: 'Failed', stopped: 'Stopped', idle: 'Not running' };
let snapshot;

function priority(service) {
  if (service.state === 'failed') return 0;
  if (service.preview) return 1;
  if (service.state === 'running' || service.state === 'stopping') return 2;
  return 3;
}

function render() {
  if (!snapshot) return;
  const { workspaceId, services, setups } = snapshot;
  count.textContent = String(services.filter(service => service.preview).length);
  const visible = services.filter(service => !readyOnly.checked || service.preview);
  visible.sort((a, b) => priority(a) - priority(b) || a.member.localeCompare(b.member) || a.name.localeCompare(b.name));
  workflow.list(visible.map(service => {
    const show = (label, kind, target) => workflow.button(label, () => lazybox.runtime.show(workspaceId, kind, target));
    const actions = [];
    if (service.preview) actions.push(show('Open preview', 'preview', service.id));
    if (service.hasLog && service.state === 'failed') {
      const inspect = show('Inspect failure log', 'logs', service.id);
      inspect.classList.add('primary');
      actions.push(inspect);
    }
    if (!service.preview) actions.push(show('Show in Actions', 'service', service.id));
    if (service.hasLog && service.state !== 'failed') actions.push(show('Open logs', 'logs', service.id));
    const setup = setups.find(item => item.member === service.member);
    const detail = [service.member, service.port && 'Port ' + service.port];
    if (setup && (setup.pendingApproval || setup.missing || ['failed', 'stopped'].includes(setup.state))) {
      if (setup.pendingApproval) detail.push('Setup needs approval');
      if (setup.missing) detail.push(setup.missing + (setup.missing === 1 ? ' missing file' : ' missing files'));
      if (setup.state === 'failed' || setup.state === 'stopped') detail.push('Setup ' + setup.state);
      actions.push(show('Review repository setup', 'setup', service.member));
    }
    if (service.preview) actions[0].classList.add('primary');
    const row = workflow.row(service.id, service.name, detail.filter(Boolean).join(' · '), ...actions);
    const status = document.createElement('span');
    status.className = 'service-state';
    status.textContent = labels[service.state] || service.state;
    row.dataset.state = service.state;
    row.querySelector('strong').append(' ', status);
    return row;
  }), readyOnly.checked ? 'No previews are ready. Start a service with a preview URL from the Actions panel.' : 'No services are configured in this workspace.');
}

readyOnly.onchange = render;
workflow.poll(5000, async () => {
  const workspace = await workflow.workspace();
  if (snapshot?.workspaceId !== workspace?.id) {
    snapshot = undefined;
    count.textContent = '';
    readyOnly.checked = false;
    if (workspace) workflow.list([], 'Loading services…');
  }
  if (!workspace) return;
  const { services, setups } = await lazybox.runtime.read(workspace.id);
  snapshot = { workspaceId: workspace.id, services, setups };
  render();
});
