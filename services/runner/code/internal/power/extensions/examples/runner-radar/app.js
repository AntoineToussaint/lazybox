function elapsed(at) {
  if (!at) return '';
  const minutes = Math.max(0, Math.floor((Date.now() - at) / 60000));
  if (!minutes) return 'Less than a minute';
  if (minutes < 60) return `${minutes} min`;
  if (minutes < 1440) return `${Math.floor(minutes / 60)} h`;
  return `${Math.floor(minutes / 1440)} d`;
}

const stateFilter = document.getElementById('state');
let blocked;

function render() {
  if (!blocked) return;
  const state = stateFilter.value;
  const visible = state === 'all' ? blocked : blocked.filter(item => item.state === state);
  workflow.list(visible.map(item => {
    const failed = item.state === 'failed';
    const action = workflow.button(failed ? 'Inspect failure' : 'Respond', () => lazybox.conversations.show(item.sessionId));
    const age = elapsed(item.at);
    const row = workflow.row(item.sessionId, item.title || 'Conversation',
      [item.workspaceName, age && age + ' ago'].filter(Boolean).join(' · '), action);
    const label = document.createElement('span');
    label.className = 'radar-state';
    label.textContent = item.reason || (failed ? 'Failed' : 'Needs input');
    row.dataset.state = item.state;
    row.querySelector('strong').prepend(label, ' ');
    if (item.line) {
      const preview = document.createElement('span');
      preview.className = 'radar-preview';
      preview.textContent = item.line;
      row.querySelector('small').prepend(preview);
    }
    return row;
  }), state === 'all' ? 'No blocked conversations on this history page.' :
    state === 'failed' ? 'No failed conversations on this history page.' : 'No conversations need input on this history page.');
}

stateFilter.addEventListener('change', render);
const refresh = attentionPages(({ items }) => {
  blocked = items.filter(item => item.state === 'needs-input' || item.state === 'failed')
    .sort((a, b) => (a.at || Infinity) - (b.at || Infinity));
  render();
}, error => workflow.say(error.message));
workflow.poll(5000, refresh);
