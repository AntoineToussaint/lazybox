workflow.openInbox();
const labels = {
  review_requested: 'Review requested', assign: 'Assigned to you', mention: 'Mentioned you',
  team_mention: 'Mentioned your team', author: 'Activity on your item', comment: 'New comment', state_change: 'State changed',
};
const directReasons = new Set(['review_requested', 'assign', 'mention', 'team_mention']);
const all = document.getElementById('all'), direct = document.getElementById('direct');
const dateFormat = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
let result, directOnly = false;

function render() {
  all.setAttribute('aria-pressed', String(!directOnly));
  direct.setAttribute('aria-pressed', String(directOnly));
  if (!result) return;
  const requested = result.items.filter(item => directReasons.has(item.reason));
  all.querySelector('span').textContent = result.items.length;
  direct.querySelector('span').textContent = requested.length;
  const visible = directOnly ? requested : result.items;
  workflow.list(visible.map(item => {
    const updated = Date.parse(item.updatedAt);
    const detail = [item.repository, labels[item.reason] || 'GitHub activity'];
    if (Number.isFinite(updated)) detail.push(dateFormat.format(updated));
    return workflow.row(item.id, item.title, detail.join(' · '),
      workflow.button('Open on GitHub', () => lazybox.github.open(item.url)));
  }), directOnly ? 'No unread review requests, assignments or mentions.' : 'No unread GitHub issue or pull request activity.');
  if (result.truncated) workflow.say('Showing the newest notifications.' + (directOnly && !visible.length ? ' None are requests or mentions.' : ''));
}

all.onclick = () => { directOnly = false; render(); };
direct.onclick = () => { directOnly = true; render(); };
workflow.poll(120000, async () => { result = await lazybox.mailbox.list(); render(); });
