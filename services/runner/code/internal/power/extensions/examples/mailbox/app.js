workflow.openInbox();
const labels = { review_requested: 'Review requested', assign: 'Assigned to you', mention: 'Mentioned you',
  team_mention: 'Mentioned your team', author: 'Activity on your item', comment: 'New comment', state_change: 'State changed' };
workflow.poll(120000, async () => {
  const { items, truncated } = await lazybox.mailbox.list();
  workflow.list(items.map(item => workflow.row(item.id, item.title,
    `${item.repository} · ${labels[item.reason] || 'GitHub activity'}`,
    workflow.button('Open on GitHub', () => lazybox.github.open(item.url)))), 'No unread GitHub issue or pull request activity.');
  if (truncated) workflow.say('Showing the newest notifications.');
});
