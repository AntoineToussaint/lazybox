
type Reaction = { phase: 'sending' | 'sent' | 'error' | 'manual' | 'dismissed'; text: string; at: number; hidden?: boolean };
const prefix = 'lazybox:pull-reaction:';
const receiptDuration = 10_000;
// The longest reaction RPC times out after 120 seconds.
const pendingDuration = 125_000;
const records = new Map<string, Reaction>();
const listeners = new Set<() => void>();
let timer: ReturnType<typeof setTimeout>;
function decode(raw: string | null): Reaction | undefined {
  try {
    const v = JSON.parse(raw || 'null');
    if (v && ['sending','sent','error','manual','dismissed'].includes(v.phase) && Number.isFinite(v.at) && typeof v.text === 'string') return v;
  } catch { /* Ignore invalid saved state. */ }
}
function restore() {
  try {
    for (const key of Object.keys(localStorage)) {
      if (!key.startsWith(prefix)) continue;
      const record = decode(localStorage.getItem(key));
      if (record) records.set(key.slice(prefix.length), record);
    }
  } catch { /* This window can still track actions in memory. */ }
}
restore();
function build() {
  const now = Date.now(), dismissed = new Set<string>(), completed = new Map<string, {text: string; until: number}>();
  const states = new Map(records);
  let next = Infinity;
  for (const [key, record] of records) {
    if (record.hidden || record.phase === 'sent' || record.phase === 'dismissed') dismissed.add(key);
    if (record.phase !== 'sent' && record.phase !== 'sending') continue;
    const until = record.at + (record.phase === 'sent' ? receiptDuration : pendingDuration);
    if (until > now) {
      next = Math.min(next, until);
      if (record.phase === 'sent' && !record.hidden) completed.set(key, { text: record.text, until });
    } else if (record.phase === 'sending') states.set(key, { ...record, phase: 'error', text: 'Delivery was not confirmed. Check the conversation before retrying.' });
  }
  clearTimeout(timer);
  if (Number.isFinite(next)) timer = setTimeout(publish, next - now);
  return { dismissed, completed, states };
}
let snapshot = build();
function publish() { snapshot = build(); listeners.forEach(listener => listener()); }
function write(key: string, value: Reaction) {
  records.set(key, value);
  try {
    localStorage.setItem(prefix + key, JSON.stringify(value));
    if (records.size > 500) {
      const oldest = [...records].filter(([, r]) => r.phase !== 'sending' || r.at + pendingDuration <= Date.now()).sort((a,b) => a[1].at - b[1].at);
      for (const [old] of oldest.slice(0, records.size - 500)) { records.delete(old); localStorage.removeItem(prefix + old); }
    }
  } catch { /* Keep the in-memory result. */ }
  publish();
}
if (typeof window !== 'undefined') window.addEventListener('storage', event => {
  if (event.key === null) { records.clear(); restore(); publish(); return; }
  if (!event.key.startsWith(prefix)) return;
  const key = event.key.slice(prefix.length);
  const record = decode(localStorage.getItem(event.key));
  if (record) records.set(key, record); else records.delete(key);
  publish();
});
export const pullReactions = {
  snapshot: () => snapshot,
  subscribe(listener: () => void) { listeners.add(listener); return () => { listeners.delete(listener); }; },
  dismiss(key: string) {
    const record = records.get(key);
    write(key, record ? { ...record, hidden: true } : { phase: 'dismissed', text: '', at: Date.now(), hidden: true });
  },
  async run(key: string, work: () => Promise<{ok: boolean; text: string; manual?: boolean}>) {
    const execute = async () => {
      // Read under the lock; another window's storage event can arrive later.
      try { const saved = decode(localStorage.getItem(prefix + key)); if (saved) records.set(key, saved); } catch { /* in memory */ }
      const previous = records.get(key);
      if (previous?.phase === 'sent') return {ok: true, text: previous.text};
      if (previous?.phase === 'sending' && previous.at + pendingDuration > Date.now()) return {ok: false, text: 'Already sending…'};
      write(key, {phase: 'sending', text: 'Working…', at: Date.now(), hidden: previous?.hidden});
      const result = await work();
      let latest = records.get(key);
      try { latest = decode(localStorage.getItem(prefix + key)) ?? latest; } catch { /* In-memory state. */ }
      write(key, {phase: result.manual ? 'manual' : result.ok ? 'sent' : 'error', text: result.text, at: Date.now(), hidden: latest?.hidden});
      return result;
    };
    if (!navigator.locks) return execute();
    return navigator.locks.request(prefix + key, { ifAvailable: true }, lock => {
      if (lock) return execute();
      return { ok: false, text: 'Already sending…' };
    });
  },
};
