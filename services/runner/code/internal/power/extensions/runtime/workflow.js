// Shared rendering for Lazybox's bundled workflow panels.
const workflow = (() => {
  const scope = document.getElementById('scope');
  const notice = document.getElementById('notice');
  const rows = document.getElementById('rows');
  const header = scope.closest('header');
  const filter = document.createElement('input');
  filter.type = 'search';
  filter.placeholder = 'Filter list…';
  filter.setAttribute('aria-label', 'Filter list');
  const empty = document.createElement('p');
  empty.className = 'filter-empty';
  empty.textContent = 'No matching items.';
  empty.hidden = true;
  rows.before(filter);
  rows.after(empty);
  const applyFilter = () => {
    const query = filter.value.trim().toLowerCase();
    const items = [...rows.children];
    for (const item of items) {
      const text = [item.querySelector('strong')?.textContent, item.querySelector('small')?.textContent].join(' ');
      item.hidden = !text.toLowerCase().includes(query);
    }
    filter.hidden = items.length < 2 && !filter.value && document.activeElement !== filter;
    empty.hidden = !items.length || items.some(item => !item.hidden);
  };
  applyFilter();
  filter.addEventListener('input', applyFilter);
  if (scope.textContent === 'Runner') {
    lazybox.ready.then(hello => { if (hello.runnerLabel) scope.textContent = `Runner · ${hello.runnerLabel}`; }).catch(() => {});
  }

  const say = text => {
    notice.textContent = text;
    notice.hidden = !text;
  };
  const button = (label, run) => {
    const node = document.createElement('button');
    node.type = 'button';
    node.textContent = label;
    node.onclick = async () => {
      node.disabled = true;
      try { await run(); } catch (error) { say(error.message); }
      finally { node.disabled = false; }
    };
    return node;
  };
  const row = (key, label, detail, ...actions) => {
    const li = document.createElement('li');
    li.dataset.key = key;
    const name = document.createElement('strong');
    name.textContent = label;
    const subtitle = document.createElement('small');
    subtitle.textContent = detail;
    li.append(name, subtitle);
    if (actions.length) {
      const controls = document.createElement('div');
      controls.append(...actions);
      li.append(controls);
    }
    return li;
  };
  const list = (items, message) => {
    const focused = document.activeElement?.closest('li[data-key]');
    const key = focused?.dataset.key, action = focused ? document.activeElement.textContent : '';
    rows.replaceChildren(...items);
    applyFilter();
    if (key) [...items.find(item => item.dataset.key === key)?.querySelectorAll('button') || []].find(node => node.textContent === action)?.focus();
    say(items.length ? '' : message);
  };
  const workspace = async () => {
    const { workspace: value } = await lazybox.workspaces.focused();
    scope.textContent = value ? `Workspace · ${value.name}` : 'No workspace selected';
    if (!value) list([], 'Select a workspace to continue.');
    return value;
  };
  const loaders = [];
  const refresh = button('Refresh', () => Promise.all(loaders.map(load => load())));
  header.append(refresh);
  const poll = (ms, load) => {
    let pending;
    const update = () => {
      if (!pending) pending = Promise.resolve().then(load).catch(error => say(error.message)).finally(() => { pending = undefined; });
      return pending;
    };
    loaders.push(update);
    return lazybox.every(ms, update);
  };
  const openInbox = () => {
    const trigger = document.getElementById('inbox');
    trigger.addEventListener('click', () => lazybox.run('inbox.open').catch(error => say(error.message)));
  };
  return { list, row, button, say, workspace, poll, openInbox };
})();
