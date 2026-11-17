/// <reference path="lazybox.d.ts" />
// Your extension's script. `lazybox` is the bridge (lazybox.d.ts lists every
// call and the grant it needs); a call outside your grants is refused with a
// LazyboxError whose code is "not-granted".
const items = document.getElementById('items');
const status = document.getElementById('status');

lazybox.every(5000, async () => {
  try {
    const { items: rows } = await lazybox.attention.list();
    const waiting = rows.filter(row => row.state === 'needs-input');
    status.textContent = waiting.length ? `${waiting.length} need you` : 'Nothing needs you';
    items.replaceChildren(...waiting.map(row => {
      const li = document.createElement('li');
      li.textContent = `${row.title} — ${row.reason}`;
      return li;
    }));
  } catch (error) {
    status.textContent = error.message;
  }
});
