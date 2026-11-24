const find = id => document.getElementById(id);
const delay = find('delay'), when = find('when'), message = find('message');
const button = find('save');
delay.onchange = () => { when.hidden = delay.value !== 'custom'; when.required = !when.hidden; };
async function refresh() {
  const rows = await lazybox.reminders.list();
  find('scheduled').replaceChildren(...rows.map(row => {
    const item = document.createElement('li'), cancel = document.createElement('button');
    cancel.textContent = 'Cancel';
    cancel.onclick = async () => {
      try { await lazybox.reminders.remove(row.id); await refresh(); }
      catch (error) { find('status').textContent = error.message; }
    };
    item.append(row.text + ' · ' + new Date(row.due).toLocaleString() + ' ', cancel);
    return item;
  }));
}
button.onclick = async () => {
  if (!message.reportValidity() || (!when.hidden && !when.reportValidity())) return;
  button.disabled = true;
  try {
    const due = when.hidden ? Date.now() + Number(delay.value) * 60000 : new Date(when.value).getTime();
    await lazybox.reminders.create(message.value, due);
    message.value = '';
    find('status').textContent = 'Reminder set for ' + new Date(due).toLocaleString();
    await refresh();
  } catch (error) { find('status').textContent = error.message; }
  finally { button.disabled = false; }
};
lazybox.every(15000, refresh);
