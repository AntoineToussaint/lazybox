type Item = { id: string; text: string; status: 'todo' | 'done' };

export function todoProgress(content: string) {
  const { items } = JSON.parse(content) as { items: Item[] };
  return { total: items.length, remaining: items.filter(item => item.status === 'todo').length };
}

export function todoMarkdown(content: string) {
  const list = JSON.parse(content) as { items: Item[] };
  return list.items.map(item => `- [${item.status === 'done' ? 'x' : ' '}] ${item.text}`).join('\n');
}

export function TodoList({ content, disabled, onSave }: { content: string; disabled: boolean; onSave(body: string): void }) {
  const list = JSON.parse(content) as { schema: string; items: Item[] };
  return <ol>{list.items.map(item => <li key={item.id}><label>
    <input type="checkbox" checked={item.status === 'done'} disabled={disabled} onChange={event => {
      const status = event.target.checked ? 'done' : 'todo';
      const items = list.items.map(entry => entry.id === item.id ? { ...entry, status } : entry);
      onSave(JSON.stringify({ ...list, items }, null, 2));
    }}/> {item.text}
  </label></li>)}</ol>;
}
