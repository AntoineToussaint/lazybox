export type Reminder = { id: string; owner: string; text: string; due: number };
export const remindersChanged = 'lazybox-reminders-changed';
const prefix = (root: string) => 'lazybox:reminders:' + encodeURIComponent(root) + ':';
const key = (root: string, owner: string, id: string) => prefix(root) + encodeURIComponent(owner) + ':' + encodeURIComponent(id);

export function listReminders(storage: Storage, root: string, owner?: string): Reminder[] {
  const rows: Reminder[] = [];
  const start = prefix(root) + (owner === undefined ? '' : encodeURIComponent(owner) + ':');
  for (let i = 0; i < storage.length; i++) {
    const name = storage.key(i);
    if (!name?.startsWith(start)) continue;
    try {
      const row = JSON.parse(storage.getItem(name) || 'null');
      if (row && typeof row.id === 'string' && typeof row.owner === 'string' && typeof row.text === 'string' &&
        Number.isFinite(row.due) && key(root, row.owner, row.id) === name) rows.push(row);
    } catch { /* Ignore a damaged record. Other reminders still work. */ }
  }
  return rows.sort((a, b) => a.due - b.due);
}

export function createReminder(storage: Storage, root: string, owner: string, text: string, due: unknown) {
  if (!text.trim() || text.length > 240) throw new Error('Enter a reminder of up to 240 characters.');
  if (typeof due !== 'number' || !Number.isSafeInteger(due) || due <= Date.now() || due > 8640000000000000)
    throw new Error('Choose a future time.');
  if (listReminders(storage, root, owner).length >= 100) throw new Error('Remove a reminder before adding another.');
  const id = crypto.randomUUID();
  storage.setItem(key(root, owner, id), JSON.stringify({ id, owner, text: text.trim(), due }));
  window.dispatchEvent(new Event(remindersChanged));
  return id;
}

export function removeReminder(storage: Storage, root: string, owner: string, id: string) {
  storage.removeItem(key(root, owner, id));
  window.dispatchEvent(new Event(remindersChanged));
}
