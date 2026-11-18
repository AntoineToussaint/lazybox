function elapsed(at) {
  if (!at) return '';
  const minutes = Math.max(0, Math.floor((Date.now() - at) / 60000));
  if (!minutes) return 'Less than a minute';
  if (minutes < 60) return `${minutes} min`;
  if (minutes < 1440) return `${Math.floor(minutes / 60)} h`;
  return `${Math.floor(minutes / 1440)} d`;
}

workflow.poll(5000, async () => {
  const { items } = await lazybox.attention.list();
  const blocked = items.filter(item => item.state === 'needs-input' || item.state === 'failed').sort((a, b) => a.at - b.at);
  workflow.list(blocked.map(item => workflow.row(item.sessionId, item.title || 'Conversation',
    [item.workspaceName, item.reason, elapsed(item.at)].filter(Boolean).join(' · '),
    workflow.button('Open conversation', () => lazybox.conversations.show(item.sessionId)))), 'No conversations are blocked on this Runner.');
});
