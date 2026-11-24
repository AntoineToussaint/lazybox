import { useEffect, useState } from 'react';
import { reportRefusal } from './attention-alerts';
import { useAttentionHost, useAttentionRoot } from './useAttention';
import { listReminders, remindersChanged, removeReminder, type Reminder } from './reminders';

export function ReminderNotices() {
  const root = useAttentionRoot();
  const host = useAttentionHost();
  const [items, setItems] = useState<Reminder[]>([]);
  useEffect(() => {
    let timer: number | undefined;
    let previous: Set<string> | undefined;
    function refresh() {
      window.clearTimeout(timer);
      let rows: Reminder[];
      try { rows = listReminders(localStorage, root); } catch { rows = []; }
      const now = Date.now();
      const due = rows.filter(row => row.due <= now);
      const alreadyDue = previous;
      if (alreadyDue && !document.hasFocus()) {
        const arriving = due.filter(row => !alreadyDue.has(row.id));
        if (arriving.length) {
          const first = arriving[0];
          void host?.notifyAttention?.({ key: 'reminder:' + first.id, title: arriving.length === 1 ? 'Lazybox reminder' : arriving.length + ' Lazybox reminders',
            body: first.text, collectionId: '', sessionId: '' }).catch(reportRefusal);
        }
      }
      previous = new Set(due.map(row => row.id));
      setItems(due);
      const next = rows.find(row => row.due > now);
      if (next) timer = window.setTimeout(refresh, Math.min(next.due - now, 2147483647));
    }
    refresh();
    function onStorage(event: StorageEvent) {
      if (!event.key || event.key.startsWith('lazybox:reminders:')) refresh();
    }
    window.addEventListener('storage', onStorage);
    const events = ['focus', remindersChanged];
    events.forEach(name => window.addEventListener(name, refresh));
    return () => {
      window.removeEventListener('storage', onStorage);
      events.forEach(name => window.removeEventListener(name, refresh));
      window.clearTimeout(timer);
    };
  }, [root, host]);
  return <>{items.map(row =>
    <section className="account-notice" role="status" key={row.id}>
      <div className="account-notice-body"><strong>Reminder</strong><p>{row.text}</p>
        <small>{row.owner} · {new Date(row.due).toLocaleString()}</small>
        <div className="account-notice-actions"><button onClick={() => removeReminder(localStorage, root, row.owner, row.id)}>Done</button></div>
      </div>
    </section>)}</>;
}
