// Agent board: an example Lazybox extension. It reads what needs you
// (read:attention) every few seconds and shows it as a board; a card shows
// its conversation (show:conversations); the buttons run two actions that
// only move focus (do:attention.next, do:inbox.open).
const columns = [
  { state: 'needs-input', title: 'Needs you' },
  { state: 'failed', title: 'Failed' },
  { state: 'review', title: 'Ready for review' },
  { state: 'working', title: 'Working' },
  { state: 'idle', title: 'Done' },
];
const board = document.getElementById('board');
const summary = document.getElementById('summary');
const notice = document.getElementById('notice');

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function age(at) {
  if (!at || at < 1.5e12) return '';
  const seconds = Math.max(0, Math.round((Date.now() - at) / 1000));
  if (seconds < 60) return 'now';
  if (seconds < 3600) return Math.round(seconds / 60) + ' min';
  if (seconds < 86400) return Math.round(seconds / 3600) + ' h';
  return Math.round(seconds / 86400) + ' d';
}

function say(text) {
  notice.hidden = !text;
  notice.textContent = text || '';
}

function card(item) {
  const button = el('button', 'card');
  button.type = 'button';
  button.dataset.session = item.sessionId;
  const title = el('span', 'title');
  if (item.unread) title.append(el('i', 'unread'));
  title.append(el('span', '', item.title || 'Conversation'));
  const meta = el('span', 'meta');
  meta.append(el('span', '', item.workspaceName || 'No workspace'), el('span', '', '·'), el('span', '', item.reason), el('span', 'age', age(item.at)));
  button.append(title, meta);
  if (item.line) button.append(el('span', 'line', item.line));
  button.title = 'Show ' + (item.title || 'this conversation');
  button.addEventListener('click', () => lazybox.conversations.show(item.sessionId).catch(error => say(error.message)));
  return button;
}

function render(result) {
  const focused = document.activeElement && document.activeElement.closest('.card') ? document.activeElement.dataset.session : '';
  board.replaceChildren(...columns.map(column => {
    const items = result.items.filter(item => item.state === column.state);
    const section = el('section', 'column ' + column.state + (items.length ? '' : ' is-empty'));
    section.setAttribute('aria-label', column.title);
    const heading = el('h2');
    heading.append(el('i', 'dot'), el('span', '', column.title), el('span', 'count', String(items.length)));
    const list = el('ul', 'cards');
    if (!items.length) list.append(el('li', 'empty', 'Nothing here'));
    for (const item of items) { const li = el('li'); li.append(card(item)); list.append(li); }
    section.append(heading, list);
    return section;
  }));
  if (focused) [...board.querySelectorAll('.card')].find(node => node.dataset.session === focused)?.focus();
  const counts = result.counts;
  const parts = [];
  if (counts['needs-input']) parts.push(counts['needs-input'] + ' need you');
  if (counts.failed) parts.push(counts.failed + ' failed');
  if (counts.working) parts.push(counts.working + ' working');
  summary.textContent = parts.join(' · ') || 'Nothing needs you';
  summary.title = summary.textContent;
}

// Arrow keys move between cards: up and down in a column, left and right across.
board.addEventListener('keydown', event => {
  const current = event.target.closest('.card');
  if (!current) return;
  const cols = [...board.querySelectorAll('.column')].map(c => [...c.querySelectorAll('.card')]);
  const col = cols.findIndex(c => c.includes(current)), row = cols[col].indexOf(current);
  let target;
  if (event.key === 'ArrowDown') target = cols[col][row + 1];
  if (event.key === 'ArrowUp') target = cols[col][row - 1];
  if (event.key === 'ArrowRight' || event.key === 'ArrowLeft') {
    const step = event.key === 'ArrowRight' ? 1 : -1;
    for (let c = col + step; c >= 0 && c < cols.length && !target; c += step) target = cols[c][Math.min(row, cols[c].length - 1)];
  }
  if (target) { event.preventDefault(); target.focus(); }
});

document.getElementById('next').addEventListener('click', () => lazybox.run('attention.next').catch(error => say(error.message)));
document.getElementById('inbox').addEventListener('click', () => lazybox.run('inbox.open').catch(error => say(error.message)));

lazybox.every(3000, async () => {
  try { render(await lazybox.attention.list()); say(''); }
  catch (error) { say(error.code === 'not-granted' ? 'This board may not read what needs you; allow it in Settings → Extensions.' : error.message); }
});
