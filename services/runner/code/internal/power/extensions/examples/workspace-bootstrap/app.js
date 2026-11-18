workflow.poll(5000, async () => {
  const workspace = await workflow.workspace();
  if (!workspace) return;
  const { setups, services } = await lazybox.runtime.read(workspace.id);
  const rows = setups.map(setup => {
    const status = [setup.pendingApproval ? 'Needs approval' : `Setup ${setup.state}`];
    if (setup.copied) status.push(`${setup.copied} files copied`);
    if (setup.missing) status.push(`${setup.missing} files missing`);
    const action = workflow.button(setup.pendingApproval ? 'Review setup' : 'Open setup', () =>
      lazybox.runtime.show(workspace.id, 'setup', setup.member));
    return workflow.row('setup:' + setup.member, setup.member, status.join(' · '), action);
  });
  for (const service of services) {
    if (service.state === 'idle') continue;
    const actions = service.hasLog ? [workflow.button('Open logs', () => lazybox.runtime.show(workspace.id, 'logs', service.id))] : [];
    rows.push(workflow.row(service.id, service.name, `${service.member} · ${service.state}`, ...actions));
  }
  workflow.list(rows, 'No setup or service activity in this workspace.');
});
