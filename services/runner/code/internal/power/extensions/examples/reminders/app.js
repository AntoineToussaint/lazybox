const find = id => document.getElementById(id);
const delay = find('delay'), when = find('when'), message = find('message');
const button = find('save');
delay.onchange = () => { when.hidden = delay.value !== 'custom'; when.required = !when.hidden; };
function scheduledTime(now) {
  if (delay.value === 'custom') return new Date(when.value).getTime();
  if (delay.value === 'tomorrow' || delay.value === 'monday') {
    const days = delay.value === 'tomorrow' ? 1 : (8 - now.getDay()) % 7 || 7;
    const morning = new Date(now);
    morning.setDate(morning.getDate() + days);
    morning.setHours(9, 0, 0, 0);
    return morning.getTime();
  }
  return now.getTime() + Number(delay.value) * 60000;
}
async function refresh() {
  const rows = await lazybox.reminders.list();
  find('empty').hidden = rows.length > 0;
  find('scheduled').replaceChildren(...rows.map(row => {
    const item = document.createElement('li'), cancel = document.createElement('button');
    cancel.textContent = 'Cancel';
    cancel.onclick = async () => {
      try { await lazybox.reminders.remove(row.id); await refresh(); }
      catch (error) { find('status').textContent = error.message; }
    };
    const title = document.createElement('strong'), detail = document.createElement('small');
    const time = document.createElement('time');
    title.textContent = row.text;
    const date = new Date(row.due);
    time.dateTime = date.toISOString();
    time.textContent = date.toLocaleString(undefined, { weekday: 'short', month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
    detail.append(time);
    item.append(title, detail, cancel);
    return item;
  }));
}
button.onclick = async () => {
  if (!message.reportValidity() || (!when.hidden && !when.reportValidity())) return;
  button.disabled = true;
  try {
    const due = scheduledTime(new Date());
    await lazybox.reminders.create(message.value, due);
    message.value = '';
    find('status').textContent = 'Reminder set for ' + new Date(due).toLocaleString();
    await refresh();
  } catch (error) { find('status').textContent = error.message; }
  finally { button.disabled = false; }
};
message.addEventListener('keydown', event => {
  if (event.key === 'Enter' && !event.isComposing) {
    event.preventDefault();
    button.click();
  }
});
lazybox.every(15000, refresh);
