workflow.poll(5000, async () => {
  const workspace = await workflow.workspace();
  if (!workspace) return;
  const { services } = await lazybox.runtime.read(workspace.id);
  workflow.list(services.map(service => {
    const actions = [];
    if (service.preview) actions.push(workflow.button('Open preview', () => lazybox.runtime.show(workspace.id, 'preview', service.id)));
    if (service.hasLog) actions.push(workflow.button('Open logs', () => lazybox.runtime.show(workspace.id, 'logs', service.id)));
    const status = [service.member, service.state, service.port ? `port ${service.port}` : ''].filter(Boolean);
    return workflow.row(service.id, service.name, status.join(' · '), ...actions);
  }), 'No services are configured in this workspace.');
});
