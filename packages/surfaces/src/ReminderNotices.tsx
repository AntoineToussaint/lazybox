import { useEffect, useState } from 'react';
import { sendSystemNotice } from './attention-alerts';
import { attentionWindowFocused, useAttentionHost, useAttentionRoot, useNotificationPreferences } from './useAttention';
import { extensionsChanged } from './useExtensions';
import { useRunner } from './RunnerContext';
import { playAlert } from './sounds';
import { listReminders, remindersChanged, removeReminder, type Reminder } from './reminders';

export function ReminderNotices() {
  const root = useAttentionRoot();
  const host = useAttentionHost();
  const { api } = useRunner();
  const [preferences] = useNotificationPreferences();
  const [items, setItems] = useState<(Reminder & { ownerName: string })[]>([]);
  useEffect(() => {
    let timer: number | undefined;
    let revision = 0;
    const started = Date.now();
    const delivered = new Set<string>();
    async function refresh() {
      const current = ++revision;
      window.clearTimeout(timer);
      let rows: Reminder[];
      try { rows = listReminders(localStorage, root); } catch { rows = []; }
      if (!rows.length) { setItems([]); return; }

      // The extension catalog can change on another device while this window is hidden.
      // Check current grants before delivery and keep records through a connection failure.
      let catalog;
      try { catalog = await api.listExtensions({}, { timeoutMs: 5000 }); }
      catch {
        if (current === revision) { setItems([]); timer = window.setTimeout(refresh, 5000); }
        return;
      }
      if (current !== revision) return;
      const owners = new Map(catalog.extensions.filter(ext => (ext.state === 'enabled' || ext.state === 'review') &&
        ext.approvedGrants.includes('manage:reminders') && ext.grants.includes('manage:reminders')).map(ext => [ext.id, ext.name]));
      const active = rows.filter(row => owners.has(row.owner));
      for (const row of rows) {
        if (!owners.has(row.owner)) removeReminder(localStorage, root, row.owner, row.id, false);
      }
      const now = Date.now();
      const due = active.filter(row => row.due <= now);
      const arriving = due.filter(row => row.due > started && !delivered.has(row.id));
      if (arriving.length && preferences.reminders) {
        if (preferences.soundNeedsYou) playAlert('request');
        if (!attentionWindowFocused()) {
          const first = arriving[0];
          sendSystemNotice(host, { key: 'reminder:' + first.id, title: arriving.length === 1 ? 'Lazybox reminder' : arriving.length + ' Lazybox reminders',
            body: first.text, collectionId: '', sessionId: '' });
        }
      }
      delivered.clear();
      for (const row of due) delivered.add(row.id);
      setItems(due.map(row => ({ ...row, ownerName: owners.get(row.owner) || 'Extension' })));
      if (active.length) {
        const next = active.find(row => row.due > now);
        timer = window.setTimeout(refresh, Math.min(next ? next.due - now : Infinity, 15000));
      }
    }
    refresh();
    function onStorage(event: StorageEvent) {
      if (!event.key || event.key.startsWith('lazybox:reminders:')) refresh();
    }
    window.addEventListener('storage', onStorage);
    const events = ['focus', remindersChanged, extensionsChanged];
    events.forEach(name => window.addEventListener(name, refresh));
    return () => {
      revision++;
      window.removeEventListener('storage', onStorage);
      events.forEach(name => window.removeEventListener(name, refresh));
      window.clearTimeout(timer);
    };
  }, [api, root, host, preferences]);
  return <>{items.map(row =>
    <section className="account-notice" role="status" key={row.id}>
      <div className="account-notice-body"><strong>Reminder</strong><p>{row.text}</p>
        <small>{row.ownerName} · {new Date(row.due).toLocaleString()}</small>
        <div className="account-notice-actions"><button onClick={() => removeReminder(localStorage, root, row.owner, row.id)}>Done</button></div>
      </div>
    </section>)}</>;
}
